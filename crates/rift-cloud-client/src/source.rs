use std::collections::HashSet;

use rift_protocol::{
    identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity},
    source_read::{
        DeclarationPositionResult, FindDeclarationsParams, FindDeclarationsResult, GetSourceParams,
        GetSourceResult,
    },
};

use crate::{
    ClientError, GlobalClient, PACKAGES_MAX, active_response_body_bytes_max, contract, response,
    serialize_body, smaller_bound, supports_feature, validate_body_for_capabilities,
};

impl GlobalClient {
    /// Reads one physical source unit from its exact release or admitted captured view.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, or the endpoint
    /// is unavailable.
    pub async fn get_source(
        &self,
        request: &GetSourceParams,
    ) -> Result<GetSourceResult, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        if !request.is_valid() {
            return Err(ClientError::InvalidRequest { field: "unit" });
        }
        global_source_owner(request.unit.as_str(), request.view.as_ref())?;
        let capabilities = self.get_capabilities().await?;
        let body = serialize_body(request, self.inner.config.max_request)?;
        validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
        let raw = self
            .request(
                contract::Endpoint::Source,
                Some(body),
                None,
                active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
            )
            .await;
        let raw = self.observed(raw).await?;
        let parsed = self.observed(response::exact_source(raw)).await?;
        let bounded_source = match &parsed.value {
            GetSourceResult::Found { source, .. } => {
                source.text.len()
                    <= smaller_bound(
                        capabilities.bounds.source_bytes_max,
                        self.inner.config.max_source,
                    )
            }
            _ => true,
        };
        if !bounded_source || !parsed.value.is_valid_for(request) {
            return self
                .observed(Err(ClientError::InvalidResponse {
                    status: parsed.meta.status,
                }))
                .await;
        }
        Ok(parsed.value)
    }

    /// Names declarations holding source positions in one immutable selection.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, declaration lookup
    /// is unavailable, or the endpoint is unavailable.
    pub async fn find_declarations(
        &self,
        request: &FindDeclarationsParams,
    ) -> Result<FindDeclarationsResult, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        if !request.is_valid() {
            return Err(ClientError::InvalidRequest { field: "positions" });
        }
        let mut owners = HashSet::new();
        for position in &request.positions {
            if let Some(owner) = global_source_owner(position.unit.as_str(), request.view.as_ref())?
            {
                owners.insert(owner);
            }
        }
        let capabilities = self.get_capabilities().await?;
        if !supports_feature(&capabilities, "declarations") {
            return Err(ClientError::FeatureUnavailable {
                feature: "declarations",
            });
        }
        if owners.len() > smaller_bound(capabilities.bounds.packages_max, PACKAGES_MAX) {
            return Err(ClientError::InvalidRequest { field: "positions" });
        }
        let body = serialize_body(request, self.inner.config.max_request)?;
        validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
        let raw = self
            .request(
                contract::Endpoint::Declarations,
                Some(body),
                None,
                active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
            )
            .await;
        let raw = self.observed(raw).await?;
        let parsed = self.observed(response::source_declarations(raw)).await?;
        if !parsed.value.is_valid_for(request)
            || !parsed.value.results.iter().all(|result| match result {
                DeclarationPositionResult::Found { id, .. } => SymbolIdentity::parse(id.as_str())
                    .is_ok_and(|identity| {
                        matches!(
                            identity.owner(),
                            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
                        )
                    }),
                _ => true,
            })
        {
            return self
                .observed(Err(ClientError::InvalidResponse {
                    status: parsed.meta.status,
                }))
                .await;
        }
        Ok(parsed.value)
    }
}

fn global_source_owner(
    unit: &str,
    view: Option<&rift_protocol::symbol_read::CapturedViewId>,
) -> Result<Option<SymbolOwner>, ClientError> {
    let (owner, _) = parse_source_unit_identity(unit)
        .map_err(|_| ClientError::InvalidRequest { field: "unit" })?;
    match owner {
        Some(owner @ (SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. })) => {
            Ok(Some(owner))
        }
        None if view.is_some()
            && rift_protocol::read::SourceUnitId::parse(unit)
                .is_ok_and(|unit| !unit.is_project()) =>
        {
            Ok(None)
        }
        _ => Err(ClientError::InvalidRequest { field: "unit" }),
    }
}
