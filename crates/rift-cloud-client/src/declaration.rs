//! Declarations holding positions in files of exact package versions.

use std::collections::HashSet;

use crate::{
    Capabilities, ClientError, GlobalClient, PACKAGES_MAX, PackageDeclarationRequest,
    PackageDeclarationResponse, PackagePosition, active_response_body_bytes_max, contract, domain,
    response, serialize_body, smaller_bound, supports_feature, validate_body_for_capabilities,
    validate_package_identity,
};

/// Most positions one declaration request carries.
pub const DECLARATION_POSITIONS_MAX: usize = 1_000;

/// Largest line or character one position carries, the `uinteger` bound of the Language
/// Server Protocol whose positions the request forwards.
pub const POSITION_COMPONENT_MAX: i64 = 2_147_483_647;

/// The capability feature a server advertises when it names declarations at positions.
const DECLARATION_FEATURE: &str = "declarations";

/// One position as the client accounts for it: package, path, line, and character.
type PositionKey<'a> = (&'a str, &'a str, &'a str, &'a str, i64, i64);

impl GlobalClient {
    /// Names the smallest declaration holding each position, at the exact package versions the
    /// positions name.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, the server does
    /// not advertise declaration lookup, or the endpoint is unavailable.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future abandons the request. Capabilities already read stay cached, and the
    /// abandoned request records no failure.
    pub async fn find_package_declarations(
        &self,
        request: &PackageDeclarationRequest,
    ) -> Result<PackageDeclarationResponse, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_declaration_request(request)?;
        let capabilities = self.get_capabilities().await?;
        validate_declaration_request_for_capabilities(request, &capabilities)?;
        let body = serialize_body(request)?;
        validate_body_for_capabilities(&body, &capabilities)?;
        let response_body_bytes_max = active_response_body_bytes_max(&capabilities);
        let raw = self
            .request(
                contract::Endpoint::Declarations,
                Some(body),
                None,
                response_body_bytes_max,
            )
            .await;
        let raw = self.observed(raw).await?;
        let answer = response::declarations(raw).await.map(|parsed| parsed.value);
        let answer = self.observed(answer).await?;
        self.observed(validate_declaration_response(request, &answer))
            .await?;
        Ok(answer)
    }
}

pub(crate) fn validate_declaration_request(
    request: &PackageDeclarationRequest,
) -> Result<(), ClientError> {
    let count = request.positions.len();
    if count == 0 || count > DECLARATION_POSITIONS_MAX {
        return Err(ClientError::InvalidRequest { field: "positions" });
    }
    let mut seen = HashSet::new();
    for position in &request.positions {
        validate_position(position)?;
        if !seen.insert(position_key(position)) {
            return Err(ClientError::InvalidRequest {
                field: "duplicate_position",
            });
        }
    }
    Ok(())
}

fn validate_position(position: &PackagePosition) -> Result<(), ClientError> {
    validate_package_identity(&position.package)?;
    let path_names_a_file = rift_core::ProjectPath::new(position.path.as_str())
        .ok()
        .filter(|path| !path.as_str().is_empty())
        .is_some_and(|path| {
            rift_core::SourceUnitId::for_package(
                &domain::package_identity(&position.package),
                &path,
            )
            .is_ok()
        });
    if !path_names_a_file {
        return Err(ClientError::InvalidRequest { field: "path" });
    }
    let component = 0..=POSITION_COMPONENT_MAX;
    if !component.contains(&position.line) {
        return Err(ClientError::InvalidRequest { field: "line" });
    }
    if !component.contains(&position.character) {
        return Err(ClientError::InvalidRequest { field: "character" });
    }
    Ok(())
}

pub(crate) fn validate_declaration_request_for_capabilities(
    request: &PackageDeclarationRequest,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    if !supports_feature(capabilities, DECLARATION_FEATURE) {
        return Err(ClientError::InvalidRequest { field: "positions" });
    }
    let packages = request
        .positions
        .iter()
        .map(|position| {
            let package = &position.package;
            (&package.manager, &package.name, &package.version)
        })
        .collect::<HashSet<_>>();
    if packages.len() > smaller_bound(capabilities.bounds.packages_max, PACKAGES_MAX) {
        return Err(ClientError::InvalidRequest { field: "packages" });
    }
    Ok(())
}

/// Checks that the answer names every submitted position exactly once, each beside a
/// canonical symbol identity or none.
pub(crate) fn validate_declaration_response(
    request: &PackageDeclarationRequest,
    response: &PackageDeclarationResponse,
) -> Result<(), ClientError> {
    let expected: HashSet<_> = request.positions.iter().map(position_key).collect();
    let mut seen = HashSet::new();
    for result in &response.results {
        let key = position_key(&result.position);
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "position_accounting",
            });
        }
        if result
            .declaration
            .as_deref()
            .is_some_and(|id| rift_core::parse_symbol_identity(id).is_err())
        {
            return Err(ClientError::InvalidResponseField {
                field: "declaration",
            });
        }
    }
    if seen.len() != expected.len() {
        return Err(ClientError::InvalidResponseField {
            field: "position_accounting",
        });
    }
    Ok(())
}

fn position_key(position: &PackagePosition) -> PositionKey<'_> {
    let package = &position.package;
    (
        package.manager.as_str(),
        package.name.as_str(),
        package.version.as_str(),
        position.path.as_str(),
        position.line,
        position.character,
    )
}
