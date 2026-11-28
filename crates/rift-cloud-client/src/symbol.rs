use rift_core::SourceUnitId;
use rift_error::RiftError;

use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    read::{Language, SourceKind, SourceLocationKind, SymbolOrigin},
    symbol_read::{GetSymbolParams, GetSymbolResult},
};

use crate::{
    GlobalClient, SYMBOL_DOCUMENTATION_FEATURE, active_response_body_bytes_max, contract, failure,
    response, serialize_body, smaller_bound, supports_feature, validate_body_for_capabilities,
};

impl GlobalClient {
    /// Reads one canonical package or runtime symbol in its exact release and serving view.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, or the endpoint
    /// is unavailable.
    pub async fn get_symbol(
        &self,
        request: &GetSymbolParams,
    ) -> Result<GetSymbolResult, RiftError> {
        self.read_symbol(request, true).await
    }

    /// Reads the symbol with its requested documentation when the API advertises it.
    ///
    /// # Errors
    /// Returns the same request, response, and transport failures as [`Self::get_symbol`].
    pub async fn get_symbol_with_available_documentation(
        &self,
        request: &GetSymbolParams,
    ) -> Result<GetSymbolResult, RiftError> {
        self.read_symbol(request, false).await
    }

    async fn read_symbol(
        &self,
        request: &GetSymbolParams,
        require_documentation: bool,
    ) -> Result<GetSymbolResult, RiftError> {
        rift_tracing::traced!(component = "global", operation = "get_symbol", async {
            if !self.inner.enabled {
                return Err(failure::configuration_invalid(
                    "enabled",
                    "global client disabled",
                ));
            }
            validate_request(request)?;
            let capabilities = self.get_capabilities().await?;
            let documentation_unavailable = request.include.documentation
                && !supports_feature(&capabilities, SYMBOL_DOCUMENTATION_FEATURE);
            if documentation_unavailable && require_documentation {
                return Err(failure::configuration_invalid(
                    "feature",
                    &format!(
                        "global API does not advertise feature: {SYMBOL_DOCUMENTATION_FEATURE}"
                    ),
                ));
            }
            let mut available;
            let request = if documentation_unavailable {
                rift_tracing::warn!(
                    component = "global",
                    operation = "read.symbol.documentation",
                    feature = SYMBOL_DOCUMENTATION_FEATURE,
                    "the global API does not advertise symbol documentation"
                );
                available = request.clone();
                available.include.documentation = false;
                &available
            } else {
                request
            };
            let body = serialize_body(request, self.inner.config.max_request)?;
            validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
            let raw = match self
                .request(
                    contract::Endpoint::Symbols,
                    Some(body),
                    None,
                    active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
                )
                .await
            {
                Ok(raw) => raw,
                Err(error) => return self.observed(Err(error)).await,
            };
            let response::Parsed { value, meta } = match response::exact_symbol(raw) {
                Ok(parsed) => parsed,
                Err(error) => return self.observed(Err(error)).await,
            };
            let source_bytes_max = smaller_bound(
                capabilities.bounds.source_bytes_max,
                self.inner.config.max_source,
            );
            if !valid_result(request, &value, source_bytes_max) {
                return self
                    .observed(Err(failure::response_invalid(
                        "get_symbol",
                        "body",
                        "field failed validation",
                        Some(meta.status),
                    )))
                    .await;
            }
            Ok(value)
        })
        .await
    }
}

fn validate_request(request: &GetSymbolParams) -> Result<(), RiftError> {
    let identity = SymbolIdentity::parse(request.id.as_str())?;
    if !matches!(
        identity.owner(),
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    ) {
        return Err(failure::request_invalid(
            "validate_request",
            "id",
            "field failed validation",
        ));
    }
    if request
        .revision
        .as_ref()
        .is_some_and(|revision| !revision.answers_for(identity.owner()))
    {
        return Err(failure::request_invalid(
            "validate_request",
            "revision",
            "field failed validation",
        ));
    }
    if !(1..=rift_protocol::symbol_read::GET_SYMBOL_MAX_RESULTS).contains(&request.max_results) {
        return Err(failure::request_invalid(
            "validate_request",
            "max_results",
            "field failed validation",
        ));
    }
    if request.page == 0 {
        return Err(failure::request_invalid(
            "validate_request",
            "page",
            "expected a 1-based page",
        ));
    }
    Ok(())
}

fn valid_result(
    request: &GetSymbolParams,
    result: &GetSymbolResult,
    source_bytes_max: usize,
) -> bool {
    if !result.is_valid_for(&request.id) {
        return false;
    }
    let Ok(identity) = SymbolIdentity::parse(request.id.as_str()) else {
        return false;
    };
    let owner = identity.owner();
    let answered = result.revision();
    let other_release = answered
        .zip(request.revision.as_ref())
        .is_some_and(|(answered, requested)| answered != requested);
    if other_release {
        return false;
    }
    let version = answered.and_then(|revision| revision.release_version());
    match result {
        GetSymbolResult::Found {
            symbol,
            declarations,
            pagination,
            history,
            documentation,
            ..
        } => {
            super::validate_pagination(
                pagination,
                request.max_results,
                request.page,
                declarations.len(),
            )
            .is_ok()
                && u64::try_from(declarations.len())
                    .is_ok_and(|length| length <= request.max_results)
                && (valid_origin(&symbol.origin, owner, version)
                    || (declarations.is_empty()
                        && symbol.origin.source_kind == SourceKind::Synthetic
                        && symbol.origin.location.is_none()
                        && symbol.origin.package.is_none()
                        && symbol.origin.runtime.is_none()))
                && declarations.iter().all(|declaration| {
                    declaration.unit.as_ref().is_some_and(|unit| {
                        SourceUnitId::parse(unit.as_str()).is_ok_and(|unit| {
                            (unit.source_owner() == Some(owner)
                                && valid_origin(&declaration.origin, owner, version))
                                || (typeshed_stdlib_unit(&unit, owner, identity.language())
                                    && declaration.origin.location
                                        == Some(SourceLocationKind::External)
                                    && declaration.origin.source_kind == SourceKind::Authored
                                    && declaration.origin.package.is_none()
                                    && declaration.origin.runtime.is_none())
                        })
                    })
                })
                && history.is_some() == request.include.history
                && documentation.as_ref().is_none_or(|context| {
                    request.include.documentation
                        && valid_documentation(
                            context,
                            &request.id,
                            (owner, &symbol.origin),
                            source_bytes_max,
                        )
                })
        }
        GetSymbolResult::Missing { .. } | GetSymbolResult::Unavailable { .. } => true,
    }
}

/// A pinned Typeshed standard-library stub that can declare a Python runtime symbol.
pub(crate) fn typeshed_stdlib_unit(
    unit: &SourceUnitId,
    owner: &SymbolOwner,
    language: &Language,
) -> bool {
    if !matches!(owner, SymbolOwner::Runtime { runtime } if runtime == "cpython")
        || language.name != "python"
        || language.dialect.is_some()
        || unit.resolver().as_str() != "external"
    {
        return false;
    }
    let Some((commit, stub)) = unit.key().as_str().split_once('/') else {
        return false;
    };
    commit.strip_prefix("typeshed-").is_some_and(|pin| {
        pin.len() == 40
            && pin
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }) && stub.strip_prefix("stdlib/").is_some_and(|path| {
        path.rsplit('/')
            .next()
            .and_then(|file| file.strip_suffix(".pyi"))
            .is_some_and(|name| !name.is_empty())
    })
}

/// Whether `origin` names `owner` at the answered release `version`.
pub(crate) fn valid_origin(
    origin: &SymbolOrigin,
    owner: &SymbolOwner,
    version: Option<&str>,
) -> bool {
    if origin.source_kind == SourceKind::Synthetic {
        return false;
    }
    match owner {
        SymbolOwner::Package { .. } => {
            origin.location == Some(SourceLocationKind::Dependency)
                && origin.runtime.is_none()
                && origin.package.as_ref().is_some_and(|package| {
                    package.owner().is_ok_and(|actual| &actual == owner)
                        && version == Some(package.version.as_str())
                })
        }
        SymbolOwner::Runtime { .. } => {
            origin.location == Some(SourceLocationKind::Stdlib)
                && origin.package.is_none()
                && origin.runtime.as_ref().is_some_and(|runtime| {
                    runtime.owner().is_ok_and(|actual| &actual == owner)
                        && version == Some(runtime.version.as_str())
                })
        }
        _ => false,
    }
}

/// Whether documentation answered for `target` stays inside its `owner` at the release of
/// the answered symbol's `origin`.
fn valid_documentation(
    context: &rift_protocol::documentation::DocumentationContext,
    target: &rift_protocol::read::SymbolId,
    (owner, origin): (&SymbolOwner, &SymbolOrigin),
    source_bytes_max: usize,
) -> bool {
    if context
        .references
        .iter()
        .try_fold(0_usize, |bytes, reference| {
            bytes
                .checked_add(reference.excerpt.as_ref().map_or(0, String::len))
                .filter(|total| *total <= source_bytes_max)
        })
        .is_none()
    {
        return false;
    }
    match owner {
        SymbolOwner::Package { .. } => {
            let Some(package) = origin
                .package
                .as_ref()
                .filter(|package| package.owner().ok().as_ref() == Some(owner))
            else {
                return false;
            };
            crate::domain::validate_documentation_context(context, target, package).is_ok()
        }
        SymbolOwner::Runtime { .. } => {
            if rift_analysis::documentation::validate_documentation_context(context, target)
                .is_err()
            {
                return false;
            }
            let valid_identity =
                |identity: &rift_protocol::documentation::DocumentationContentIdentity| {
                    let rift_protocol::documentation::DocumentationSourceIdentity::Package { unit } =
                        &identity.source
                    else {
                        return false;
                    };
                    rift_core::SourceUnitId::parse(&unit.0)
                        .is_ok_and(|unit| unit.source_owner() == Some(owner))
                };
            context.references.iter().all(|reference| {
                let source = &reference.documentation.source;
                valid_identity(&source.identity) && source.origin.runtime == origin.runtime
            }) && context
                .warnings
                .iter()
                .all(|warning| valid_identity(&warning.source))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
