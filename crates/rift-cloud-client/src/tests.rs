use super::*;
use rift_protocol::{
    read::GetSymbolInclude,
    symbol_read::{GetSymbolParams, GetSymbolResult},
};
use serde_json::json;

fn assert_registered_field(error: &rift_error::RiftError, slug: &str, field: &str) {
    assert_eq!(error.slug().as_str(), slug, "{error}");
    assert!(
        error
            .context()
            .any(|(key, value)| key == "field" && value == field),
        "{error}"
    );
}

fn remote_metadata(error: &RiftError) -> serde_json::Value {
    serde_json::from_str(error_text_field(error, "metadata")).expect("retained response metadata")
}

fn assert_remote_error(actual: &RiftError, expected: &RiftError) {
    assert_eq!(actual.slug(), expected.slug());
    assert_eq!(remote_metadata(actual), remote_metadata(expected));
    let problem = |error: &RiftError| {
        error.fields().iter().find_map(|field| match field.value() {
            rift_error::ErrorValue::Display(value) if field.key() == "problem" => {
                Some(serde_json::from_str::<serde_json::Value>(value).expect("retained problem"))
            }
            _ => None,
        })
    };
    assert_eq!(problem(actual), problem(expected));
}

fn error_text_field<'error>(error: &'error rift_error::RiftError, key: &str) -> &'error str {
    error
        .fields()
        .iter()
        .find_map(|field| match field.value() {
            rift_error::ErrorValue::Display(value) if field.key() == key => Some(value.as_str()),
            _ => None,
        })
        .expect("registered error field")
}

mod cache;
mod collection;
mod declaration;
mod documentation;
mod nodes;
mod pages;
mod pattern;
mod resolution;

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{Method, Request, header},
    response::{IntoResponse, Response},
};
use tokio::{net::TcpListener, sync::Mutex as AsyncMutex, task::JoinHandle};

#[derive(Clone)]
enum FixtureMode {
    Capabilities,
    CapabilitiesDelay(Duration),
    Redirect,
    WrongMedia,
    MalformedJson,
    Unauthorized,
    RetryAfter {
        status: StatusCode,
        value: &'static str,
    },
    StatusSequence(Vec<StatusCode>),
    Oversize,
    Delay(Duration),
    Operations(OperationFixture),
}

#[derive(Clone)]
enum OperationFixture {
    Valid,
    ExactSymbol(serde_json::Value),
    SourceDeclarations(serde_json::Value, StatusCode),
    /// The `/v1/nodes` answer: this body with this status, or a problem for a 404.
    Nodes(serde_json::Value, StatusCode),
    Documentation {
        search: serde_json::Value,
        symbols: serde_json::Value,
    },
    DocumentationBounds {
        search: serde_json::Value,
        symbols: serde_json::Value,
        source_bytes_max: usize,
    },
    EchoResolution,
    InvalidResolutionAccounting,
    SearchPages,
    MismatchedCursor,
    PageRevisionChange,
    UnrequestedPackage,
    InvalidPublication,
    MissingRequiredField,
    TightBounds,
    TightRequestBound,
    TightSourceBound,
    CollectionBounds {
        source_bytes: usize,
        source_bytes_max: usize,
    },
    CandidateBound,
    PartialFailure,
    RetrySearch,
    RepeatedCursor,
    InvalidOrigin,
    InvalidSource,
    InvalidSourceIdentity,
    InvalidMatchClass,
    UnknownMatchClass,
    PageLimitMax(i64),
    NamedSubstitution,
    UnnamedSubstitution,
    SubstitutionAtRequestedVersion,
    RequirementOutsideRange,
    BodyBoundStop,
    Patterns,
    Declarations,
    Problem(StatusCode),
    AdditiveResponse,
}

struct FixtureState {
    mode: FixtureMode,
    requests: AtomicUsize,
    current: AtomicUsize,
    max_current: AtomicUsize,
    last_authorization: AsyncMutex<Option<String>>,
    last_accept: AsyncMutex<Option<String>>,
    last_if_none_match: AsyncMutex<Option<String>>,
    last_path: AsyncMutex<Option<String>>,
    last_query: AsyncMutex<Option<String>>,
    request_log: AsyncMutex<Vec<CapturedRequest>>,
}

#[derive(Debug, Eq, PartialEq)]
struct CapturedRequest {
    method: Method,
    path: String,
    query: Option<String>,
    body: Vec<u8>,
}

struct FixtureServer {
    endpoint: String,
    state: Arc<FixtureState>,
    task: JoinHandle<()>,
}

impl FixtureServer {
    async fn start(mode: FixtureMode) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = Arc::new(FixtureState {
            mode,
            requests: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            max_current: AtomicUsize::new(0),
            last_authorization: AsyncMutex::new(None),
            last_accept: AsyncMutex::new(None),
            last_if_none_match: AsyncMutex::new(None),
            last_path: AsyncMutex::new(None),
            last_query: AsyncMutex::new(None),
            request_log: AsyncMutex::new(Vec::new()),
        });
        let app = Router::new()
            .fallback(fixture_handler)
            .with_state(Arc::clone(&state));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            endpoint: format!("http://{address}/rift/rest"),
            state,
            task,
        })
    }

    fn config(&self) -> Config {
        Config {
            endpoint: self.endpoint.clone(),
            capabilities_ttl: Duration::from_secs(60),
            ..Config::default()
        }
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one fixture handler keeps request capture and every transport response together"
)]
async fn fixture_handler(
    State(state): State<Arc<FixtureState>>,
    request: Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query().map(str::to_owned);
    let body = to_bytes(body, REQUEST_BODY_BYTES_MAX)
        .await
        .expect("fixture request body")
        .to_vec();
    *state.last_authorization.lock().await = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    *state.last_accept.lock().await = parts
        .headers
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    *state.last_if_none_match.lock().await = parts
        .headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    *state.last_path.lock().await = Some(path.clone());
    *state.last_query.lock().await = query.clone();
    state.request_log.lock().await.push(CapturedRequest {
        method: parts.method,
        path: path.clone(),
        query: query.clone(),
        body: body.clone(),
    });

    let request_number = state.requests.fetch_add(1, Ordering::SeqCst);
    let current = state.current.fetch_add(1, Ordering::SeqCst) + 1;
    let mut observed = state.max_current.load(Ordering::SeqCst);
    while current > observed {
        match state.max_current.compare_exchange(
            observed,
            current,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => break,
            Err(value) => observed = value,
        }
    }
    let operation_mode = match &state.mode {
        FixtureMode::Operations(mode) => Some(mode.clone()),
        _ => None,
    };
    let response = if let Some(mode) = operation_mode {
        operation_response(&mode, &path, query.as_deref(), request_number, &body)
    } else {
        match &state.mode {
            FixtureMode::Capabilities | FixtureMode::CapabilitiesDelay(_) => {
                if let FixtureMode::CapabilitiesDelay(delay) = &state.mode {
                    tokio::time::sleep(*delay).await;
                }
                if state.last_if_none_match.lock().await.as_deref() == Some("v1") {
                    (
                        StatusCode::NOT_MODIFIED,
                        [(header::ETAG, "v1"), (header::CACHE_CONTROL, "max-age=0")],
                        Body::empty(),
                    )
                        .into_response()
                } else {
                    let mut response = capabilities_response();
                    if matches!(&state.mode, FixtureMode::CapabilitiesDelay(_)) {
                        response.headers_mut().insert(
                            header::CACHE_CONTROL,
                            HeaderValue::from_static("max-age=60"),
                        );
                    }
                    response
                }
            }
            FixtureMode::Redirect => (
                StatusCode::TEMPORARY_REDIRECT,
                [(header::LOCATION, "/rift/rest/v1/capabilities")],
                Body::empty(),
            )
                .into_response(),
            FixtureMode::WrongMedia => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain")],
                Body::from("not json"),
            )
                .into_response(),
            FixtureMode::MalformedJson => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                Body::from("{"),
            )
                .into_response(),
            FixtureMode::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [
                    (header::CONTENT_TYPE, "application/problem+json"),
                    (header::WWW_AUTHENTICATE, "Bearer realm=rift"),
                ],
                Body::from(problem_json(StatusCode::UNAUTHORIZED)),
            )
                .into_response(),
            FixtureMode::RetryAfter { status, value } => {
                let mut response = problem_response(*status);
                response
                    .headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from_static(value));
                response
            }
            FixtureMode::StatusSequence(statuses) => {
                let status = match statuses.get(request_number) {
                    Some(value) => *value,
                    None => StatusCode::INTERNAL_SERVER_ERROR,
                };
                if status == StatusCode::OK {
                    capabilities_response()
                } else {
                    let mut response = (
                        status,
                        [(header::CONTENT_TYPE, "application/problem+json")],
                        Body::from(problem_json(status)),
                    )
                        .into_response();
                    if status == StatusCode::TOO_MANY_REQUESTS
                        || status == StatusCode::SERVICE_UNAVAILABLE
                        || status == StatusCode::GATEWAY_TIMEOUT
                    {
                        response
                            .headers_mut()
                            .insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
                    }
                    if status == StatusCode::TOO_MANY_REQUESTS {
                        response
                            .headers_mut()
                            .insert(RATE_LIMIT_LIMIT, HeaderValue::from_static("10"));
                        response
                            .headers_mut()
                            .insert(RATE_LIMIT_REMAINING, HeaderValue::from_static("0"));
                        response
                            .headers_mut()
                            .insert(RATE_LIMIT_RESET, HeaderValue::from_static("60"));
                    }
                    response
                }
            }
            FixtureMode::Oversize => {
                let body = vec![b'x'; RESPONSE_BODY_BYTES_MAX.saturating_add(1)];
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    Body::from(body),
                )
                    .into_response()
            }
            FixtureMode::Delay(delay) => {
                tokio::time::sleep(*delay).await;
                StatusCode::NO_CONTENT.into_response()
            }
            FixtureMode::Operations(_) => unreachable!(),
        }
    };
    state.current.fetch_sub(1, Ordering::SeqCst);
    response
}

fn operation_response(
    mode: &OperationFixture,
    path: &str,
    query: Option<&str>,
    request_number: usize,
    request_body: &[u8],
) -> Response {
    if path.ends_with("/capabilities") {
        return operation_capabilities_response(mode);
    }
    if let OperationFixture::Problem(status) = mode {
        return problem_response(*status);
    }
    if path.ends_with("/resolutions") {
        return operation_resolution_response(mode, request_body);
    }
    if path.ends_with("/search") {
        return operation_search_response(mode, query, request_number);
    }
    if path.ends_with("/symbols") {
        if let OperationFixture::ExactSymbol(value) = mode {
            let mut response = json_response(value, None);
            *response.status_mut() = match value["outcome"].as_str() {
                Some("missing") => StatusCode::NOT_FOUND,
                Some("unavailable") => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::OK,
            };
            return response;
        }
        return operation_symbol_response(mode, query);
    }
    if path.ends_with("/declarations") {
        if let OperationFixture::SourceDeclarations(value, status) = mode {
            let mut response = json_response(value, None);
            *response.status_mut() = *status;
            return response;
        }
        return json_response(&declaration::declaration_response_json(), None);
    }
    if path.ends_with("/nodes") {
        return match mode {
            OperationFixture::Nodes(_, StatusCode::NOT_FOUND) => {
                problem_response(StatusCode::NOT_FOUND)
            }
            OperationFixture::Nodes(value, status) => {
                let mut response = json_response(value, None);
                *response.status_mut() = *status;
                response
            }
            OperationFixture::CollectionBounds { source_bytes, .. } => {
                json_response(&nodes::single_node_response_json(*source_bytes), None)
            }
            _ => json_response(&nodes::nodes_response_json(), None),
        };
    }
    if path.ends_with("/patterns") {
        let mut page = pattern::pattern_page_json(query_cursor(query));
        if let OperationFixture::CollectionBounds { source_bytes, .. } = mode {
            page["items"][0]["source"] = serde_json::json!("x".repeat(*source_bytes));
            page["items"][0]["declaration"]["source"] =
                serde_json::json!("x".repeat(*source_bytes));
        }
        return json_response(&page, None);
    }
    status_response(StatusCode::NOT_FOUND)
}

/// The `cursor_bytes_max` the nodes fixture advertises, below the client's own bound.
const NODES_CURSOR_BYTES_ADVERTISED: usize = 64;

/// The `page_limit_max` the pattern fixture advertises, below the client's own bound.
const PATTERN_PAGE_LIMIT_ADVERTISED: i64 = 200;

fn operation_capabilities_response(mode: &OperationFixture) -> Response {
    capabilities_response_with(|value| match mode {
        OperationFixture::Documentation { .. } | OperationFixture::DocumentationBounds { .. } => {
            value["supported_features"] = serde_json::json!([
                "resolutions",
                "search",
                "symbols",
                "documentation_search",
                "symbol_documentation"
            ]);
            value["documentation_revision"] = serde_json::json!("0123abcd");
            if let OperationFixture::DocumentationBounds {
                source_bytes_max, ..
            } = mode
            {
                value["bounds"]["source_bytes_max"] = serde_json::json!(source_bytes_max);
            }
        }
        OperationFixture::InvalidPublication => {
            value["publication_format"] = serde_json::json!("other");
        }
        OperationFixture::MissingRequiredField => {
            value["required_search_fields"] = serde_json::json!(["name"]);
        }
        OperationFixture::TightBounds => {
            value["bounds"]["response_body_bytes_max"] = serde_json::json!(1);
            value["bounds"]["packages_max"] = serde_json::json!(1);
            value["bounds"]["page_limit_max"] = serde_json::json!(2);
            value["bounds"]["page_limit_default"] = serde_json::json!(2);
        }
        OperationFixture::TightRequestBound => {
            value["bounds"]["request_body_bytes_max"] = serde_json::json!(1);
        }
        OperationFixture::TightSourceBound => {
            value["bounds"]["source_bytes_max"] = serde_json::json!(1);
        }
        OperationFixture::CollectionBounds {
            source_bytes_max, ..
        } => {
            value["supported_features"] =
                serde_json::json!(["resolutions", "search", "symbols", "patterns", "nodes"]);
            value["bounds"]["request_body_bytes_max"] =
                serde_json::json!(rift_protocol::configuration::GLOBAL_REQUEST_BYTES_MAX);
            value["bounds"]["response_body_bytes_max"] =
                serde_json::json!(rift_protocol::configuration::GLOBAL_RESPONSE_BYTES_MAX);
            value["bounds"]["source_bytes_max"] = serde_json::json!(source_bytes_max);
        }
        OperationFixture::CandidateBound => {
            value["bounds"]["candidate_pool_max"] = serde_json::json!(1);
        }
        OperationFixture::AdditiveResponse => {
            value["future_field"] = serde_json::json!("ignored by this client");
        }
        OperationFixture::PageLimitMax(advertised) => {
            value["bounds"]["page_limit_max"] = serde_json::json!(advertised);
        }
        OperationFixture::Nodes(_, _) => {
            value["supported_features"] =
                serde_json::json!(["resolutions", "search", "symbols", "nodes"]);
            value["bounds"]["cursor_bytes_max"] = serde_json::json!(NODES_CURSOR_BYTES_ADVERTISED);
        }
        OperationFixture::Declarations | OperationFixture::SourceDeclarations(_, _) => {
            value["supported_features"] =
                serde_json::json!(["resolutions", "search", "symbols", "declarations"]);
        }
        OperationFixture::Patterns => {
            value["supported_features"] =
                serde_json::json!(["resolutions", "search", "symbols", "patterns"]);
            value["bounds"]["page_limit_max"] = serde_json::json!(PATTERN_PAGE_LIMIT_ADVERTISED);
        }
        OperationFixture::Problem(_) => {
            value["supported_features"] = serde_json::json!([
                "resolutions",
                "search",
                "symbols",
                "patterns",
                "declarations",
                "nodes"
            ]);
        }
        _ => {}
    })
}

fn operation_resolution_response(mode: &OperationFixture, request_body: &[u8]) -> Response {
    if matches!(mode, OperationFixture::EchoResolution) {
        let request: serde_json::Value =
            serde_json::from_slice(request_body).expect("resolution request fixture");
        let entries = request["entries"]
            .as_array()
            .expect("resolution entries fixture");
        let mut available_exact = Vec::new();
        let mut missing_requirements = Vec::new();
        for entry in entries {
            if let Some(version) = entry["version"].as_str() {
                available_exact.push(serde_json::json!({
                    "manager": entry["manager"],
                    "registry": entry["registry"],
                    "name": entry["name"],
                    "version": version,
                }));
            } else {
                missing_requirements.push(entry.clone());
            }
        }
        return json_response(
            &serde_json::json!({
                "available_exact": available_exact,
                "resolved_requirements": [],
                "missing_exact": [],
                "missing_requirements": missing_requirements,
            }),
            Some("max-age=60"),
        );
    }
    if let Some(body) = resolution::substitution_response(mode) {
        return json_response(&body, Some("max-age=60"));
    }
    let body = match mode {
        OperationFixture::InvalidResolutionAccounting => serde_json::json!({
            "available_exact": [],
            "resolved_requirements": [],
            "missing_exact": [],
            "missing_requirements": []
        }),
        _ => serde_json::json!({
            "available_exact": [],
            "resolved_requirements": [{
                "entry": {"availability":"canonical","manager":"cargo","registry":"crates.io","name":"demo","requirement":"^1","version":null},
                "package": {"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}
            }],
            "missing_exact": [],
            "missing_requirements": []
        }),
    };
    json_response(&body, Some("max-age=60"))
}

fn operation_search_response(
    mode: &OperationFixture,
    query: Option<&str>,
    request_number: usize,
) -> Response {
    if matches!(mode, OperationFixture::PartialFailure) && request_number > 1 {
        return problem_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    if matches!(mode, OperationFixture::RetrySearch) && request_number == 1 {
        return problem_response_with_retry(StatusCode::SERVICE_UNAVAILABLE);
    }
    let cursor = query_cursor(query);
    let body = match mode {
        OperationFixture::Documentation { search, .. }
        | OperationFixture::DocumentationBounds { search, .. } => search.clone(),
        OperationFixture::UnrequestedPackage => {
            search_page_json("other", None, "analyzer-v1", "first")
        }
        OperationFixture::InvalidOrigin
        | OperationFixture::InvalidSource
        | OperationFixture::InvalidSourceIdentity
        | OperationFixture::InvalidMatchClass => invalid_search_page(mode),
        OperationFixture::UnknownMatchClass => {
            let mut page = search_page_json("demo", None, "analyzer-v1", "first");
            page["items"][0]["match_class"] = serde_json::json!("unknown");
            page
        }
        OperationFixture::TightSourceBound => {
            let mut page = search_page_json("demo", None, "analyzer-v1", "first");
            page["items"][0]["source"] = serde_json::json!("too large");
            page
        }
        OperationFixture::CollectionBounds { source_bytes, .. } => {
            let mut page = search_page_json("demo", None, "analyzer-v1", "first");
            page["items"][0]["source"] = serde_json::json!("x".repeat(*source_bytes));
            page
        }
        OperationFixture::AdditiveResponse => {
            let mut page = search_page_json("demo", None, "analyzer-v2", "first");
            page["items"][0]["symbol"]["future_field"] =
                serde_json::json!("ignored by this client");
            page
        }
        OperationFixture::PageRevisionChange => search_page_json(
            "demo",
            Some("next"),
            if cursor.is_some() {
                "analyzer-v2"
            } else {
                "analyzer-v1"
            },
            if cursor.is_some() { "second" } else { "first" },
        ),
        OperationFixture::MismatchedCursor => mismatched_cursor_page(query),
        OperationFixture::BodyBoundStop => pages::body_bound_page(
            search_page_json("demo", Some("next"), "analyzer-v2", "first"),
            search_page_json("demo", None, "analyzer-v2", "second"),
            cursor,
        ),
        OperationFixture::SearchPages
        | OperationFixture::CandidateBound
        | OperationFixture::PartialFailure
        | OperationFixture::RepeatedCursor => paged_search_response(mode, cursor),
        _ => search_page_json("demo", None, "analyzer-v2", "first"),
    };
    json_response(&body, None)
}

fn paged_search_response(mode: &OperationFixture, cursor: Option<&str>) -> serde_json::Value {
    if cursor.is_none() {
        search_page_json("demo", Some("next"), "analyzer-v2", "first")
    } else if matches!(mode, OperationFixture::RepeatedCursor) {
        search_page_json("demo", Some("next"), "analyzer-v2", "second")
    } else {
        search_page_json("demo", None, "analyzer-v2", "second")
    }
}

fn operation_symbol_response(mode: &OperationFixture, _query: Option<&str>) -> Response {
    let value = match mode {
        OperationFixture::Documentation { symbols, .. }
        | OperationFixture::DocumentationBounds { symbols, .. } => symbols.clone(),
        _ => symbol_result_json("demo", "first"),
    };
    json_response(&value, None)
}

fn query_cursor(query: Option<&str>) -> Option<&str> {
    query.and_then(|value| {
        value
            .split('&')
            .find_map(|item| item.strip_prefix("cursor="))
    })
}

fn mismatched_cursor_page(query: Option<&str>) -> serde_json::Value {
    if query.is_some_and(|value| value.contains("cursor=next")) {
        search_page_json("demo", None, "analyzer-v2", "first")
    } else {
        search_page_json("demo", Some("next"), "analyzer-v2", "first")
    }
}

fn capabilities_response_with(mut mutate: impl FnMut(&mut serde_json::Value)) -> Response {
    let mut value = serde_json::from_str::<serde_json::Value>(&capabilities_json())
        .unwrap_or_else(|_| serde_json::json!({}));
    mutate(&mut value);
    json_response(&value, Some("max-age=60"))
}

fn json_response(value: &serde_json::Value, cache_control: Option<&str>) -> Response {
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(value.to_string()),
    )
        .into_response();
    if let Some(cache_control) = cache_control {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_str(cache_control)
                .unwrap_or_else(|_| HeaderValue::from_static("no-cache")),
        );
    }
    response
}

fn problem_response_with_retry(status: StatusCode) -> Response {
    let mut response = problem_response(status);
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("0"));
    response
}

fn status_response(status: StatusCode) -> Response {
    (status, Body::empty()).into_response()
}

fn problem_json(status: StatusCode) -> String {
    serde_json::json!({
        "type": format!("https://api.volar.sh/rift/problems/{}", status.as_u16()),
        "title": status.canonical_reason().unwrap_or("HTTP error"),
        "status": status.as_u16(),
        "detail": "fixture problem"
    })
    .to_string()
}

fn problem_response(status: StatusCode) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/problem+json")],
        Body::from(problem_json(status)),
    )
        .into_response()
}

fn package_json(name: &str) -> serde_json::Value {
    serde_json::json!({"manager":"cargo","registry":"crates.io","name":name,"version":"1.0.0"})
}

fn symbol_json(package: &str, suffix: &str) -> serde_json::Value {
    serde_json::json!({
        "id":format!("rift://symbol/rust/cargo/{package}@1.0.0/src/{suffix}.rs/demo"),
        "kind":"function",
        "language":"rust",
        "name":"demo",
        "origin":{"location":"dependency","package":package_json(package),"source_kind":"authored"}
    })
}

fn search_hit_json(package: &str, suffix: &str) -> serde_json::Value {
    serde_json::json!({
        "package":package_json(package), "symbol":symbol_json(package, suffix),
        "unit":format!("rift://source/cargo/crates.io/{package}@1.0.0/src/{suffix}.rs"), "range":{"start":0,"end":4}, "line":1,
        "contributing_fields":["qualified_name"], "match_class":"qualified_exact"
    })
}

fn search_page_json(
    package: &str,
    next: Option<&str>,
    analyzer: &str,
    suffix: &str,
) -> serde_json::Value {
    serde_json::json!({
        "items":[search_hit_json(package, suffix)], "next_cursor":next,
        "warnings":[], "publication_format":"rift-package-index-v2",
        "analyzer_revision":analyzer, "corpus_revision":"corpus-v1",
        "view":{"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"}
    })
}

fn invalid_search_page(mode: &OperationFixture) -> serde_json::Value {
    let mut page = search_page_json("demo", None, "analyzer-v1", "first");
    if matches!(mode, OperationFixture::InvalidOrigin) {
        page["items"][0]["symbol"]["origin"]["location"] = serde_json::json!("project");
    } else if matches!(mode, OperationFixture::InvalidSource) {
        page["items"][0]["source"] = serde_json::json!("unexpected source");
    } else if matches!(mode, OperationFixture::InvalidSourceIdentity) {
        page["items"][0]["unit"] =
            serde_json::json!("rift://source/cargo/crates.io/other@1.0.0/src/first.rs");
    } else {
        page["items"][0]["match_class"] = serde_json::json!("substring");
    }
    page
}

fn capabilities_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::ETAG, "v1"),
            (header::CACHE_CONTROL, "max-age=0"),
        ],
        Body::from(capabilities_json()),
    )
        .into_response()
}

fn capabilities_json() -> String {
    serde_json::json!({
        "supported_package_managers": ["cargo"],
        "supported_features": ["resolutions", "search", "symbols"],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "required_search_fields": [
            "name",
            "qualified_name",
            "documentation",
            "signature"
        ],
        "bounds": {
            "request_body_bytes_max": REQUEST_BODY_BYTES_MAX,
            "response_body_bytes_max": RESPONSE_BODY_BYTES_MAX,
            "dependency_entries_max": DEPENDENCY_ENTRIES_MAX,
            "query_bytes_max": QUERY_BYTES_MAX,
            "query_terms_max": QUERY_TERMS_MAX,
            "query_term_bytes_max": QUERY_TERM_BYTES_MAX,
            "identifiers_max": IDENTIFIERS_MAX,
            "identifier_bytes_max": IDENTIFIER_BYTES_MAX,
            "packages_max": PACKAGES_MAX,
            "page_limit_min": PAGE_LIMIT_MIN,
            "page_limit_max": PAGE_LIMIT_MAX,
            "page_limit_default": 20,
            "cursor_bytes_max": CURSOR_BYTES_MAX,
            "candidate_pool_max": CANDIDATE_POOL_MAX,
            "warnings_max": WARNINGS_MAX,
            "source_bytes_max": SOURCE_BYTES_MAX
        }
    })
    .to_string()
}

#[test]
fn test_config_rejects_non_loopback_http() {
    let config = Config {
        endpoint: "http://example.test/rift".to_owned(),
        ..Config::default()
    };
    let error = GlobalClient::new(config).err().expect("endpoint refusal");
    assert_registered_field(&error, "rift.client.configuration_invalid", "endpoint");
}

#[test]
fn test_config_accepts_loopback_http() {
    let config = Config {
        endpoint: "http://127.0.0.1:4321/rift".to_owned(),
        ..Config::default()
    };
    assert!(GlobalClient::new(config).is_ok());
}

#[test]
fn test_config_rejects_invalid_endpoint_shapes() {
    let endpoints = [
        "not a URL",
        "https://user@example.test/rift",
        "https://:secret@example.test/rift",
        "https://example.test/rift?x=1",
        "https://example.test/rift#part",
        "https://example.test/rift/",
    ];
    for endpoint in endpoints {
        let config = Config {
            endpoint: endpoint.to_owned(),
            ..Config::default()
        };
        let error = GlobalClient::new(config).err().expect("endpoint refusal");
        assert_registered_field(&error, "rift.client.configuration_invalid", "endpoint");
    }
}

#[test]
fn test_protocol_configuration_converts_without_drift() {
    let source = rift_protocol::configuration::GlobalConfiguration::default();
    let converted = Config::try_from(&source);
    assert_eq!(
        converted.expect("protocol configuration"),
        Config::default()
    );
}

#[test]
#[expect(
    clippy::type_complexity,
    reason = "configuration bound cases stay together in one test table"
)]
fn test_config_rejects_every_out_of_range_setting_and_token_name() {
    let cases: [(&str, fn(&mut Config)); 7] = [
        ("connect_timeout", |value| {
            value.connect_timeout = Duration::from_millis(99);
        }),
        ("request_timeout", |value| {
            value.request_timeout = Duration::from_millis(999);
        }),
        ("attempts", |value| value.attempts = 0),
        ("max_in_flight", |value| value.max_in_flight = 0),
        ("capabilities_ttl", |value| {
            value.capabilities_ttl = Duration::from_secs(59);
        }),
        ("resolution_ttl", |value| {
            value.resolution_ttl = Duration::from_hours(25);
        }),
        ("failure_ttl", |value| {
            value.failure_ttl = Duration::from_millis(999);
        }),
    ];
    for (field, change) in cases {
        let mut config = Config::default();
        change(&mut config);
        let error = GlobalClient::new(config)
            .err()
            .expect("configuration bound");
        assert_registered_field(&error, "rift.client.configuration_invalid", field);
    }

    for token_env in ["", "RIFT-TOKEN"] {
        let config = Config {
            token_env: token_env.to_owned(),
            ..Config::default()
        };
        let error = GlobalClient::new(config)
            .err()
            .expect("token configuration");
        assert_registered_field(&error, "rift.client.configuration_invalid", "token_env");
    }
}

#[test]
fn test_client_errors_use_bounded_messages() {
    let cases = [
        (
            failure::configuration_invalid("enabled", "global client disabled"),
            "configuration_invalid",
            "violation",
            "global client disabled",
        ),
        (
            failure::configuration_invalid("token", "global credential configuration invalid"),
            "configuration_invalid",
            "violation",
            "global credential configuration invalid",
        ),
        (
            failure::body_limit("fixture", "request", 5, 4),
            "body_limit",
            "bytes",
            "5",
        ),
        (
            failure::body_limit("fixture", "response", 6, 5),
            "body_limit",
            "bytes",
            "6",
        ),
        (
            failure::request_failed("fixture", "global request deadline elapsed"),
            "request_failed",
            "reason",
            "global request deadline elapsed",
        ),
        (
            failure::request_cancelled("fixture"),
            "request_cancelled",
            "operation",
            "fixture",
        ),
        (
            failure::request_failed("fixture", "global endpoint connection failed"),
            "request_failed",
            "reason",
            "global endpoint connection failed",
        ),
    ];
    for (error, slug, field, expected) in cases {
        assert_eq!(error.slug().as_str(), format!("rift.client.{slug}"));
        assert_eq!(error_text_field(&error, field), expected);
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn test_client_response_errors_use_bounded_messages() {
    let meta = ResponseMeta {
        status: 503,
        content_type: None,
        etag: None,
        cache_control: None,
        www_authenticate: None,
        retry_after: None,
        rate_limit_limit: None,
        rate_limit_remaining: None,
        rate_limit_reset: None,
    };
    let decode = serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON");
    let cases = [
        (
            failure::response_invalid(
                "fixture",
                "content_type",
                "invalid response media type: Some(\"text/plain\")",
                Some(200),
            ),
            "response_invalid",
            "violation",
            "invalid response media type: Some(\"text/plain\")",
        ),
        (
            failure::response_invalid(
                "fixture",
                "status",
                "unsupported global response status",
                Some(418),
            ),
            "response_invalid",
            "status",
            "418",
        ),
        (
            failure::response_source(
                "fixture",
                "body",
                "global response decode failed",
                Some(200),
                decode,
            ),
            "response_invalid",
            "violation",
            "global response decode failed",
        ),
        (
            failure::request_invalid("fixture", "query", "global request violates bound"),
            "request_invalid",
            "field",
            "query",
        ),
        (
            failure::response_invalid(
                "fixture",
                "query",
                "global response violates contract",
                None,
            ),
            "response_invalid",
            "field",
            "query",
        ),
        (
            failure::configuration_invalid(
                "feature",
                "global API does not advertise feature: patterns",
            ),
            "configuration_invalid",
            "violation",
            "global API does not advertise feature: patterns",
        ),
        (
            failure::remote("fixture", &meta, None),
            "remote_unavailable",
            "status",
            "503",
        ),
    ];
    for (error, slug, field, expected) in cases {
        assert_eq!(error.slug().as_str(), format!("rift.client.{slug}"));
        assert_eq!(error_text_field(&error, field), expected);
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn test_request_relationships_are_validated_before_transport() {
    let mut resolution = resolution_request();
    resolution.entries[0].availability = PackageAvailability::Path;
    assert_registered_field(
        &(validate_resolution_request(&resolution)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "availability",
    );
    let mut resolution = resolution_request();
    resolution.entries[0].version = Some("1.0.0".to_owned());
    assert_registered_field(
        &(validate_resolution_request(&resolution)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "version_or_requirement",
    );
    let mut resolution = resolution_request();
    resolution.entries.push(resolution.entries[0].clone());
    assert_registered_field(
        &(validate_resolution_request(&resolution)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "duplicate_entry",
    );

    let mut search = search_request();
    search.terms.as_mut().expect("lexical terms")[0].phrase = true;
    search.terms.as_mut().expect("lexical terms")[0].prefix = true;
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "phrase_prefix",
    );
    let mut search = search_request();
    search.terms.as_mut().expect("lexical terms")[0].text = "ab".to_owned();
    search.terms.as_mut().expect("lexical terms")[0].prefix = true;
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "prefix",
    );
    let mut search = search_request();
    search.include = Some(vec!["score".to_owned()]);
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "include",
    );
    let mut search = search_request();
    search.phase = Some(PackageSearchRequestPhase::Broad);
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "broad_phase",
    );
    let mut search = search_request();
    search.packages = Some(vec![package_request(), package_request()]);
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "duplicate_package",
    );
    assert_registered_field(
        &(validate_page(0, None)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "limit",
    );
    assert_registered_field(
        &(validate_page(1, Some(""))).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "cursor",
    );
}

#[test]
fn test_disabled_client_makes_no_request() {
    let config = Config {
        enabled: false,
        ..Config::default()
    };
    let client = GlobalClient::new(config).expect("disabled client construction");
    let runtime = tokio::runtime::Runtime::new().expect("test runtime construction");
    let result = runtime.block_on(client.get_capabilities());
    let error = result.expect_err("disabled global access");
    assert_registered_field(&error, "rift.client.configuration_invalid", "enabled");
    assert_eq!(
        error_text_field(&error, "violation"),
        "global client disabled"
    );
}

#[test]
fn test_generated_requests_serialize_only_declared_fields() {
    let value = serde_json::to_value(search_request()).expect("search request serialization");
    assert!(value.get("query").is_some());
    assert!(value.get("target").is_none());
    assert!(value.get("unknown").is_none());
}

#[tokio::test]
async fn test_documentation_request_requires_advertised_capability() {
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    let mut request = search_request();
    request.target = Some(PackageSearchRequestTarget::Documentation);
    let error = validate_search_request_for_capabilities(&request, 20, None, &capabilities)
        .expect_err("documentation capability");
    assert_registered_field(&error, "rift.client.configuration_invalid", "feature");
    assert!(error_text_field(&error, "violation").contains("documentation_search"));

    capabilities
        .supported_features
        .push("documentation_search".to_owned());
    capabilities.documentation_revision = Some("0123abcd".to_owned());
    validate_capabilities(&capabilities).expect("valid documentation capability");
    assert!(validate_search_request_for_capabilities(&request, 20, None, &capabilities).is_ok());

    let mut symbol = symbol_request();
    symbol.include = vec![GetSymbolInclude::Documentation];
    let (server, client) = operation_client(OperationFixture::Valid).await;
    let error = client
        .get_symbol(&symbol)
        .await
        .expect_err("documentation capability");
    assert_registered_field(&error, "rift.client.configuration_invalid", "feature");
    assert!(error_text_field(&error, "violation").contains("symbol_documentation"));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
    assert!(
        server
            .state
            .request_log
            .lock()
            .await
            .iter()
            .all(|sent| sent.method != Method::POST)
    );
    assert!(
        canonical_symbol_response(
            symbol_result_json("demo", "first"),
            &symbol,
            SOURCE_BYTES_MAX
        )
        .await
        .is_ok()
    );
}

#[test]
fn test_generated_unit_enums_use_wire_strings() {
    assert_eq!(
        serde_json::to_string(&PackageAvailability::Canonical).ok(),
        Some("\"canonical\"".to_owned())
    );
    assert_eq!(
        serde_json::to_string(&PackageSearchRequestPhase::Broad).ok(),
        Some("\"broad\"".to_owned())
    );
    assert_eq!(
        serde_json::from_str::<SourceKind>("\"generated\"").ok(),
        Some(SourceKind::Generated)
    );
}

#[test]
fn test_generated_response_status_preserves_unknown_values() {
    assert!(matches!(
        GetCapabilitiesResponse::Unknown,
        GetCapabilitiesResponse::Unknown
    ));
}

#[test]
fn test_generated_values_allow_response_additions_and_reject_request_additions() {
    let mut value = serde_json::from_str::<serde_json::Value>(&capabilities_json())
        .expect("capabilities fixture JSON");
    value["future_field"] = serde_json::json!("ignored by this client");
    let capabilities =
        serde_json::from_value::<Capabilities>(value).expect("additive response field must decode");
    assert_eq!(
        capabilities.additional_properties.get("future_field"),
        Some(&serde_json::json!("ignored by this client"))
    );
    assert_eq!(
        serde_json::from_value::<PublicationFormat>(serde_json::json!("future-format"))
            .expect("extensible response enum must decode"),
        PublicationFormat::Unknown
    );

    let mut request = serde_json::json!({
        "availability": "canonical",
        "manager": "cargo",
        "registry": "crates.io",
        "name": "demo",
        "requirement": "^1",
        "version": null,
        "future_field": true
    });
    assert!(serde_json::from_value::<PackageContextEntry>(request.clone()).is_err());
    request["future_field"] = serde_json::Value::Null;
    request["availability"] = serde_json::json!("future-availability");
    assert!(serde_json::from_value::<PackageContextEntry>(request).is_err());
}

#[tokio::test]
async fn test_fixture_accepts_additive_capability_field() {
    let (_server, client) = operation_client(OperationFixture::AdditiveResponse).await;
    let capabilities = client
        .get_capabilities()
        .await
        .expect("additive capabilities response");
    assert_eq!(
        capabilities.additional_properties.get("future_field"),
        Some(&serde_json::json!("ignored by this client"))
    );
}

#[tokio::test]
async fn test_fixture_accepts_additive_nested_symbol_field() {
    let (_server, client) = operation_client(OperationFixture::AdditiveResponse).await;
    let page = client
        .search_packages(&search_request(), 20, None)
        .await
        .expect("additive symbol field");
    assert_eq!(page.items.len(), 1);
    let PackageSearchItem::Search(hit) = &page.items[0] else {
        panic!("legacy search item must remain symbol hit");
    };
    assert_eq!(hit.symbol.name, "demo");
}

#[test]
fn test_serialize_body_rejects_oversized_request() {
    let value = "x".repeat(REQUEST_BODY_BYTES_MAX + 1);
    let error = serialize_body(&value, REQUEST_BODY_BYTES_MAX).expect_err("request body bound");
    assert_eq!(error.slug().as_str(), "rift.client.body_limit");
    assert_eq!(error_text_field(&error, "direction"), "request");
    assert_eq!(
        error_text_field(&error, "maximum")
            .parse::<usize>()
            .expect("body maximum"),
        REQUEST_BODY_BYTES_MAX
    );
    assert!(
        error_text_field(&error, "bytes")
            .parse::<usize>()
            .expect("body bytes")
            > REQUEST_BODY_BYTES_MAX
    );
}

#[test]
fn test_retry_policy_is_bounded_by_status() {
    assert!(should_retry(StatusCode::TOO_MANY_REQUESTS, 1, 3));
    assert!(should_retry(StatusCode::SERVICE_UNAVAILABLE, 2, 3));
    assert!(should_retry(StatusCode::GATEWAY_TIMEOUT, 1, 3));
    assert!(!should_retry(StatusCode::GATEWAY_TIMEOUT, 2, 3));
    assert!(!should_retry(StatusCode::INTERNAL_SERVER_ERROR, 1, 3));
}

#[test]
fn test_url_join_preserves_endpoint_path_and_encodes_query() {
    let endpoint = Url::parse("http://127.0.0.1:4321/rift/rest").expect("fixture endpoint URL");
    let mut request = reqwest::Client::new().get(make_url(&endpoint, "/v1/search"));
    request = request.query(&SearchPackagesRequestQuery {
        limit: Some(10),
        cursor: Some("a/b c?d".to_owned()),
    });
    let url = request.build().expect("typed query builds").url().clone();
    assert_eq!(
        url.as_str(),
        "http://127.0.0.1:4321/rift/rest/v1/search?limit=10&cursor=a%2Fb+c%3Fd"
    );
}

#[test]
fn test_token_never_enters_error_text() {
    for token in [String::new(), "two words".to_owned()] {
        let error = validate_token(token).expect_err("invalid authorization token");
        assert_registered_field(&error, "rift.client.configuration_invalid", "token");
    }
    let error = validate_token("secret token".to_owned()).expect_err("invalid authorization token");
    assert_registered_field(&error, "rift.client.configuration_invalid", "token");
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn test_token_construction_cases_do_not_touch_environment() {
    let config = Config {
        token_env: "RIFT_CLOUD_CLIENT_TEST_TOKEN_CASES".to_owned(),
        ..Config::default()
    };
    assert!(GlobalClient::new_with_token(config.clone(), None).is_ok());
    for token in [String::new(), "token with spaces".to_owned()] {
        let error = GlobalClient::new_with_token(config.clone(), Some(token))
            .err()
            .expect("invalid authorization token");
        assert_registered_field(&error, "rift.client.configuration_invalid", "token");
    }
}

#[tokio::test]
async fn test_fixture_sends_valid_authorization_header() {
    let server = FixtureServer::start(FixtureMode::Capabilities)
        .await
        .expect("fixture server");
    let client = GlobalClient::new_with_token(server.config(), Some("secret".to_owned()))
        .expect("fixture client");
    assert!(client.get_capabilities().await.is_ok());
    assert_eq!(
        server.state.last_authorization.lock().await.as_deref(),
        Some("Bearer secret")
    );
}

#[tokio::test]
async fn test_fixture_capabilities_cache_headers_and_anonymous_auth() {
    let server = FixtureServer::start(FixtureMode::Capabilities)
        .await
        .expect("fixture server");
    let mut config = server.config();
    config.token_env = "RIFT_CLOUD_CLIENT_TEST_TOKEN_UNSET".to_owned();
    let client = GlobalClient::new(config).expect("fixture client");
    let first = client.get_capabilities().await;
    let second = client.get_capabilities().await;
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        server.state.last_authorization.lock().await.as_deref(),
        None
    );
    assert!(
        server
            .state
            .last_accept
            .lock()
            .await
            .as_deref()
            .is_some_and(|value| value.contains("application/json"))
    );
    assert_eq!(
        server.state.last_if_none_match.lock().await.as_deref(),
        Some("v1")
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/capabilities")
    );
}

#[tokio::test]
async fn test_fixture_new_clients_replace_capability_and_credential_cache_state() {
    let first_server = FixtureServer::start(FixtureMode::Capabilities)
        .await
        .expect("first fixture server");
    let first_client =
        GlobalClient::new_with_token(first_server.config(), Some("first-secret".to_owned()))
            .expect("first fixture client");
    assert!(first_client.get_capabilities().await.is_ok());
    assert_eq!(first_server.state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        first_server
            .state
            .last_authorization
            .lock()
            .await
            .as_deref(),
        Some("Bearer first-secret")
    );

    let second_server = FixtureServer::start(FixtureMode::Capabilities)
        .await
        .expect("second fixture server");
    let second_client =
        GlobalClient::new_with_token(second_server.config(), Some("second-secret".to_owned()))
            .expect("second fixture client");
    assert!(second_client.get_capabilities().await.is_ok());
    assert_eq!(second_server.state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        second_server
            .state
            .last_authorization
            .lock()
            .await
            .as_deref(),
        Some("Bearer second-secret")
    );
}

#[tokio::test]
async fn test_fixture_redirect_refused() {
    let server = FixtureServer::start(FixtureMode::Redirect)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let result = client.get_capabilities().await;
    let error = result.expect_err("redirect refusal");
    assert_eq!(error.slug().as_str(), "rift.client.remote_failed");
    assert_eq!(error_text_field(&error, "status"), "307");
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_fixture_media_and_json_errors() {
    let server = FixtureServer::start(FixtureMode::WrongMedia)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let error = client.get_capabilities().await.expect_err("media refusal");
    assert_registered_field(&error, "rift.client.response_invalid", "content_type");
    assert_eq!(error_text_field(&error, "status"), "200");
    assert!(error_text_field(&error, "violation").contains("text/plain"));
    drop(client);

    let server = FixtureServer::start(FixtureMode::MalformedJson)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let error = client.get_capabilities().await.expect_err("JSON refusal");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
    assert_eq!(error_text_field(&error, "status"), "200");
    assert!(
        error
            .fields()
            .iter()
            .any(|field| matches!(field.value(), rift_error::ErrorValue::Source(_)))
    );
}

#[tokio::test]
async fn test_fixture_unauthorized_retains_challenge() {
    let server = FixtureServer::start(FixtureMode::Unauthorized)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let result = client.get_capabilities().await;
    let error = result.expect_err("unauthorized HTTP refusal");
    assert_eq!(
        error.slug().as_str(),
        "rift.client.remote_permission_denied"
    );
    let meta = remote_metadata(&error);
    assert_eq!(meta["status"], 401);
    assert_eq!(meta["www_authenticate"], "Bearer realm=rift");
}

#[tokio::test]
async fn test_fixture_retry_statuses_and_headers() {
    let server = FixtureServer::start(FixtureMode::StatusSequence(vec![
        StatusCode::TOO_MANY_REQUESTS,
    ]))
    .await
    .expect("fixture server");
    let client = GlobalClient::new(Config {
        attempts: 1,
        ..server.config()
    })
    .expect("fixture client");
    let result = client.get_capabilities().await;
    let error = result.expect_err("rate limit HTTP refusal");
    assert_eq!(error.slug().as_str(), "rift.client.remote_unavailable");
    let meta = remote_metadata(&error);
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(meta["status"], 429);
    assert_eq!(meta["retry_after"], "0");
    assert_eq!(meta["rate_limit_limit"], "10");
    assert_eq!(meta["rate_limit_remaining"], "0");
    assert_eq!(meta["rate_limit_reset"], "60");
    drop(client);

    let server = FixtureServer::start(FixtureMode::StatusSequence(vec![
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::OK,
    ]))
    .await
    .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert!(client.get_capabilities().await.is_ok());
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
    drop(client);

    let server = FixtureServer::start(FixtureMode::StatusSequence(vec![
        StatusCode::GATEWAY_TIMEOUT,
        StatusCode::GATEWAY_TIMEOUT,
        StatusCode::OK,
    ]))
    .await
    .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert!(matches!(
        client.get_capabilities().await,
        Err(ref error) if error.slug().as_str() == "rift.client.remote_unavailable"
            && error_text_field(error, "status") == "504"
    ));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_retry_repeats_method_query_and_body() {
    let (server, client) = operation_client(OperationFixture::RetrySearch).await;
    client
        .search_packages(&search_request(), 20, Some("page/2"))
        .await
        .expect("retried search response");

    let requests = server.state.request_log.lock().await;
    assert_eq!(requests.len(), 3);
    let first = &requests[1];
    let retry = &requests[2];
    assert_eq!(first.method, Method::POST);
    assert_eq!(first.path, "/rift/rest/v1/search");
    assert_eq!(first.method, retry.method);
    assert_eq!(first.path, retry.path);
    assert_eq!(first.query, retry.query);
    assert_eq!(first.body, retry.body);
}

#[tokio::test]
async fn test_fixture_500_and_502_do_not_retry() {
    for status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::BAD_GATEWAY] {
        let server = FixtureServer::start(FixtureMode::StatusSequence(vec![status]))
            .await
            .expect("fixture server");
        let client = GlobalClient::new(server.config()).expect("fixture client");
        assert!(client.get_capabilities().await.is_err());
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn test_fixture_documented_problem_statuses_decode() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_ACCEPTABLE,
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
    ] {
        let (server, client) = operation_client(OperationFixture::Problem(status)).await;
        let result = client.search_packages(&search_request(), 20, None).await;
        let error = result.expect_err("documented HTTP refusal");
        let expected_slug = if status == StatusCode::FORBIDDEN {
            "rift.client.remote_permission_denied"
        } else {
            "rift.client.remote_failed"
        };
        assert_eq!(error.slug().as_str(), expected_slug);
        assert_eq!(remote_metadata(&error)["status"], status.as_u16());
        let problem: serde_json::Value = serde_json::from_str(error_text_field(&error, "problem"))
            .expect("valid retained problem details");
        assert_eq!(problem["status"], i64::from(status.as_u16()));
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
    }
}

/// The page limit `operation_failure` asks for, within every fixture's `page_limit_max`.
const OPERATION_PAGE_LIMIT: i64 = 2;

/// Sends one request to `endpoint` and keeps the failure it answers.
async fn operation_failure(
    client: &GlobalClient,
    endpoint: crate::contract::Endpoint,
) -> Option<RiftError> {
    use crate::contract::Endpoint;
    match endpoint {
        Endpoint::Capabilities => client.get_capabilities().await.err(),
        Endpoint::Resolutions => client
            .resolve_package_context(&resolution_request())
            .await
            .err(),
        Endpoint::Search => client
            .search_packages(&search_request(), OPERATION_PAGE_LIMIT, None)
            .await
            .err(),
        Endpoint::Symbols => client.get_symbol(&symbol_request()).await.err(),
        Endpoint::Nodes => client.nodes(&nodes::nodes_request()).await.err(),
        Endpoint::Patterns => client
            .search_package_patterns(&pattern::pattern_request(), OPERATION_PAGE_LIMIT, None)
            .await
            .err(),
        Endpoint::Declarations => client
            .find_declarations(&declaration::declaration_request())
            .await
            .err(),
    }
}

/// The registered failure retains problem details only for a documented status.
fn fixture_problem(status: StatusCode, documented: bool) -> RiftError {
    let problem = serde_json::from_str::<ProblemDetails>(&problem_json(status)).ok();
    failure::remote(
        "fixture_problem",
        &ResponseMeta {
            status: status.as_u16(),
            content_type: Some("application/problem+json".to_owned()),
            etag: None,
            cache_control: None,
            www_authenticate: None,
            retry_after: None,
            rate_limit_limit: None,
            rate_limit_remaining: None,
            rate_limit_reset: None,
        },
        problem.filter(|_| documented).as_ref(),
    )
}

/// Every problem status an operation's contract documents, each decoded into its own
/// response variant.
const DOCUMENTED_PROBLEM_STATUSES: [StatusCode; 11] = [
    StatusCode::BAD_REQUEST,
    StatusCode::UNAUTHORIZED,
    StatusCode::FORBIDDEN,
    StatusCode::NOT_ACCEPTABLE,
    StatusCode::PAYLOAD_TOO_LARGE,
    StatusCode::UNSUPPORTED_MEDIA_TYPE,
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
];

/// Every operation answers a problem status as `Http`, with the problem details a documented
/// status carries and without them for an undocumented one. The operation records the
/// failure, so a later request answers it without reaching the endpoint. One attempt per
/// request keeps the retried statuses to one request each.
#[tokio::test]
async fn test_fixture_every_operation_answers_problem_statuses() {
    use crate::contract::Endpoint;
    let operations = [
        Endpoint::Resolutions,
        Endpoint::Search,
        Endpoint::Symbols,
        Endpoint::Nodes,
        Endpoint::Patterns,
        Endpoint::Declarations,
    ];
    let statuses = DOCUMENTED_PROBLEM_STATUSES
        .map(|status| (status, true))
        .into_iter()
        .chain([(StatusCode::CONFLICT, false)]);
    for (status, documented) in statuses {
        for endpoint in operations {
            let mode = FixtureMode::Operations(OperationFixture::Problem(status));
            let server = FixtureServer::start(mode).await.expect("fixture server");
            let config = Config {
                attempts: 1,
                ..server.config()
            };
            let client = GlobalClient::new(config).expect("fixture client");
            let expected = fixture_problem(status, documented);
            let actual = operation_failure(&client, endpoint)
                .await
                .expect("HTTP refusal");
            assert_remote_error(&actual, &expected);
            assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
            let cached = operation_failure(&client, Endpoint::Capabilities)
                .await
                .expect("recorded HTTP refusal");
            assert_remote_error(&cached, &expected);
            assert_eq!(cached.to_string(), actual.to_string());
            assert_eq!(
                server.state.requests.load(Ordering::SeqCst),
                2,
                "the recorded failure answers without a request"
            );
        }
    }
}

#[tokio::test]
async fn test_fixture_retry_after_beyond_deadline_does_not_repeat() {
    let server = FixtureServer::start(FixtureMode::RetryAfter {
        status: StatusCode::TOO_MANY_REQUESTS,
        value: "3600",
    })
    .await
    .expect("fixture server");
    let client = GlobalClient::new(Config {
        attempts: 3,
        request_timeout: Duration::from_secs(1),
        ..server.config()
    })
    .expect("fixture client");
    let result = client.get_capabilities().await;
    let error = result.expect_err("rate limit HTTP refusal");
    assert_eq!(error.slug().as_str(), "rift.client.remote_unavailable");
    let meta = remote_metadata(&error);
    assert_eq!(meta["status"], 429);
    assert_eq!(meta["retry_after"], "3600");
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

/// Connect bound under which a refused loopback port still answers as refused.
///
/// Windows answers a refused connect only after it resends the SYN, which takes about
/// two seconds on loopback; a shorter bound reports the refusal as a deadline there.
const REFUSAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Request bound above [`REFUSAL_CONNECT_TIMEOUT`], so the connect decides the answer.
const REFUSAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[tokio::test]
async fn test_fixture_connection_refusal_is_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("unused listener");
    let address = listener.local_addr().expect("unused listener address");
    drop(listener);
    let client = GlobalClient::new(Config {
        endpoint: format!("http://{address}/rift/rest"),
        attempts: 1,
        connect_timeout: REFUSAL_CONNECT_TIMEOUT,
        request_timeout: REFUSAL_REQUEST_TIMEOUT,
        ..Config::default()
    })
    .expect("fixture client");
    let error = client
        .get_capabilities()
        .await
        .expect_err("connection refusal");
    assert_eq!(error.slug().as_str(), "rift.client.request_failed");
    assert_eq!(
        error_text_field(&error, "reason"),
        "global endpoint connection failed"
    );
    assert!(
        error
            .fields()
            .iter()
            .any(|field| matches!(field.value(), rift_error::ErrorValue::Source(_)))
    );
}

#[tokio::test]
async fn test_fixture_tls_handshake_timeout_is_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("TLS stall listener");
    let address = listener.local_addr().expect("TLS stall listener address");
    let task = tokio::spawn(async move {
        let (_stream, _address) = listener.accept().await.expect("TLS stall accept");
        std::future::pending::<()>().await;
    });
    let client = GlobalClient::new(Config {
        endpoint: format!("https://{address}/rift/rest"),
        attempts: 1,
        connect_timeout: Duration::from_millis(100),
        request_timeout: Duration::from_secs(1),
        ..Config::default()
    })
    .expect("TLS stall client");
    let error = client
        .get_capabilities()
        .await
        .expect_err("deadline refusal");
    assert_eq!(error.slug().as_str(), "rift.client.request_failed");
    assert_eq!(
        error_text_field(&error, "reason"),
        "global request deadline elapsed"
    );
    assert!(
        error
            .fields()
            .iter()
            .any(|field| matches!(field.value(), rift_error::ErrorValue::Source(_)))
    );
    task.abort();
}

#[tokio::test]
async fn test_fixture_content_length_bound() {
    let server = FixtureServer::start(FixtureMode::Oversize)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert!(matches!(
        client.get_capabilities().await,
        Err(ref error) if error.slug().as_str() == "rift.client.body_limit"
            && error_text_field(error, "direction") == "response"
            && error_text_field(error, "bytes").parse::<usize>().expect("body bytes") > RESPONSE_BODY_BYTES_MAX
    ));
}

#[tokio::test]
async fn test_fixture_total_deadline_and_cancellation() {
    let server = FixtureServer::start(FixtureMode::Delay(Duration::from_secs(2)))
        .await
        .expect("fixture server");
    let client = GlobalClient::new(Config {
        request_timeout: Duration::from_secs(1),
        ..server.config()
    })
    .expect("fixture client");
    let error = client
        .get_capabilities()
        .await
        .expect_err("deadline refusal");
    assert_eq!(error.slug().as_str(), "rift.client.request_failed");
    assert_eq!(
        error_text_field(&error, "reason"),
        "global request deadline elapsed"
    );
    assert!(
        error
            .fields()
            .iter()
            .any(|field| matches!(field.value(), rift_error::ErrorValue::Source(_)))
    );
    drop(client);

    let server = FixtureServer::start(FixtureMode::Delay(Duration::from_secs(2)))
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let task = tokio::spawn(async move { client.get_capabilities().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    assert!(task.await.is_err());
}

#[tokio::test]
async fn test_fixture_capabilities_single_flight() {
    let server = FixtureServer::start(FixtureMode::CapabilitiesDelay(Duration::from_millis(50)))
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let first = {
        let client = client.clone();
        tokio::spawn(async move { client.get_capabilities().await })
    };
    let second = {
        let client = client.clone();
        tokio::spawn(async move { client.get_capabilities().await })
    };
    let third = {
        let client = client.clone();
        tokio::spawn(async move { client.get_capabilities().await })
    };
    assert!(first.await.is_ok());
    assert!(second.await.is_ok());
    assert!(third.await.is_ok());
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_fixture_capabilities_flight_records_the_wait_and_its_holder() {
    let server = FixtureServer::start(FixtureMode::Capabilities)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let holding =
        rift_tracing::traced!(component = "global", operation = "global.request", async {
            rift_tracing::lock(CAPABILITIES_FLIGHT_LOCK)
                .acquire(client.inner.capabilities_flight.lock())
                .await
        })
        .await;
    let mut asking = Box::pin(client.get_capabilities());
    let waiting = tokio::select! {
        biased;
        _ = &mut asking => false,
        () = std::future::ready(()) => true,
    };
    assert!(waiting, "a second caller waits for the flight");
    drop(holding);
    assert!(asking.await.is_ok());
    let metrics = recorder.metrics();
    drop(recorder);

    let flight = [
        ("lock.name", CAPABILITIES_FLIGHT_LOCK),
        ("lock.mode", "exclusive"),
    ];
    for name in ["lock.wait.duration", "lock.held.duration"] {
        let count = match metrics
            .find(name, &flight)
            .map(rift_tracing::MetricSeries::value)
        {
            Some(rift_tracing::SeriesValue::Buckets { count, .. }) => *count,
            _ => 0,
        };
        assert_eq!(count, 2, "{name}: the holder and the caller");
    }
    let records = drain.queued_records();
    let wait = records
        .iter()
        .find(|record| {
            record.message() == "lock.wait" && record.fields().contains("\"span\":\"closed\"")
        })
        .expect("the waiting caller opened a wait span");
    let wait: serde_json::Value = serde_json::from_str(wait.fields()).expect("fields");
    assert_eq!(wait["lock.name"], CAPABILITIES_FLIGHT_LOCK, "{wait}");
    assert_eq!(wait["outcome"], "acquired", "{wait}");
    assert_eq!(wait["holder"], "global.request", "{wait}");
}

#[tokio::test]
async fn test_fixture_in_flight_bound() {
    let server = FixtureServer::start(FixtureMode::Delay(Duration::from_millis(50)))
        .await
        .expect("fixture server");
    let client = GlobalClient::new(Config {
        max_in_flight: 1,
        ..server.config()
    })
    .expect("fixture client");
    let first = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .request(
                    crate::contract::Endpoint::Capabilities,
                    None,
                    None,
                    RESPONSE_BODY_BYTES_MAX,
                )
                .await
        })
    };
    let second = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .request(
                    crate::contract::Endpoint::Capabilities,
                    None,
                    None,
                    RESPONSE_BODY_BYTES_MAX,
                )
                .await
        })
    };
    assert!(first.await.is_ok());
    assert!(second.await.is_ok());
    assert_eq!(server.state.max_current.load(Ordering::SeqCst), 1);
}

fn resolution_request() -> PackageResolutionRequest {
    PackageResolutionRequest {
        entries: vec![PackageContextEntry {
            availability: PackageAvailability::Canonical,
            manager: "cargo".to_owned(),
            registry: Some("crates.io".to_owned()),
            name: "demo".to_owned(),
            requirement: Some("^1".to_owned()),
            version: None,
        }],
    }
}

fn package_request() -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        registry: "crates.io".to_owned(),
        name: "demo".to_owned(),
        version: "1.0.0".to_owned(),
    }
}

/// The contract's `maxLength` for `query`, a query term's `text`, and a symbol `name` counts
/// characters, so each field holding that many two-byte characters is accepted although it
/// holds twice as many bytes, and one character more refuses naming the field.
#[test]
fn test_query_term_and_name_bounds_count_characters() {
    let at_bound = |max: usize| "\u{e9}".repeat(max);
    let past_bound = |max: usize| "\u{e9}".repeat(max + 1);
    assert_eq!(at_bound(QUERY_BYTES_MAX).len(), 2 * QUERY_BYTES_MAX);

    let mut search = search_request();
    search.query = Some(at_bound(QUERY_BYTES_MAX));
    assert!((validate_search_request(&search)).is_ok());
    search.query = Some(past_bound(QUERY_BYTES_MAX));
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "query",
    );

    let mut search = search_request();
    search.terms.as_mut().expect("lexical terms")[0].text = at_bound(QUERY_TERM_BYTES_MAX);
    assert!((validate_search_request(&search)).is_ok());
    search.terms.as_mut().expect("lexical terms")[0].text = past_bound(QUERY_TERM_BYTES_MAX);
    assert_registered_field(
        &(validate_search_request(&search)).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "term",
    );

    let mut search = search_request();
    search.identifiers = Some(vec!["x".repeat(IDENTIFIER_BYTES_MAX)]);
    assert!(validate_search_request(&search).is_ok());
    search.identifiers = Some(vec!["x".repeat(IDENTIFIER_BYTES_MAX + 1)]);
    assert_registered_field(
        &validate_search_request(&search).expect_err("identifier bound"),
        "rift.client.request_invalid",
        "identifiers",
    );
}

fn traversal_request() -> PackageSearchRequest {
    PackageSearchRequest {
        query: None,
        terms: None,
        identifiers: None,
        phase: None,
        include: None,
        packages: None,
        target: None,
        view: None,
        traversal: Some(SearchTraversal {
            seed: Some("rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile".to_owned()),
            to: None,
            depth: Some(1),
            direction: None,
            facets: None,
        }),
    }
}

#[test]
fn traversal_request_preserves_runtime_seed_without_lexical_fields() {
    let request = traversal_request();
    validate_search_request(&request).expect("runtime traversal request");
    let value = serde_json::to_value(&request).expect("request serialization");
    for field in ["query", "terms", "identifiers", "phase", "packages"] {
        assert!(value.get(field).is_none(), "unexpected field: {field}");
    }
    assert_eq!(
        value["traversal"]["seed"],
        "rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile"
    );
}

#[test]
fn traversal_request_refuses_missing_seed_local_owner_and_excess_depth() {
    for (seed, depth, field) in [
        (None, 1, "traversal.seed"),
        (
            Some("rift://symbol/local/rust/demo/beacon"),
            1,
            "traversal.seed",
        ),
        (
            Some("rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile"),
            3,
            "traversal",
        ),
    ] {
        let mut request = traversal_request();
        let traversal = request.traversal.as_mut().expect("traversal");
        traversal.seed = seed.map(str::to_owned);
        traversal.depth = Some(depth);
        let error = validate_search_request(&request).expect_err("invalid traversal request");
        assert_registered_field(&error, "rift.client.request_invalid", field);
    }
}

#[test]
fn traversal_only_request_refuses_lexical_members_and_documentation_target() {
    let mut request = traversal_request();
    request.terms = Some(Vec::new());
    let error = validate_search_request(&request).expect_err("lexical member without query");
    assert_registered_field(&error, "rift.client.request_invalid", "query");
    request.terms = None;
    request.target = Some(PackageSearchRequestTarget::Documentation);
    let error = validate_search_request(&request).expect_err("traversal documentation target");
    assert_registered_field(&error, "rift.client.request_invalid", "traversal");
}

#[test]
fn empty_search_page_requires_original_captured_view() {
    let mut value = search_page_json("demo", None, "analyzer-v1", "demo");
    value["items"] = serde_json::json!([]);
    let capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities");
    let mut request = traversal_request();
    request.view = Some("a".repeat(64));
    let page: PackageSearchPage =
        serde_json::from_value(value.clone()).expect("empty captured page");
    validate_search_page(&request, &capabilities, &page, None, SOURCE_BYTES_MAX)
        .expect("empty page retains requested view");
    request.view = Some("b".repeat(64));
    let error = validate_search_page(&request, &capabilities, &page, None, SOURCE_BYTES_MAX)
        .expect_err("different captured view");
    assert_registered_field(&error, "rift.client.response_invalid", "view");
    value.as_object_mut().expect("page").remove("view");
    assert!(serde_json::from_value::<PackageSearchPage>(value).is_err());
}

fn search_request() -> PackageSearchRequest {
    PackageSearchRequest {
        query: Some("demo".to_owned()),
        terms: Some(vec![QueryTerm {
            text: "demo".to_owned(),
            phrase: false,
            prefix: false,
        }]),
        identifiers: Some(vec!["demo".to_owned()]),
        include: None,
        packages: Some(vec![package_request()]),
        phase: Some(PackageSearchRequestPhase::Precise),
        target: None,
        traversal: None,
        view: None,
    }
}

fn symbol_request() -> GetSymbolParams {
    serde_json::from_value(serde_json::json!({
        "id": "rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo"
    }))
    .expect("canonical symbol request")
}

fn symbol_result_json(package: &str, suffix: &str) -> serde_json::Value {
    serde_json::json!({
        "outcome": "found", "symbol": {
            "id": format!("rift://symbol/cargo/crates.io/{package}@1.0.0/rust/{package}"),
            "language": "rust", "name": package, "kind": "function",
            "origin": {"location": "dependency", "package": package_json(package), "source_kind": "authored"}
        },
        "view": {"id": "a".repeat(64), "expires_at": "2026-10-10T01:00:00Z"},
        "revision": format!("rift://revision/cargo/crates.io/{package}@1.0.0"),
        "declarations": [{
            "origin": {"location": "dependency", "package": package_json(package), "source_kind": "authored"},
            "unit": format!("rift://source/cargo/crates.io/{package}@1.0.0/src/{suffix}.rs"),
            "range": {"start": 0, "end": 4}, "line": 1,
            "signature_indices": [], "type_indices": [], "documentation_indices": []
        }]
    })
}

async fn canonical_symbol_response(
    value: serde_json::Value,
    request: &GetSymbolParams,
    source_bytes_max: usize,
) -> Result<GetSymbolResult, RiftError> {
    canonical_symbol_response_with_bounds(value, request, source_bytes_max, SOURCE_BYTES_MAX).await
}

async fn canonical_symbol_response_with_bounds(
    value: serde_json::Value,
    request: &GetSymbolParams,
    configured_source_bytes_max: usize,
    advertised_source_bytes_max: usize,
) -> Result<GetSymbolResult, RiftError> {
    let (server, _) = operation_client(OperationFixture::DocumentationBounds {
        search: documentation::page(),
        symbols: value,
        source_bytes_max: advertised_source_bytes_max,
    })
    .await;
    let mut config = server.config();
    config.max_source = configured_source_bytes_max;
    let client = GlobalClient::new(config).expect("canonical client");
    client.get_symbol(request).await
}

async fn operation_client(mode: OperationFixture) -> (FixtureServer, GlobalClient) {
    let server = FixtureServer::start(FixtureMode::Operations(mode))
        .await
        .unwrap_or_else(|error| panic!("fixture server: {error}"));
    let client = GlobalClient::new(server.config())
        .unwrap_or_else(|error| panic!("fixture client: {error:?}"));
    (server, client)
}

#[test]
fn generated_source_range_preserves_exact_integer_bound() {
    use validator::Validate;
    let bound = 9_007_199_254_740_991;
    assert!(
        crate::generated::TextRange {
            start: bound,
            end: bound
        }
        .validate()
        .is_ok()
    );
    assert!(
        crate::generated::TextRange {
            start: 0,
            end: bound + 1
        }
        .validate()
        .is_err()
    );
    assert!(
        crate::generated::TextRange {
            start: bound + 1,
            end: bound + 1
        }
        .validate()
        .is_err()
    );
}

#[tokio::test]
async fn captured_custom_positions_retain_the_requested_view_and_coverage_refusal() {
    use rift_protocol::source_read::FindDeclarationsParams;
    let unit = "rift://source/custom/src/lib.rs";
    let view = serde_json::json!({"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"});
    let position = serde_json::json!({"unit":unit,"line":0,"character":0});
    let request: FindDeclarationsParams = serde_json::from_value(
        serde_json::json!({"position_encoding":"utf-16","positions":[position],"view":view["id"]}),
    )
    .expect("captured custom position");
    let value = serde_json::json!({"results":[{"outcome":"unavailable","position":position,"view":view,"reason":"insufficient_coverage"}]});
    let (server, client) = operation_client(OperationFixture::SourceDeclarations(
        value.clone(),
        StatusCode::OK,
    ))
    .await;
    assert_eq!(
        client
            .find_declarations(&request)
            .await
            .expect("coverage refusal"),
        serde_json::from_value(value).expect("position result")
    );
    let requests = server.state.request_log.lock().await;
    let sent = requests
        .iter()
        .find(|request| request.path.ends_with("/declarations"))
        .expect("custom position request");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&sent.body).expect("body")["view"],
        view["id"]
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/resolutions"))
    );
}

#[tokio::test]
async fn captured_view_does_not_admit_project_sources_to_global_reads() {
    use rift_protocol::source_read::FindDeclarationsParams;
    let (server, client) = operation_client(OperationFixture::Valid).await;
    let unit = "rift://source/project/src/lib.rs";
    let declarations: FindDeclarationsParams = serde_json::from_value(serde_json::json!({"position_encoding":"utf-8","positions":[{"unit":unit,"line":0,"character":0}],"view":"a".repeat(64)})).expect("project position");
    assert_registered_field(
        &(client.find_declarations(&declarations).await).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "unit",
    );
    assert!(server.state.request_log.lock().await.is_empty());
}

#[tokio::test]
async fn declaration_positions_refuse_unproved_global_owners_before_http() {
    use rift_protocol::source_read::FindDeclarationsParams;
    let (server, client) = operation_client(OperationFixture::Valid).await;
    for unit in [
        "rift://source/project/src/lib.rs",
        "rift://source/custom/src/lib.rs",
    ] {
        let request: FindDeclarationsParams = serde_json::from_value(serde_json::json!({
            "position_encoding":"utf-8","positions":[{"unit":unit,"line":0,"character":0}]
        }))
        .expect("valid local position");
        assert!(matches!(
            client.find_declarations(&request).await,
            Err(ref error) if error.slug().as_str() == "rift.client.request_invalid"
                && error_text_field(error, "field") == "unit"
        ));
    }
    assert!(server.state.request_log.lock().await.is_empty());
}

#[tokio::test]
async fn source_declaration_batch_preserves_runtime_and_stub_owners_with_one_view() {
    use rift_protocol::source_read::FindDeclarationsParams;
    let positions = serde_json::json!([
        {"unit":"rift://source/stdlib/cpython@3.12.9/Lib/pathlib.py","line":0,"character":0},
        {"unit":"rift://source/pypi/pypi.org/types-pathlib@0.1.0/pathlib.pyi","line":2,"character":3}
    ]);
    let request: FindDeclarationsParams = serde_json::from_value(serde_json::json!({"position_encoding":"utf-16","positions":positions,"view":"a".repeat(64)})).expect("position request");
    let view = serde_json::json!({"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"});
    let value = serde_json::json!({"results":[
        {"outcome":"found","position":positions[0],"id":"rift://symbol/stdlib/cpython@3.12.9/python/pathlib/Path","kind":"class","view":view},
        {"outcome":"unavailable","position":positions[1],"reason":"insufficient_coverage","view":view}
    ],"warnings":[]});
    let (server, client) = operation_client(OperationFixture::SourceDeclarations(
        value.clone(),
        StatusCode::OK,
    ))
    .await;
    assert_eq!(
        client
            .find_declarations(&request)
            .await
            .expect("ordered outcomes"),
        serde_json::from_value(value).expect("batch outcome")
    );
    let requests = server.state.request_log.lock().await;
    let sent = requests
        .iter()
        .find(|sent| sent.path.ends_with("/declarations"))
        .expect("position request");
    assert_eq!(sent.query, None);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&sent.body).expect("request JSON"),
        serde_json::to_value(request).expect("request")
    );
    assert!(
        !requests
            .iter()
            .any(|sent| sent.path.ends_with("/resolutions"))
    );
}

#[tokio::test]
async fn source_declaration_batch_refuses_reordering_foreign_views_and_whole_batch_missing() {
    use rift_protocol::source_read::FindDeclarationsParams;
    let positions = serde_json::json!([
        {"unit":"rift://source/stdlib/cpython@3.12.9/Lib/pathlib.py","line":0,"character":0},
        {"unit":"rift://source/stdlib/cpython@3.12.9/Lib/pathlib.py","line":1,"character":0}
    ]);
    let request: FindDeclarationsParams = serde_json::from_value(
        serde_json::json!({"position_encoding":"utf-8","positions":positions}),
    )
    .expect("position request");
    let view = serde_json::json!({"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"});
    let value = serde_json::json!({"results":[{"outcome":"missing","position":positions[0],"view":view},{"outcome":"missing","position":positions[1],"view":view}],"warnings":[]});
    let mut reordered = value.clone();
    reordered["results"]
        .as_array_mut()
        .expect("results")
        .swap(0, 1);
    let mut foreign_view = value.clone();
    foreign_view["results"][1]["view"]["id"] = serde_json::json!("b".repeat(64));
    for (value, status) in [
        (reordered, StatusCode::OK),
        (foreign_view, StatusCode::OK),
        (value, StatusCode::NOT_FOUND),
    ] {
        let (_server, client) =
            operation_client(OperationFixture::SourceDeclarations(value, status)).await;
        assert!(client.find_declarations(&request).await.is_err());
    }
}

#[tokio::test]
async fn exact_symbol_request_carries_one_immutable_id_and_no_query_selectors() {
    use rift_protocol::read::SymbolId;
    use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};
    let id = SymbolId::parse("rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/Thing")
        .expect("canonical fixture");
    let request: GetSymbolParams =
        serde_json::from_value(serde_json::json!({"id":id})).expect("request");
    let view = serde_json::json!({"id":"a".repeat(64),"expires_at":"2026-10-10T10:05:00Z"});
    for answer in [
        serde_json::json!({"outcome":"found","symbol":{"id":id,"language":"rust","name":"Thing","kind":"struct","origin":{"source_kind":"synthetic"}},"view":view,"revision":"rift://revision/cargo/crates.io/demo@1.0.0","declarations":[],"warnings":[]}),
        serde_json::json!({"outcome":"missing","symbol_not_found":{"id":id,"alternatives":[]},"view":view,"revision":"rift://revision/cargo/crates.io/demo@1.0.0","warnings":[]}),
        serde_json::json!({"outcome":"unavailable","id":id,"reason":"exact_release","warnings":[]}),
    ] {
        let (server, client) = operation_client(OperationFixture::ExactSymbol(answer)).await;
        assert!(matches!(
            client
                .get_symbol(&request)
                .await
                .expect("typed exact outcome"),
            GetSymbolResult::Found { .. }
                | GetSymbolResult::Missing { .. }
                | GetSymbolResult::Unavailable { .. }
        ));
        let requests = server.state.request_log.lock().await;
        let sent = requests
            .iter()
            .find(|sent| sent.path.ends_with("/symbols"))
            .expect("symbol request");
        assert_eq!(sent.method, reqwest::Method::POST);
        assert_eq!(sent.path, "/rift/rest/v1/symbols");
        assert_eq!(sent.query, None);
        assert_eq!(
            requests
                .iter()
                .filter(|sent| sent.path.ends_with("/symbols"))
                .count(),
            1
        );
        let body: serde_json::Value = serde_json::from_slice(&sent.body).expect("JSON request");
        assert_eq!(body["id"], serde_json::to_value(&id).expect("ID"));
        assert_eq!(body["declaration_limit"], 5);
        assert_eq!(
            body.get("include"),
            None,
            "an omitted include requests no projection"
        );
        for removed in [
            "name",
            "packages",
            "language",
            "scope",
            "limit",
            "page_index",
        ] {
            assert!(body.get(removed).is_none());
        }
        assert_eq!(
            requests
                .iter()
                .filter(|sent| sent.path.ends_with("/resolutions"))
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn test_fixture_resolution_valid_and_cache_control() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    let request = resolution_request();
    let first = client.resolve_package_context(&request).await;
    let second = client.resolve_package_context(&request).await;
    assert!(first.is_ok());
    assert_eq!(first.ok(), second.ok());
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_resolution_single_flight() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    let request = resolution_request();
    let mut tasks = Vec::new();
    for _ in 0..3 {
        let client = client.clone();
        let request = request.clone();
        tasks.push(tokio::spawn(async move {
            client.resolve_package_context(&request).await
        }));
    }
    for task in tasks {
        assert!(task.await.is_ok_and(|result| result.is_ok()));
    }
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_new_client_does_not_reuse_resolution_cache() {
    let first_server = FixtureServer::start(FixtureMode::Operations(OperationFixture::Valid))
        .await
        .expect("first fixture server");
    let first_client = GlobalClient::new(first_server.config()).expect("first fixture client");
    let request = resolution_request();
    assert!(first_client.resolve_package_context(&request).await.is_ok());
    assert!(first_client.resolve_package_context(&request).await.is_ok());
    assert_eq!(first_server.state.requests.load(Ordering::SeqCst), 2);

    let second_server = FixtureServer::start(FixtureMode::Operations(OperationFixture::Valid))
        .await
        .expect("second fixture server");
    let second_client = GlobalClient::new(second_server.config()).expect("second fixture client");
    assert!(
        second_client
            .resolve_package_context(&request)
            .await
            .is_ok()
    );
    assert_eq!(second_server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_resolution_rejects_bad_accounting() {
    let (server, client) = operation_client(OperationFixture::InvalidResolutionAccounting).await;
    let result = client.resolve_package_context(&resolution_request()).await;
    assert_registered_field(
        &(result).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "resolution_accounting",
    );
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_search_and_symbol_pages() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    assert!(
        client
            .search_packages(&search_request(), 20, None)
            .await
            .is_ok()
    );
    assert!(client.get_symbol(&symbol_request()).await.is_ok());
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/symbols")
    );
}

#[tokio::test]
async fn test_discovery_search_accepts_omitted_or_empty_package_selection() {
    for packages in [None, Some(Vec::new())] {
        let (_server, client) = operation_client(OperationFixture::UnrequestedPackage).await;
        let mut request = search_request();
        request.packages = packages;
        let serialized = serde_json::to_value(&request).expect("search request");
        if request.packages.is_none() {
            assert!(serialized.get("packages").is_none());
        }
        let page = client
            .search_packages(&request, 20, None)
            .await
            .expect("discovery page");
        assert_eq!(page.items.len(), 1);
        let PackageSearchItem::Search(hit) = &page.items[0] else {
            panic!("symbol search hit")
        };
        assert_eq!(hit.package.name, "other");
    }
    let (_server, client) = operation_client(OperationFixture::UnrequestedPackage).await;
    assert_registered_field(
        &(client.search_packages(&search_request(), 20, None).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "package",
    );
}

#[tokio::test]
async fn test_fixture_page_assembly_handles_cursor_and_revisions() {
    let (server, client) = operation_client(OperationFixture::SearchPages).await;
    let pages = client.search_packages_pages(&search_request(), 20).await;
    assert!(pages.is_ok());
    assert_eq!(pages.ok().map(|value| value.items.len()), Some(2));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 3);

    let (server, client) = operation_client(OperationFixture::Valid).await;
    let result = client
        .get_symbol(&symbol_request())
        .await
        .expect("canonical result");
    assert!(
        matches!(result, GetSymbolResult::Found { ref declarations, .. } if declarations.len() == 1)
    );
    assert_eq!(
        server.state.requests.load(Ordering::SeqCst),
        2,
        "exact reads issue no implicit continuation"
    );

    let (_server, client) = operation_client(OperationFixture::PageRevisionChange).await;
    assert_registered_field(
        &(client.search_packages_pages(&search_request(), 20).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "revision",
    );

    let (server, client) = operation_client(OperationFixture::CandidateBound).await;
    let pages = client.search_packages_pages(&search_request(), 20).await;
    assert_eq!(pages.ok().map(|value| value.items.len()), Some(1));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_page_rejects_repeated_cursor_and_partial_failure() {
    let (_server, client) = operation_client(OperationFixture::RepeatedCursor).await;
    assert_registered_field(
        &(client.search_packages_pages(&search_request(), 20).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "cursor_progress",
    );

    let (server, client) = operation_client(OperationFixture::PartialFailure).await;
    assert!(matches!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ref error) if error.slug().as_str() == "rift.client.remote_unavailable"
            && error_text_field(error, "status") == "503"
    ));
    let requests = server.state.requests.load(Ordering::SeqCst);
    assert!(
        matches!(
            client.search_packages_pages(&search_request(), 20).await,
        Err(ref error) if error.slug().as_str() == "rift.client.remote_unavailable"
            && error_text_field(error, "status") == "503"
        ),
        "a later request answers the failure that marked the endpoint unavailable"
    );
    assert_eq!(server.state.requests.load(Ordering::SeqCst), requests);
}

#[tokio::test]
async fn test_fixture_new_endpoint_replaces_failure_cache() {
    let failed_server = FixtureServer::start(FixtureMode::Operations(OperationFixture::Problem(
        StatusCode::SERVICE_UNAVAILABLE,
    )))
    .await
    .expect("failed fixture server");
    let failed_client = GlobalClient::new(failed_server.config()).expect("failed fixture client");
    assert!(matches!(
        failed_client
            .search_packages(&search_request(), 20, None)
            .await,
        Err(ref error) if error.slug().as_str() == "rift.client.remote_unavailable"
            && error_text_field(error, "status") == "503"
    ));
    assert_eq!(failed_server.state.requests.load(Ordering::SeqCst), 4);

    let healthy_server = FixtureServer::start(FixtureMode::Operations(OperationFixture::Valid))
        .await
        .expect("healthy fixture server");
    let healthy_client =
        GlobalClient::new(healthy_server.config()).expect("healthy fixture client");
    assert!(
        healthy_client
            .search_packages(&search_request(), 20, None)
            .await
            .is_ok()
    );
    assert_eq!(healthy_server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_rejects_query_mismatched_cursor_page() {
    let (server, client) = operation_client(OperationFixture::MismatchedCursor).await;
    assert_registered_field(
        &(client.search_packages_pages(&search_request(), 20).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "duplicate_item",
    );
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 3);
    assert_eq!(
        server.state.last_query.lock().await.as_deref(),
        Some("limit=20&cursor=next")
    );
}

#[tokio::test]
async fn test_fixture_rejects_unrequested_package() {
    let (_server, client) = operation_client(OperationFixture::UnrequestedPackage).await;
    assert_registered_field(
        &(client.search_packages(&search_request(), 20, None).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "package",
    );
}

#[tokio::test]
async fn test_fixture_rejects_invalid_origin_source_and_match_class() {
    let cases = [
        (OperationFixture::InvalidOrigin, "origin"),
        (OperationFixture::InvalidSource, "source"),
        (OperationFixture::InvalidSourceIdentity, "source_identity"),
        (OperationFixture::InvalidMatchClass, "match_class"),
    ];
    for (mode, field) in cases {
        let (_server, client) = operation_client(mode).await;
        assert_registered_field(
            &(client.search_packages(&search_request(), 20, None).await)
                .expect_err("registered refusal"),
            "rift.client.response_invalid",
            field,
        );
    }
}

/// Original name matching chooses the reported class before a qualified case fallback.
#[test]
fn test_search_match_class_uses_original_spelling_before_class() {
    let mut request = search_request();
    request.query = Some("module::searchhit SearchHit".to_owned());
    let mut value = search_hit_json("demo", "first");
    value["symbol"]["name"] = serde_json::json!("SearchHit");
    value["match_class"] = serde_json::json!("name_exact");
    let hit: PackageSearchHit = serde_json::from_value(value).expect("search hit fixture");
    request.identifiers = Some(vec!["module::searchhit".to_owned(), "SearchHit".to_owned()]);
    assert!((super::validate_search_match_class(&request, &hit, "Module::SearchHit")).is_ok());
    request.identifiers = Some(Vec::new());
    assert_registered_field(
        &(super::validate_search_match_class(&request, &hit, "Module::SearchHit"))
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "match_class",
    );
}

/// A hit found by its text or its vector reports `unknown`, and the page reaches the caller when
/// the hit matches none of the requested identifiers; `unknown` on a hit spelling a requested
/// identifier contradicts it (#389).
#[tokio::test]
async fn test_fixture_accepts_unknown_match_class_on_a_hit_matching_no_identifier() {
    for identifiers in [Vec::new(), vec!["absent".to_owned()]] {
        let (_server, client) = operation_client(OperationFixture::UnknownMatchClass).await;
        let mut request = search_request();
        request.identifiers = Some(identifiers);
        let page = client
            .search_packages(&request, 20, None)
            .await
            .expect("an unknown hit matching no identifier is valid");
        assert_eq!(page.items.len(), 1);
    }

    let (_server, client) = operation_client(OperationFixture::UnknownMatchClass).await;
    assert_registered_field(
        &(client.search_packages(&search_request(), 20, None).await)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "match_class",
    );
}

/// Exact identity selection preserves the declaration's original displayed spelling.
#[tokio::test]
async fn test_symbol_lookup_keeps_original_names() {
    for name in ["SearchHit", "searchHit"] {
        let mut value = symbol_result_json("demo", "first");
        value["symbol"]["name"] = serde_json::json!(name);
        let result = canonical_symbol_response(value, &symbol_request(), SOURCE_BYTES_MAX)
            .await
            .expect("canonical symbol");
        let GetSymbolResult::Found {
            symbol,
            declarations,
            ..
        } = result
        else {
            panic!("found symbol")
        };
        assert_eq!(symbol.name, name);
        assert_eq!(declarations.len(), 1);
        assert_eq!(
            declarations[0].unit.as_ref().map(|unit| unit.0.as_str()),
            Some("rift://source/cargo/crates.io/demo@1.0.0/src/first.rs")
        );
    }
    let mut request = symbol_request();
    request.id.0 = format!(" \t{}\n ", request.id.0);
    let (server, client) = operation_client(OperationFixture::Valid).await;
    assert!(client.get_symbol(&request).await.is_err());
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_symbol_lookup_refuses_discovery_match_class() {
    let mut value = symbol_result_json("demo", "first");
    value["match_class"] = serde_json::json!("unknown");
    assert_registered_field(
        &canonical_symbol_response(value, &symbol_request(), SOURCE_BYTES_MAX)
            .await
            .expect_err("unknown exact-result field"),
        "rift.client.response_invalid",
        "body",
    );
}

/// Package analysis mints a package declaration's identity over its unit's resolver and key, so
/// a hit carries `rift://symbol/rust/cargo/demo@1.0.0/src/first.rs/demo` beside the unit
/// `rift://source/cargo/crates.io/demo@1.0.0/src/first.rs`. The package-relative spelling names a
/// project file, and no producer mints it for a package declaration, so it is refused.
#[tokio::test]
async fn test_hits_accept_the_symbol_identity_package_analysis_mints() {
    let hit = |id: &str| {
        let mut value = search_hit_json("demo", "first");
        value["symbol"]["id"] = serde_json::json!(id);
        serde_json::from_value::<PackageSearchHit>(value).expect("search hit fixture")
    };
    let minted = rift_core::symbol_identity("rust", "cargo/demo@1.0.0/src/first.rs", "demo");
    let candidate = PackageSearchCandidate::try_from(&hit(&minted));
    assert!(candidate.is_ok(), "{minted}: {candidate:?}");
    let refused = [
        "rift://symbol/rust/src/first.rs/demo",
        "rift://symbol/rust/cargo/demo@2.0.0/src/first.rs/demo",
        "rift://symbol/rust/npm/demo@1.0.0/src/first.rs/demo",
        "rift://symbol/rust/cargo/demo@1.0.0/src/second.rs/demo",
        "rift://symbol/rust/cargo/demo@1.0.0/demo",
    ];
    for id in refused {
        let error = PackageSearchCandidate::try_from(&hit(id)).expect_err("identity refusal");
        assert_registered_field(&error, "rift.client.response_invalid", "symbol_identity");
    }

    let result = canonical_symbol_response(
        symbol_result_json("demo", "first"),
        &symbol_request(),
        SOURCE_BYTES_MAX,
    )
    .await
    .expect("canonical owner and source unit");
    let GetSymbolResult::Found {
        symbol,
        declarations,
        ..
    } = result
    else {
        panic!("found symbol")
    };
    assert_eq!(symbol.id.as_ref(), Some(&symbol_request().id));
    assert_eq!(
        declarations[0].unit.as_ref().map(|unit| unit.0.as_str()),
        Some("rift://source/cargo/crates.io/demo@1.0.0/src/first.rs")
    );
    for unit in [
        "rift://source/cargo/crates.io/demo@2.0.0/src/first.rs",
        "rift://source/npm/npmjs.org/demo@1.0.0/src/first.rs",
    ] {
        let mut value = symbol_result_json("demo", "first");
        value["declarations"][0]["unit"] = serde_json::json!(unit);
        assert_registered_field(
            &canonical_symbol_response(value, &symbol_request(), SOURCE_BYTES_MAX)
                .await
                .expect_err("foreign source owner"),
            "rift.client.response_invalid",
            "body",
        );
    }
}

#[test]
fn test_released_source_preserves_registry_owner_and_relative_path() {
    let mut package: PackageIdentity =
        serde_json::from_value(package_json("demo")).expect("package");
    package.registry = "registry.example/releases".to_owned();
    let owner = domain::package_identity(&package).owner().expect("owner");
    let unit = rift_protocol::identity::released_source_identity(&owner, "src/first.rs")
        .expect("source unit");
    assert!(unit.contains("registry.example%2Freleases/"));
    assert_eq!(
        package_source_path(&package, &unit).expect("same owner"),
        "src/first.rs"
    );
    package.registry = "crates.io".to_owned();
    assert_registered_field(
        &(package_source_path(&package, &unit)).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "source_identity",
    );
}

#[test]
fn test_package_selection_distinguishes_exact_registry_owners() {
    let package: PackageIdentity = serde_json::from_value(package_json("demo")).expect("package");
    let mut other = package.clone();
    other.registry = "registry.example/releases".to_owned();
    assert!((validate_packages(&[package.clone(), other])).is_ok());
    assert_registered_field(
        &(validate_packages(&[package.clone(), package])).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "duplicate_package",
    );
}

#[tokio::test]
async fn test_fixture_rejects_invalid_capabilities() {
    let (_server, client) = operation_client(OperationFixture::InvalidPublication).await;
    assert_registered_field(
        &(client.get_capabilities().await).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "publication_format",
    );

    let (_server, client) = operation_client(OperationFixture::MissingRequiredField).await;
    assert_registered_field(
        &(client.get_capabilities().await).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "required_search_fields",
    );
}

/// The search fields are an exact set: a service whose `required_search_fields` still name
/// `declaration_source` ranks over a field the client no longer merges, and is refused.
#[test]
fn test_capabilities_still_searching_declaration_source_are_refused() {
    let mut value: serde_json::Value =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    let current: Capabilities =
        serde_json::from_value(value.clone()).expect("current capabilities");
    assert!((validate_capabilities(&current)).is_ok());
    value["required_search_fields"]
        .as_array_mut()
        .expect("required_search_fields is an array")
        .push(serde_json::json!("declaration_source"));
    let previous: Capabilities = serde_json::from_value(value).expect("previous capabilities");
    assert_registered_field(
        &(validate_capabilities(&previous)).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "required_search_fields",
    );
}

#[tokio::test]
async fn test_fixture_enforces_active_server_bounds() {
    let (_server, client) = operation_client(OperationFixture::TightBounds).await;
    assert_registered_field(
        &(client.search_packages(&search_request(), 3, None).await)
            .expect_err("registered refusal"),
        "rift.client.request_invalid",
        "limit",
    );
    assert!(matches!(
        client.resolve_package_context(&resolution_request()).await,
        Err(ref error) if error.slug().as_str() == "rift.client.body_limit"
            && error_text_field(error, "direction") == "response"
    ));

    // Each page read meets the advertised response body bound on its own request, refusing
    // the page by its declared length, and records the failure a later request answers.
    let search_page = search_page_json("demo", None, "analyzer-v2", "first").to_string();
    let symbol_page = symbol_result_json("demo", "first").to_string();
    for (endpoint, page) in [
        (crate::contract::Endpoint::Search, search_page),
        (crate::contract::Endpoint::Symbols, symbol_page),
    ] {
        let (server, client) = operation_client(OperationFixture::TightBounds).await;
        let error = operation_failure(&client, endpoint)
            .await
            .expect("body bound refusal");
        assert_eq!(error.slug().as_str(), "rift.client.body_limit");
        assert_eq!(error_text_field(&error, "direction"), "response");
        assert_eq!(error_text_field(&error, "bytes"), page.len().to_string());
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
        let cached = client
            .get_capabilities()
            .await
            .expect_err("recorded body refusal");
        assert_eq!(cached.to_string(), error.to_string());
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
    }

    let (server, client) = operation_client(OperationFixture::TightRequestBound).await;
    assert!(matches!(
        client.resolve_package_context(&resolution_request()).await,
        Err(ref error) if error.slug().as_str() == "rift.client.body_limit"
            && error_text_field(error, "direction") == "request"
    ));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);

    let (server, client) = operation_client(OperationFixture::TightSourceBound).await;
    let mut request = search_request();
    request.include = Some(vec!["source".to_owned()]);
    assert_registered_field(
        &(client.search_packages(&request, 20, None).await).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "source",
    );
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[test]
fn generated_read_warning_preserves_bound_and_source_evidence() {
    let unit = "rift://source/stdlib/node@22.14.0/lib/fs.js";
    let value = serde_json::json!({
        "code": "object_unavailable", "unit": unit,
        "range": {"start": 4, "end": 12}, "qualified_name": "fs::wide",
        "field": "name", "bound": 4096
    });
    let shared: rift_protocol::read::ReadWarning =
        serde_json::from_value(value.clone()).expect("strict shared warning");
    assert_eq!(serde_json::to_value(shared).expect("warning wire"), value);
    let warning: crate::generated::ReadWarning =
        serde_json::from_value(value).expect("generated warning");
    assert_eq!(warning.code.as_deref(), Some("object_unavailable"));
    assert_eq!(warning.unit.as_deref(), Some(unit));
    assert_eq!(warning.range.expect("physical range").start, 4);
    assert_eq!(warning.qualified_name.as_deref(), Some("fs::wide"));
    assert_eq!(warning.field.as_deref(), Some("name"));
    assert_eq!(warning.bound, Some(4096));
}

#[test]
fn generated_exact_symbol_shapes_keep_shared_requests_and_typed_outcomes() {
    use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};
    const ID: &str = "rift://symbol/cargo/crates.io/tokio@1.50.0/rust/tokio/Beacon";
    let request: GetSymbolParams = serde_json::from_value(json!({
        "id": ID, "include": ["history", "documentation"], "declaration_limit": 5,
        "view": "a".repeat(64),
    }))
    .expect("shared request");
    let wire = serde_json::to_value(&request).expect("shared request encoding");
    let generated = crate::generated::GetSymbolParams {
        id: request.id.as_str().to_owned(),
        include: Some(vec![
            crate::generated::GetSymbolInclude::History,
            crate::generated::GetSymbolInclude::Documentation,
        ]),
        declaration_limit: Some(request.declaration_limit),
        declaration_cursor: request.declaration_cursor.clone(),
        revision: request.revision.as_ref().map(ToString::to_string),
        view: request.view.as_ref().map(|view| view.as_str().to_owned()),
    };
    assert_eq!(
        serde_json::to_value(&generated).expect("generated request wire"),
        wire
    );
    assert_eq!(generated.id, request.id.as_str());
    assert_eq!(generated.declaration_limit, Some(5));
    assert_eq!(
        generated.view.as_deref(),
        request
            .view
            .as_ref()
            .map(rift_protocol::symbol_read::CapturedViewId::as_str)
    );
    assert_eq!(generated.include.expect("requested includes").len(), 2);
    let view = json!({"id": "a".repeat(64), "expires_at": "2026-10-10T01:00:00Z"});
    for value in [
        json!({"outcome": "found", "symbol": {"id": ID, "language": "rust", "name": "Beacon", "kind": "struct"}, "view": view, "revision": "rift://revision/cargo/crates.io/tokio@1.50.0", "declarations": []}),
        json!({"outcome": "missing", "symbol_not_found": {"id": ID, "alternatives": []}, "view": view, "revision": "rift://revision/cargo/crates.io/tokio@1.50.0"}),
        json!({"outcome": "unavailable", "id": ID, "reason": "exact_release"}),
    ] {
        let shared: GetSymbolResult =
            serde_json::from_value(value.clone()).expect("shared outcome");
        assert!(shared.is_valid_for(&request.id));
        let generated: crate::generated::GetSymbolResult =
            serde_json::from_value(value).expect("generated outcome shape");
        match (shared, generated) {
            (
                GetSymbolResult::Found { symbol, view, .. },
                crate::generated::GetSymbolResult::Object(result),
            ) => {
                assert_eq!(result.outcome, "found");
                assert_eq!(
                    result.symbol.id.as_deref(),
                    symbol
                        .id
                        .as_ref()
                        .map(rift_protocol::read::SymbolId::as_str)
                );
                assert_eq!(result.view.id, view.id.as_str());
                assert!(result.declarations.is_empty());
            }
            (
                GetSymbolResult::Missing {
                    symbol_not_found,
                    view,
                    ..
                },
                crate::generated::GetSymbolResult::Object2(result),
            ) => {
                assert_eq!(result.outcome, "missing");
                assert_eq!(result.symbol_not_found.id, symbol_not_found.id.as_str());
                assert_eq!(result.view.id, view.id.as_str());
            }
            (
                GetSymbolResult::Unavailable { id, .. },
                crate::generated::GetSymbolResult::Object3(result),
            ) => {
                assert_eq!(result.outcome, "unavailable");
                assert_eq!(result.id, id.as_str());
                assert!(result.view.is_none());
            }
            _ => panic!("generated outcome must keep the shared branch"),
        }
    }
    assert!(
        serde_json::from_value::<GetSymbolParams>(json!({"id": ID, "name": "Beacon"})).is_err()
    );
    assert!(
        serde_json::from_value::<GetSymbolResult>(
            json!({"outcome": "unavailable", "reason": "exact_release"})
        )
        .is_err()
    );
}
