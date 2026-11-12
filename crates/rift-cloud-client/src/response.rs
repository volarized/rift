//! Generated response decoding after bounded body collection.

use rift_error::RiftError;

use crate::{
    Capabilities, GetCapabilitiesRequest, GetCapabilitiesResponse, PackagePatternPage,
    PackageResolutionResponse, PackageSearchPage, RawResponse, ResolvePackageContextRequest,
    ResolvePackageContextResponse, ResponseMeta, SearchPackagePatternsRequest,
    SearchPackagePatternsResponse, contract, failure,
};

pub(crate) struct Parsed<T> {
    pub(crate) value: T,
    pub(crate) meta: ResponseMeta,
}

pub(crate) enum ParsedCapabilities {
    Ok(Box<Parsed<Capabilities>>),
    NotModified(ResponseMeta),
}

pub(crate) async fn capabilities(response: RawResponse) -> Result<ParsedCapabilities, RiftError> {
    let operation = contract::Endpoint::Capabilities.operation_id();
    let (response, meta) = generated_response(response, operation)?;
    let status = meta.status;
    let response = GetCapabilitiesRequest::parse_response(response)
        .await
        .map_err(|error| {
            failure::response_source(
                operation,
                "body",
                "global response decode failed",
                Some(status),
                error,
            )
        })?;
    match response {
        GetCapabilitiesResponse::Ok(value) => {
            Ok(ParsedCapabilities::Ok(Box::new(Parsed { value, meta })))
        }
        GetCapabilitiesResponse::NotModified => Ok(ParsedCapabilities::NotModified(meta)),
        GetCapabilitiesResponse::BadRequest(problem)
        | GetCapabilitiesResponse::Unauthorized(problem)
        | GetCapabilitiesResponse::Forbidden(problem)
        | GetCapabilitiesResponse::NotAcceptable(problem)
        | GetCapabilitiesResponse::TooManyRequests(problem)
        | GetCapabilitiesResponse::InternalServerError(problem)
        | GetCapabilitiesResponse::BadGateway(problem)
        | GetCapabilitiesResponse::ServiceUnavailable(problem)
        | GetCapabilitiesResponse::GatewayTimeout(problem) => {
            Err(failure::remote(operation, &meta, Some(&problem)))
        }
        GetCapabilitiesResponse::Unknown => Err(failure::remote(operation, &meta, None)),
    }
}

pub(crate) async fn resolution(
    response: RawResponse,
) -> Result<Parsed<PackageResolutionResponse>, RiftError> {
    let operation = contract::Endpoint::Resolutions.operation_id();
    let (response, meta) = generated_response(response, operation)?;
    let status = meta.status;
    let response = ResolvePackageContextRequest::parse_response(response)
        .await
        .map_err(|error| {
            failure::response_source(
                operation,
                "body",
                "global response decode failed",
                Some(status),
                error,
            )
        })?;
    match response {
        ResolvePackageContextResponse::Ok(value) => Ok(Parsed { value, meta }),
        ResolvePackageContextResponse::BadRequest(problem)
        | ResolvePackageContextResponse::Unauthorized(problem)
        | ResolvePackageContextResponse::Forbidden(problem)
        | ResolvePackageContextResponse::NotAcceptable(problem)
        | ResolvePackageContextResponse::ContentTooLarge(problem)
        | ResolvePackageContextResponse::UnsupportedMediaType(problem)
        | ResolvePackageContextResponse::TooManyRequests(problem)
        | ResolvePackageContextResponse::InternalServerError(problem)
        | ResolvePackageContextResponse::BadGateway(problem)
        | ResolvePackageContextResponse::ServiceUnavailable(problem)
        | ResolvePackageContextResponse::GatewayTimeout(problem) => {
            Err(failure::remote(operation, &meta, Some(&problem)))
        }
        ResolvePackageContextResponse::Unknown => Err(failure::remote(operation, &meta, None)),
    }
}

pub(crate) fn search(response: RawResponse) -> Result<Parsed<PackageSearchPage>, RiftError> {
    exact_response(
        response,
        contract::Endpoint::Search.operation_id(),
        &[200],
        |_, _| true,
    )
}

pub(crate) fn exact_symbol(
    response: RawResponse,
) -> Result<Parsed<rift_protocol::symbol_read::GetSymbolResult>, RiftError> {
    use rift_protocol::symbol_read::GetSymbolResult;
    exact_response(
        response,
        contract::Endpoint::Symbols.operation_id(),
        &[200, 404, 503],
        |status, value| {
            matches!(
                (status, value),
                (200, GetSymbolResult::Found { .. })
                    | (404, GetSymbolResult::Missing { .. })
                    | (503, GetSymbolResult::Unavailable { .. })
            )
        },
    )
}

/// The nodes page a `/v1/nodes` answer carries. A problem answering 404 states that the
/// global API holds no declaration of `symbol`, which the local read refuses the same way.
pub(crate) fn nodes(
    response: RawResponse,
    symbol: &rift_protocol::read::SymbolId,
) -> Result<Parsed<rift_protocol::read::NodesResult>, RiftError> {
    if response.status == reqwest::StatusCode::NOT_FOUND
        && crate::is_media(&response.meta, "application/problem+json")
    {
        return Err(rift_error::errors::server::read_symbol_not_found()
            .id(symbol.as_str())
            .error());
    }
    exact_response(
        response,
        contract::Endpoint::Nodes.operation_id(),
        &[200],
        |_, _| true,
    )
}

pub(crate) fn source_declarations(
    response: RawResponse,
) -> Result<Parsed<rift_protocol::source_read::FindDeclarationsResult>, RiftError> {
    exact_response(
        response,
        contract::Endpoint::Declarations.operation_id(),
        &[200],
        |_, _| true,
    )
}

fn exact_response<T: serde::de::DeserializeOwned>(
    response: RawResponse,
    operation: &'static str,
    statuses: &[u16],
    accepts: impl FnOnce(u16, &T) -> bool,
) -> Result<Parsed<T>, RiftError> {
    let RawResponse { status, body, meta } = response;
    if statuses.contains(&status.as_u16()) && crate::is_media(&meta, "application/json") {
        let value: T = serde_json::from_slice(&body).map_err(|error| {
            failure::response_source(
                operation,
                "body",
                "global response decode failed",
                Some(status.as_u16()),
                error,
            )
        })?;
        if !accepts(status.as_u16(), &value) {
            return Err(failure::response_invalid(
                operation,
                "status",
                "unsupported global response status",
                Some(status.as_u16()),
            ));
        }
        return Ok(Parsed { value, meta });
    }
    if status == reqwest::StatusCode::OK {
        return Err(failure::response_invalid(
            operation,
            "content_type",
            &format!("invalid response media type: {:?}", meta.content_type),
            Some(status.as_u16()),
        ));
    }
    if matches!(
        status.as_u16(),
        400 | 401 | 403 | 406 | 413 | 415 | 429 | 500 | 502 | 503 | 504
    ) && crate::is_media(&meta, "application/problem+json")
    {
        let problem = serde_json::from_slice(&body).map_err(|error| {
            failure::response_source(
                operation,
                "body",
                "global response decode failed",
                Some(status.as_u16()),
                error,
            )
        })?;
        return Err(failure::remote(operation, &meta, Some(&problem)));
    }
    Err(failure::remote(operation, &meta, None))
}

pub(crate) async fn patterns(
    response: RawResponse,
) -> Result<Parsed<PackagePatternPage>, RiftError> {
    let operation = contract::Endpoint::Patterns.operation_id();
    let (response, meta) = generated_response(response, operation)?;
    let status = meta.status;
    let response = SearchPackagePatternsRequest::parse_response(response)
        .await
        .map_err(|error| {
            failure::response_source(
                operation,
                "body",
                "global response decode failed",
                Some(status),
                error,
            )
        })?;
    match response {
        SearchPackagePatternsResponse::Ok(value) => Ok(Parsed { value, meta }),
        SearchPackagePatternsResponse::BadRequest(problem)
        | SearchPackagePatternsResponse::Unauthorized(problem)
        | SearchPackagePatternsResponse::Forbidden(problem)
        | SearchPackagePatternsResponse::NotAcceptable(problem)
        | SearchPackagePatternsResponse::ContentTooLarge(problem)
        | SearchPackagePatternsResponse::UnsupportedMediaType(problem)
        | SearchPackagePatternsResponse::TooManyRequests(problem)
        | SearchPackagePatternsResponse::InternalServerError(problem)
        | SearchPackagePatternsResponse::BadGateway(problem)
        | SearchPackagePatternsResponse::ServiceUnavailable(problem)
        | SearchPackagePatternsResponse::GatewayTimeout(problem) => {
            Err(failure::remote(operation, &meta, Some(&problem)))
        }
        SearchPackagePatternsResponse::Unknown => Err(failure::remote(operation, &meta, None)),
    }
}

fn generated_response(
    response: RawResponse,
    operation: &'static str,
) -> Result<(reqwest::Response, ResponseMeta), RiftError> {
    let RawResponse { status, body, meta } = response;
    let response = http::Response::builder()
        .status(status)
        .body(body)
        .map(reqwest::Response::from)
        .map_err(|error| {
            failure::response_source(
                operation,
                "status",
                "global response construction failed",
                Some(status.as_u16()),
                error,
            )
        })?;
    Ok((response, meta))
}
