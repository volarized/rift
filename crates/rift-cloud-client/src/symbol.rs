use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner},
    read::{GetSymbolInclude, PAGE_LIMIT_MAX},
    symbol_read::{GetSymbolParams, GetSymbolResult},
};

use crate::{
    CURSOR_BYTES_MAX, ClientError, GlobalClient, SYMBOL_DOCUMENTATION_FEATURE,
    active_response_body_bytes_max, contract, response, serialize_body, smaller_bound,
    supports_feature, validate_body_for_capabilities,
};

impl GlobalClient {
    /// Reads one canonical package or runtime symbol in its exact release and serving view.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, or the endpoint
    /// is unavailable.
    pub async fn get_symbol(
        &self,
        request: &GetSymbolParams,
    ) -> Result<GetSymbolResult, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_request(request)?;
        let capabilities = self.get_capabilities().await?;
        if request.include.contains(&GetSymbolInclude::Documentation)
            && !supports_feature(&capabilities, SYMBOL_DOCUMENTATION_FEATURE)
        {
            return Err(ClientError::FeatureUnavailable {
                feature: SYMBOL_DOCUMENTATION_FEATURE,
            });
        }
        if request.declaration_cursor.as_ref().is_some_and(|cursor| {
            cursor.len() > smaller_bound(capabilities.bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
        }) {
            return Err(ClientError::InvalidRequest {
                field: "declaration_cursor",
            });
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
                .observed(Err(ClientError::InvalidResponse {
                    status: meta.status,
                }))
                .await;
        }
        Ok(value)
    }
}

fn validate_request(request: &GetSymbolParams) -> Result<(), ClientError> {
    let identity = SymbolIdentity::parse(request.id.as_str())
        .map_err(|_| ClientError::InvalidRequest { field: "id" })?;
    if !matches!(
        identity.owner(),
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    ) {
        return Err(ClientError::InvalidRequest { field: "id" });
    }
    if request.rev.is_some() {
        return Err(ClientError::InvalidRequest { field: "rev" });
    }
    if !(1..=PAGE_LIMIT_MAX).contains(&request.declaration_limit) {
        return Err(ClientError::InvalidRequest {
            field: "declaration_limit",
        });
    }
    if request.declaration_cursor.as_ref().is_some_and(|cursor| {
        cursor.is_empty() || cursor.len() > CURSOR_BYTES_MAX || request.view.is_none()
    }) {
        return Err(ClientError::InvalidRequest {
            field: "declaration_cursor",
        });
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
    match result {
        GetSymbolResult::Found {
            view,
            declarations,
            history,
            documentation,
            ..
        } => {
            request.view.as_ref().is_none_or(|id| id == &view.id)
                && u64::try_from(declarations.len())
                    .is_ok_and(|length| length <= request.declaration_limit)
                && declarations.iter().all(|declaration| {
                    declaration.source.as_ref().is_none_or(|source| {
                        request.include.contains(&GetSymbolInclude::Source)
                            && source.len() <= source_bytes_max
                    })
                })
                && (history.is_none() || request.include.contains(&GetSymbolInclude::History))
                && (documentation.is_none()
                    || request.include.contains(&GetSymbolInclude::Documentation))
        }
        GetSymbolResult::Missing { view, .. } => {
            request.view.as_ref().is_none_or(|id| id == &view.id)
        }
        GetSymbolResult::Unavailable { view, .. } => view
            .as_ref()
            .is_none_or(|view| request.view.as_ref().is_none_or(|id| id == &view.id)),
    }
}

#[cfg(test)]
mod tests;
