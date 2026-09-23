use super::*;

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
    CandidateBound,
    PartialFailure,
    RetrySearch,
    RepeatedCursor,
    InvalidOrigin,
    InvalidSource,
    InvalidSourceIdentity,
    InvalidMatchClass,
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
        body,
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
        operation_response(&mode, &path, query.as_deref(), request_number)
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
) -> Response {
    if path.ends_with("/capabilities") {
        return operation_capabilities_response(mode);
    }
    if path.ends_with("/resolutions") {
        return operation_resolution_response(mode);
    }
    if path.ends_with("/search") {
        return operation_search_response(mode, query, request_number);
    }
    if path.ends_with("/symbols") {
        return operation_symbol_response(query);
    }
    status_response(StatusCode::NOT_FOUND)
}

fn operation_capabilities_response(mode: &OperationFixture) -> Response {
    capabilities_response_with(|value| match mode {
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
        OperationFixture::CandidateBound => {
            value["bounds"]["candidate_pool_max"] = serde_json::json!(1);
        }
        OperationFixture::AdditiveResponse => {
            value["future_field"] = serde_json::json!("ignored by this client");
        }
        _ => {}
    })
}

fn operation_resolution_response(mode: &OperationFixture) -> Response {
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
                "entry": {"availability":"canonical","manager":"cargo","name":"demo","requirement":"^1","version":null},
                "package": {"manager":"cargo","name":"demo","version":"1.0.0"}
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
    if let OperationFixture::Problem(status) = mode {
        return problem_response(*status);
    }
    if matches!(mode, OperationFixture::PartialFailure) && request_number > 1 {
        return problem_response(StatusCode::SERVICE_UNAVAILABLE);
    }
    if matches!(mode, OperationFixture::RetrySearch) && request_number == 1 {
        return problem_response_with_retry(StatusCode::SERVICE_UNAVAILABLE);
    }
    let cursor = query_cursor(query);
    let body = match mode {
        OperationFixture::UnrequestedPackage => {
            search_page_json("other", None, "analyzer-v1", "first")
        }
        OperationFixture::InvalidOrigin
        | OperationFixture::InvalidSource
        | OperationFixture::InvalidSourceIdentity
        | OperationFixture::InvalidMatchClass => invalid_search_page(mode),
        OperationFixture::TightSourceBound => {
            let mut page = search_page_json("demo", None, "analyzer-v1", "first");
            page["items"][0]["source"] = serde_json::json!("too large");
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

fn operation_symbol_response(query: Option<&str>) -> Response {
    let body = if query_cursor(query).is_none() {
        symbol_page_json("demo", Some("next"), "analyzer-v2", "first")
    } else {
        symbol_page_json("demo", None, "analyzer-v2", "second")
    };
    json_response(&body, None)
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
    serde_json::json!({"manager":"cargo","name":name,"version":"1.0.0"})
}

fn symbol_json(package: &str, suffix: &str) -> serde_json::Value {
    serde_json::json!({
        "id":format!("rift://symbol/rust/src/{suffix}.rs/demo"),
        "kind":"function",
        "language":"rust",
        "name":"demo",
        "origin":{"location":"dependency","package":package_json(package),"source_kind":"authored"}
    })
}

fn search_hit_json(package: &str, suffix: &str) -> serde_json::Value {
    serde_json::json!({
        "package":package_json(package), "symbol":symbol_json(package, suffix),
        "unit":format!("rift://source/cargo/{package}@1.0.0/src/{suffix}.rs"), "range":{"start":0,"end":4}, "line":1,
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
        "analyzer_revision":analyzer, "corpus_revision":"corpus-v1"
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
            serde_json::json!("rift://source/cargo/other@1.0.0/src/first.rs");
    } else {
        page["items"][0]["match_class"] = serde_json::json!("substring");
    }
    page
}

fn symbol_page_json(
    package: &str,
    next: Option<&str>,
    analyzer: &str,
    suffix: &str,
) -> serde_json::Value {
    serde_json::json!({
        "items":[{"package":package_json(package),"symbol":symbol_json(package, suffix),"unit":format!("rift://source/cargo/{package}@1.0.0/src/{suffix}.rs"),"range":{"start":0,"end":4},"line":1,"match_class":"qualified_exact"}],
        "next_cursor":next, "warnings":[], "publication_format":"rift-package-index-v2",
        "analyzer_revision":analyzer, "corpus_revision":"corpus-v1"
    })
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
            "signature",
            "declaration_source"
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
    assert_eq!(
        GlobalClient::new(config).err(),
        Some(ConfigError::InvalidEndpointScheme)
    );
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
        assert_eq!(
            GlobalClient::new(config).err(),
            Some(ConfigError::InvalidEndpoint),
            "{endpoint}"
        );
    }
}

#[test]
fn test_protocol_configuration_converts_without_drift() {
    let source = rift_protocol::configuration::GlobalConfiguration::default();
    let converted = Config::try_from(&source);
    assert_eq!(converted, Ok(Config::default()));
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
        assert_eq!(
            GlobalClient::new(config).err(),
            Some(ConfigError::OutOfRange(field))
        );
    }

    for token_env in ["", "RIFT-TOKEN"] {
        let config = Config {
            token_env: token_env.to_owned(),
            ..Config::default()
        };
        assert_eq!(
            GlobalClient::new(config).err(),
            Some(ConfigError::InvalidTokenEnvironment)
        );
    }
}

#[test]
fn test_client_errors_use_bounded_messages() {
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
    let cases = [
        (ClientError::Disabled, "global client disabled"),
        (
            ClientError::CredentialConfiguration,
            "global credential configuration invalid",
        ),
        (
            ClientError::RequestBodyTooLarge { bytes: 5 },
            "request body exceeds bound: 5 bytes",
        ),
        (
            ClientError::ResponseBodyTooLarge { bytes: 6 },
            "response body exceeds bound: 6 bytes",
        ),
        (ClientError::Deadline, "global request deadline elapsed"),
        (ClientError::Cancelled, "global request cancelled"),
        (ClientError::Connection, "global endpoint connection failed"),
        (
            ClientError::InvalidMediaType {
                status: 200,
                content_type: Some("text/plain".to_owned()),
            },
            "invalid response media type for status 200: Some(\"text/plain\")",
        ),
        (
            ClientError::InvalidResponse { status: 418 },
            "unsupported global response status: 418",
        ),
        (
            ClientError::Decode { status: 200 },
            "global response decode failed for status 200",
        ),
        (
            ClientError::InvalidRequest { field: "query" },
            "global request violates bound: query",
        ),
        (
            ClientError::InvalidResponseField { field: "query" },
            "global response violates contract: query",
        ),
        (
            ClientError::Http {
                meta: Box::new(meta),
                problem: None,
            },
            "global endpoint returned HTTP 503",
        ),
    ];
    for (error, expected) in cases {
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn test_request_relationships_are_validated_before_transport() {
    let mut resolution = resolution_request();
    resolution.entries[0].availability = PackageAvailability::LocalOnly;
    assert_eq!(
        validate_resolution_request(&resolution),
        Err(ClientError::InvalidRequest {
            field: "availability"
        })
    );
    let mut resolution = resolution_request();
    resolution.entries[0].version = Some("1.0.0".to_owned());
    assert_eq!(
        validate_resolution_request(&resolution),
        Err(ClientError::InvalidRequest {
            field: "version_or_requirement"
        })
    );
    let mut resolution = resolution_request();
    resolution.entries.push(resolution.entries[0].clone());
    assert_eq!(
        validate_resolution_request(&resolution),
        Err(ClientError::InvalidRequest {
            field: "duplicate_entry"
        })
    );

    let mut search = search_request();
    search.terms[0].phrase = true;
    search.terms[0].prefix = true;
    assert_eq!(
        validate_search_request(&search),
        Err(ClientError::InvalidRequest {
            field: "phrase_prefix"
        })
    );
    let mut search = search_request();
    search.terms[0].text = "ab".to_owned();
    search.terms[0].prefix = true;
    assert_eq!(
        validate_search_request(&search),
        Err(ClientError::InvalidRequest { field: "prefix" })
    );
    let mut search = search_request();
    search.include = Some(vec!["score".to_owned()]);
    assert_eq!(
        validate_search_request(&search),
        Err(ClientError::InvalidRequest { field: "include" })
    );
    let mut search = search_request();
    search.phase = PackageSearchRequestPhase::Broad;
    assert_eq!(
        validate_search_request(&search),
        Err(ClientError::InvalidRequest {
            field: "broad_phase"
        })
    );
    let mut search = search_request();
    search.packages.push(search.packages[0].clone());
    assert_eq!(
        validate_search_request(&search),
        Err(ClientError::InvalidRequest {
            field: "duplicate_package"
        })
    );
    assert_eq!(
        validate_page(0, None),
        Err(ClientError::InvalidRequest { field: "limit" })
    );
    assert_eq!(
        validate_page(1, Some("")),
        Err(ClientError::InvalidRequest { field: "cursor" })
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
    assert_eq!(result, Err(ClientError::Disabled));
}

#[test]
fn test_generated_requests_serialize_only_declared_fields() {
    let value = serde_json::to_value(search_request()).expect("search request serialization");
    assert!(value.get("query").is_some());
    assert!(value.get("target").is_none());
    assert!(value.get("unknown").is_none());
}

#[test]
fn test_documentation_request_requires_advertised_capability() {
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    let mut request = search_request();
    request.target = Some(PackageSearchRequestTarget::Documentation);
    assert_eq!(
        validate_search_request_for_capabilities(&request, 20, None, &capabilities),
        Err(ClientError::InvalidRequest { field: "target" })
    );

    capabilities
        .supported_features
        .push("documentation_search".to_owned());
    capabilities.documentation_revision = Some("0123abcd".to_owned());
    validate_capabilities(&capabilities).expect("valid documentation capability");
    assert!(validate_search_request_for_capabilities(&request, 20, None, &capabilities).is_ok());

    let mut symbol = symbol_request();
    symbol.include = Some(vec![PackageSymbolRequestInclude::Documentation]);
    assert_eq!(
        validate_symbol_request_for_capabilities(&symbol, 20, None, &capabilities),
        Err(ClientError::InvalidRequest { field: "include" })
    );
    capabilities
        .supported_features
        .push("symbol_documentation".to_owned());
    assert!(validate_symbol_request_for_capabilities(&symbol, 20, None, &capabilities).is_ok());
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
    assert!(matches!(
        serialize_body(&value),
        Err(ClientError::RequestBodyTooLarge { bytes }) if bytes > REQUEST_BODY_BYTES_MAX
    ));
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
    assert_eq!(
        validate_token(String::new()),
        Err(ConfigError::InvalidToken)
    );
    assert_eq!(
        validate_token("two words".to_owned()),
        Err(ConfigError::InvalidToken)
    );
    let error = ClientError::CredentialConfiguration;
    assert!(!error.to_string().contains("secret"));
}

#[test]
fn test_token_construction_cases_do_not_touch_environment() {
    let config = Config {
        token_env: "RIFT_CLOUD_CLIENT_TEST_TOKEN_CASES".to_owned(),
        ..Config::default()
    };
    assert_eq!(
        GlobalClient::new_with_token(config.clone(), None).map(|_| ()),
        Ok(())
    );
    assert_eq!(
        GlobalClient::new_with_token(config.clone(), Some(String::new())).err(),
        Some(ConfigError::InvalidToken)
    );
    assert_eq!(
        GlobalClient::new_with_token(config, Some("token with spaces".to_owned())).err(),
        Some(ConfigError::InvalidToken)
    );
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
    assert!(matches!(
        result,
        Err(ClientError::Http { ref meta, .. }) if meta.status == 307
    ));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_fixture_media_and_json_errors() {
    let server = FixtureServer::start(FixtureMode::WrongMedia)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert!(matches!(
        client.get_capabilities().await,
        Err(ClientError::InvalidMediaType {
            status: 200,
            content_type: Some(_)
        })
    ));
    drop(client);

    let server = FixtureServer::start(FixtureMode::MalformedJson)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert_eq!(
        client.get_capabilities().await,
        Err(ClientError::Decode { status: 200 })
    );
}

#[tokio::test]
async fn test_fixture_unauthorized_retains_challenge() {
    let server = FixtureServer::start(FixtureMode::Unauthorized)
        .await
        .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    let result = client.get_capabilities().await;
    let Err(ClientError::Http { meta, .. }) = result else {
        panic!("expected unauthorized HTTP error");
    };
    assert_eq!(meta.status, 401);
    assert_eq!(meta.www_authenticate.as_deref(), Some("Bearer realm=rift"));
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
    let Err(ClientError::Http { meta, .. }) = result else {
        panic!("expected rate limit HTTP error");
    };
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
    assert_eq!(meta.status, 429);
    assert_eq!(meta.retry_after.as_deref(), Some("0"));
    assert_eq!(meta.rate_limit_limit.as_deref(), Some("10"));
    assert_eq!(meta.rate_limit_remaining.as_deref(), Some("0"));
    assert_eq!(meta.rate_limit_reset.as_deref(), Some("60"));
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
        Err(ClientError::Http { ref meta, .. }) if meta.status == 504
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
        let Err(ClientError::Http { meta, problem }) = result else {
            panic!("expected HTTP {status} error");
        };
        assert_eq!(meta.status, status.as_u16());
        assert_eq!(
            problem.expect("valid problem details").status,
            i64::from(status.as_u16())
        );
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
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
    let Err(ClientError::Http { meta, .. }) = result else {
        panic!("expected rate limit HTTP error");
    };
    assert_eq!(meta.status, 429);
    assert_eq!(meta.retry_after.as_deref(), Some("3600"));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);
}

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
        connect_timeout: Duration::from_millis(100),
        request_timeout: Duration::from_secs(1),
        ..Config::default()
    })
    .expect("fixture client");
    assert_eq!(
        client.get_capabilities().await,
        Err(ClientError::Connection)
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
    assert_eq!(client.get_capabilities().await, Err(ClientError::Deadline));
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
        Err(ClientError::ResponseBodyTooLarge { bytes })
            if bytes > RESPONSE_BODY_BYTES_MAX
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
    assert_eq!(client.get_capabilities().await, Err(ClientError::Deadline));
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
            name: "demo".to_owned(),
            requirement: Some("^1".to_owned()),
            version: None,
        }],
    }
}

fn package_request() -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        name: "demo".to_owned(),
        version: "1.0.0".to_owned(),
    }
}

fn search_request() -> PackageSearchRequest {
    PackageSearchRequest {
        query: "demo".to_owned(),
        terms: vec![QueryTerm {
            text: "demo".to_owned(),
            phrase: false,
            prefix: false,
        }],
        identifiers: vec!["demo".to_owned()],
        include: None,
        packages: vec![package_request()],
        phase: PackageSearchRequestPhase::Precise,
        target: None,
    }
}

fn symbol_request() -> PackageSymbolRequest {
    PackageSymbolRequest {
        name: "demo".to_owned(),
        language: Some("rust".to_owned()),
        include: None,
        packages: vec![package_request()],
    }
}

async fn operation_client(mode: OperationFixture) -> (FixtureServer, GlobalClient) {
    let server = FixtureServer::start(FixtureMode::Operations(mode))
        .await
        .unwrap_or_else(|error| panic!("fixture server: {error}"));
    let client = GlobalClient::new(server.config())
        .unwrap_or_else(|error| panic!("fixture client: {error:?}"));
    (server, client)
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
    assert_eq!(
        result,
        Err(ClientError::InvalidResponseField {
            field: "resolution_accounting"
        })
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
    assert!(
        client
            .list_package_symbols(&symbol_request(), 20, None)
            .await
            .is_ok()
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/symbols")
    );
}

#[tokio::test]
async fn test_fixture_page_assembly_handles_cursor_and_revisions() {
    let (server, client) = operation_client(OperationFixture::SearchPages).await;
    let pages = client.search_packages_pages(&search_request(), 20).await;
    assert!(pages.is_ok());
    assert_eq!(pages.ok().map(|value| value.items.len()), Some(2));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 3);

    let (_server, client) = operation_client(OperationFixture::SearchPages).await;
    let pages = client
        .list_package_symbols_pages(&symbol_request(), 20)
        .await;
    assert!(pages.is_ok());
    assert_eq!(pages.ok().map(|value| value.items.len()), Some(2));

    let (_server, client) = operation_client(OperationFixture::PageRevisionChange).await;
    assert_eq!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ClientError::InvalidResponseField { field: "revision" })
    );

    let (server, client) = operation_client(OperationFixture::CandidateBound).await;
    let pages = client.search_packages_pages(&search_request(), 20).await;
    assert_eq!(pages.ok().map(|value| value.items.len()), Some(1));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_fixture_page_rejects_repeated_cursor_and_partial_failure() {
    let (_server, client) = operation_client(OperationFixture::RepeatedCursor).await;
    assert_eq!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ClientError::InvalidResponseField {
            field: "cursor_progress"
        })
    );

    let (server, client) = operation_client(OperationFixture::PartialFailure).await;
    assert!(matches!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ClientError::Http { meta, .. }) if meta.status == 503
    ));
    let requests = server.state.requests.load(Ordering::SeqCst);
    assert_eq!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ClientError::Connection)
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
        Err(ClientError::Http { meta, .. }) if meta.status == 503
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
    assert_eq!(
        client.search_packages_pages(&search_request(), 20).await,
        Err(ClientError::InvalidResponseField {
            field: "duplicate_item"
        })
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
    assert_eq!(
        client.search_packages(&search_request(), 20, None).await,
        Err(ClientError::InvalidResponseField { field: "package" })
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
        assert_eq!(
            client.search_packages(&search_request(), 20, None).await,
            Err(ClientError::InvalidResponseField { field })
        );
    }
}

#[tokio::test]
async fn test_fixture_rejects_invalid_capabilities() {
    let (_server, client) = operation_client(OperationFixture::InvalidPublication).await;
    assert_eq!(
        client.get_capabilities().await,
        Err(ClientError::InvalidResponseField {
            field: "publication_format"
        })
    );

    let (_server, client) = operation_client(OperationFixture::MissingRequiredField).await;
    assert_eq!(
        client.get_capabilities().await,
        Err(ClientError::InvalidResponseField {
            field: "required_search_fields"
        })
    );
}

#[tokio::test]
async fn test_fixture_enforces_active_server_bounds() {
    let (_server, client) = operation_client(OperationFixture::TightBounds).await;
    assert_eq!(
        client.search_packages(&search_request(), 3, None).await,
        Err(ClientError::InvalidRequest { field: "limit" })
    );
    assert!(matches!(
        client.resolve_package_context(&resolution_request()).await,
        Err(ClientError::ResponseBodyTooLarge { .. })
    ));

    let (server, client) = operation_client(OperationFixture::TightRequestBound).await;
    assert!(matches!(
        client.resolve_package_context(&resolution_request()).await,
        Err(ClientError::RequestBodyTooLarge { .. })
    ));
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 1);

    let (server, client) = operation_client(OperationFixture::TightSourceBound).await;
    let mut request = search_request();
    request.include = Some(vec!["source".to_owned()]);
    assert_eq!(
        client.search_packages(&request, 20, None).await,
        Err(ClientError::InvalidResponseField { field: "source" })
    );
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}
