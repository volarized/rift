//! Generated response decoding after bounded body collection.

use crate::{
    Capabilities, ClientError, GetCapabilitiesRequest, GetCapabilitiesResponse,
    ListPackageSymbolsRequest, ListPackageSymbolsResponse, PackageResolutionResponse,
    PackageSearchPage, PackageSymbolPage, ProblemDetails, RawResponse,
    ResolvePackageContextRequest, ResolvePackageContextResponse, ResponseMeta,
    SearchPackagesRequest, SearchPackagesResponse,
};

pub(crate) struct Parsed<T> {
    pub(crate) value: T,
    pub(crate) meta: ResponseMeta,
}

pub(crate) enum ParsedCapabilities {
    Ok(Box<Parsed<Capabilities>>),
    NotModified(ResponseMeta),
}

pub(crate) async fn capabilities(response: RawResponse) -> Result<ParsedCapabilities, ClientError> {
    let (response, meta) = generated_response(response)?;
    let status = meta.status;
    let response = GetCapabilitiesRequest::parse_response(response)
        .await
        .map_err(|_| ClientError::Decode { status })?;
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
        | GetCapabilitiesResponse::GatewayTimeout(problem) => Err(http_error(meta, problem)),
        GetCapabilitiesResponse::Unknown => Err(unknown_http_error(meta)),
    }
}

pub(crate) async fn resolution(
    response: RawResponse,
) -> Result<Parsed<PackageResolutionResponse>, ClientError> {
    let (response, meta) = generated_response(response)?;
    let status = meta.status;
    let response = ResolvePackageContextRequest::parse_response(response)
        .await
        .map_err(|_| ClientError::Decode { status })?;
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
        | ResolvePackageContextResponse::GatewayTimeout(problem) => Err(http_error(meta, problem)),
        ResolvePackageContextResponse::Unknown => Err(unknown_http_error(meta)),
    }
}

pub(crate) async fn search(
    response: RawResponse,
) -> Result<Parsed<PackageSearchPage>, ClientError> {
    let (response, meta) = generated_response(response)?;
    let status = meta.status;
    let response = SearchPackagesRequest::parse_response(response)
        .await
        .map_err(|_| ClientError::Decode { status })?;
    match response {
        SearchPackagesResponse::Ok(value) => Ok(Parsed { value, meta }),
        SearchPackagesResponse::BadRequest(problem)
        | SearchPackagesResponse::Unauthorized(problem)
        | SearchPackagesResponse::Forbidden(problem)
        | SearchPackagesResponse::NotAcceptable(problem)
        | SearchPackagesResponse::ContentTooLarge(problem)
        | SearchPackagesResponse::UnsupportedMediaType(problem)
        | SearchPackagesResponse::TooManyRequests(problem)
        | SearchPackagesResponse::InternalServerError(problem)
        | SearchPackagesResponse::BadGateway(problem)
        | SearchPackagesResponse::ServiceUnavailable(problem)
        | SearchPackagesResponse::GatewayTimeout(problem) => Err(http_error(meta, problem)),
        SearchPackagesResponse::Unknown => Err(unknown_http_error(meta)),
    }
}

pub(crate) async fn symbols(
    response: RawResponse,
) -> Result<Parsed<PackageSymbolPage>, ClientError> {
    let (response, meta) = generated_response(response)?;
    let status = meta.status;
    let response = ListPackageSymbolsRequest::parse_response(response)
        .await
        .map_err(|_| ClientError::Decode { status })?;
    match response {
        ListPackageSymbolsResponse::Ok(value) => Ok(Parsed { value, meta }),
        ListPackageSymbolsResponse::BadRequest(problem)
        | ListPackageSymbolsResponse::Unauthorized(problem)
        | ListPackageSymbolsResponse::Forbidden(problem)
        | ListPackageSymbolsResponse::NotAcceptable(problem)
        | ListPackageSymbolsResponse::ContentTooLarge(problem)
        | ListPackageSymbolsResponse::UnsupportedMediaType(problem)
        | ListPackageSymbolsResponse::TooManyRequests(problem)
        | ListPackageSymbolsResponse::InternalServerError(problem)
        | ListPackageSymbolsResponse::BadGateway(problem)
        | ListPackageSymbolsResponse::ServiceUnavailable(problem)
        | ListPackageSymbolsResponse::GatewayTimeout(problem) => Err(http_error(meta, problem)),
        ListPackageSymbolsResponse::Unknown => Err(unknown_http_error(meta)),
    }
}

fn generated_response(
    response: RawResponse,
) -> Result<(reqwest::Response, ResponseMeta), ClientError> {
    let RawResponse { status, body, meta } = response;
    let response = http::Response::builder()
        .status(status)
        .body(body)
        .map(reqwest::Response::from)
        .map_err(|_| ClientError::InvalidResponse {
            status: status.as_u16(),
        })?;
    Ok((response, meta))
}

fn http_error(meta: ResponseMeta, problem: ProblemDetails) -> ClientError {
    ClientError::Http {
        meta: Box::new(meta),
        problem: Some(Box::new(problem)),
    }
}

fn unknown_http_error(meta: ResponseMeta) -> ClientError {
    ClientError::Http {
        meta: Box::new(meta),
        problem: None,
    }
}
