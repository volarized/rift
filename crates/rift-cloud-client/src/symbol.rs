use rift_error::RiftError;

use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    read::{
        GetSymbolInclude, PAGE_LIMIT_MAX, PackageIdentity, SourceKind, SourceLocationKind,
        SymbolOrigin,
    },
    symbol_read::{GetSymbolParams, GetSymbolResult},
};

use crate::{
    CURSOR_BYTES_MAX, GlobalClient, SYMBOL_DOCUMENTATION_FEATURE, active_response_body_bytes_max,
    contract, failure, response, serialize_body, smaller_bound, supports_feature,
    validate_body_for_capabilities,
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
        rift_tracing::traced!(component = "global", operation = "get_symbol", async {
            if !self.inner.enabled {
                return Err(failure::configuration_invalid(
                    "enabled",
                    "global client disabled",
                ));
            }
            validate_request(request)?;
            let capabilities = self.get_capabilities().await?;
            if request.include.contains(&GetSymbolInclude::Documentation)
                && !supports_feature(&capabilities, SYMBOL_DOCUMENTATION_FEATURE)
            {
                return Err(failure::configuration_invalid(
                    "feature",
                    &format!(
                        "global API does not advertise feature: {SYMBOL_DOCUMENTATION_FEATURE}"
                    ),
                ));
            }
            if request.declaration_cursor.as_ref().is_some_and(|cursor| {
                cursor.len() > smaller_bound(capabilities.bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
            }) {
                return Err(failure::request_invalid(
                    "get_symbol",
                    "declaration_cursor",
                    "field failed validation",
                ));
            }
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
    if !(1..=PAGE_LIMIT_MAX).contains(&request.declaration_limit) {
        return Err(failure::request_invalid(
            "validate_request",
            "declaration_limit",
            "field failed validation",
        ));
    }
    if request.declaration_cursor.as_ref().is_some_and(|cursor| {
        cursor.is_empty() || cursor.len() > CURSOR_BYTES_MAX || request.view.is_none()
    }) {
        return Err(failure::request_invalid(
            "validate_request",
            "declaration_cursor",
            "field failed validation",
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
    match result {
        GetSymbolResult::Found {
            symbol,
            view,
            declarations,
            history,
            documentation,
            ..
        } => {
            request.view.as_ref().is_none_or(|id| id == &view.id)
                && u64::try_from(declarations.len())
                    .is_ok_and(|length| length <= request.declaration_limit)
                && (valid_origin(&symbol.origin, owner)
                    || (declarations.is_empty()
                        && symbol.origin.source_kind == SourceKind::Synthetic
                        && symbol.origin.location.is_none()
                        && symbol.origin.package.is_none()
                        && symbol.origin.runtime.is_none()))
                && declarations.iter().all(|declaration| {
                    declaration.unit.as_ref().is_some_and(|unit| {
                        rift_core::SourceUnitId::parse(&unit.0)
                            .is_ok_and(|unit| unit.source_owner() == Some(owner))
                    }) && valid_origin(&declaration.origin, owner)
                })
                && (history.is_none() || request.include.contains(&GetSymbolInclude::History))
                && documentation.as_ref().is_none_or(|context| {
                    request.include.contains(&GetSymbolInclude::Documentation)
                        && valid_documentation(context, &request.id, owner, source_bytes_max)
                })
        }
        GetSymbolResult::Missing { view, .. } => {
            request.view.as_ref().is_none_or(|id| id == &view.id)
        }
        GetSymbolResult::Unavailable { view, .. } => view
            .as_ref()
            .is_none_or(|view| request.view.as_ref().is_none_or(|id| id == &view.id)),
    }
}

fn valid_origin(origin: &SymbolOrigin, owner: &SymbolOwner) -> bool {
    if origin.source_kind == SourceKind::Synthetic {
        return false;
    }
    match owner {
        SymbolOwner::Package { .. } => {
            origin.location == Some(SourceLocationKind::Dependency)
                && origin.runtime.is_none()
                && origin
                    .package
                    .as_ref()
                    .is_some_and(|package| package.owner().is_ok_and(|actual| &actual == owner))
        }
        SymbolOwner::Runtime { .. } => {
            origin.location == Some(SourceLocationKind::Stdlib)
                && origin.package.is_none()
                && origin
                    .runtime
                    .as_ref()
                    .is_some_and(|runtime| runtime.owner().is_ok_and(|actual| &actual == owner))
        }
        _ => false,
    }
}

fn valid_documentation(
    context: &rift_protocol::documentation::DocumentationContext,
    target: &rift_protocol::read::SymbolId,
    owner: &SymbolOwner,
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
        SymbolOwner::Package {
            manager,
            registry,
            name,
            version,
        } => {
            let package = PackageIdentity {
                manager: manager.clone(),
                registry: registry.clone(),
                name: name.clone(),
                version: version.clone(),
            };
            crate::domain::validate_documentation_context(context, target, &package).is_ok()
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
            context
                .references
                .iter()
                .all(|reference| valid_identity(&reference.documentation.source.identity))
                && context
                    .warnings
                    .iter()
                    .all(|warning| valid_identity(&warning.source))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
