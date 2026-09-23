//! Global package routing through served `get_symbol` reads.

mod hermetic_search;
#[allow(
    dead_code,
    reason = "shared integration fixture exposes helpers this suite does not use"
)]
mod workspace_client;

use std::{collections::VecDeque, fs, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use rmcp::model::ReadResourceRequestParams;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
use workspace_client::{ServedWorkspace, TestResult, served_workspace, tool_request};

const HELPER_SOURCE: &str = "pub fn helper_beacon() {}\n";
const LOCK_WITH_HELPER: &str = "version = 4\n\n[[package]]\nname = \"helper\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\ndependencies = [\n \"helper\",\n]\n";

struct DependentWorkspace {
    _helper: tempfile::TempDir,
    served: ServedWorkspace,
}

async fn served_dependent_workspace(configuration: Option<&str>) -> TestResult<DependentWorkspace> {
    let helper = tempfile::tempdir()?;
    fs::create_dir_all(helper.path().join("src"))?;
    fs::write(
        helper.path().join("Cargo.toml"),
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(helper.path().join("src/lib.rs"), HELPER_SOURCE)?;
    let manifest = format!(
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nhelper = {{ path = '{}' }}\n",
        helper.path().display()
    );
    let served = served_workspace(
        &[
            ("Cargo.toml", manifest.as_str()),
            ("Cargo.lock", LOCK_WITH_HELPER),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        configuration.map(str::to_owned),
    )
    .await?;
    Ok(DependentWorkspace {
        _helper: helper,
        served,
    })
}

struct GlobalFixture {
    endpoint: String,
    state: FixtureState,
    task: JoinHandle<()>,
}

#[derive(Clone, Copy)]
enum SymbolFixture {
    Valid,
    InvalidIdentity,
}

#[derive(Clone)]
struct FixtureState {
    symbol: SymbolFixture,
    resolution_delay: Option<Duration>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    search_responses: Arc<Mutex<VecDeque<Value>>>,
}

#[derive(Clone, Debug)]
struct ObservedRequest {
    uri: String,
    body: Option<Value>,
}

impl GlobalFixture {
    async fn start(symbol_fixture: SymbolFixture) -> Result<Self, std::io::Error> {
        Self::start_with_resolution_delay(symbol_fixture, None).await
    }

    async fn start_with_resolution_delay(
        symbol_fixture: SymbolFixture,
        resolution_delay: Option<Duration>,
    ) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let state = FixtureState {
            symbol: symbol_fixture,
            resolution_delay,
            requests: Arc::new(Mutex::new(Vec::new())),
            search_responses: Arc::new(Mutex::new(VecDeque::from([
                search_page(true),
                search_page(false),
            ]))),
        };
        let app = Router::new()
            .fallback(global_handler)
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            endpoint: format!("http://{address}/rift/rest"),
            state,
            task,
        })
    }

    async fn requests(&self) -> Vec<ObservedRequest> {
        self.state.requests.lock().await.clone()
    }
}

impl Drop for GlobalFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn global_handler(
    State(state): State<FixtureState>,
    request: axum::http::Request<Body>,
) -> Response {
    let uri = request.uri().to_string();
    let path = request.uri().path().to_owned();
    let Ok(bytes) = to_bytes(request.into_body(), 4_194_304).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let body = if bytes.is_empty() {
        None
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(body) => Some(body),
            Err(_) => return StatusCode::BAD_REQUEST.into_response(),
        }
    };
    state
        .requests
        .lock()
        .await
        .push(ObservedRequest { uri, body });
    if path.ends_with("/capabilities") {
        return json_response(&capabilities());
    }
    if path.ends_with("/resolutions") {
        if let Some(delay) = state.resolution_delay {
            tokio::time::sleep(delay).await;
        }
        return json_response(&resolution());
    }
    if path.ends_with("/search") {
        let Some(page) = state.search_responses.lock().await.pop_front() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        return json_response(&page);
    }
    if path.ends_with("/symbols") {
        return json_response(&symbols(state.symbol));
    }
    StatusCode::NOT_FOUND.into_response()
}

fn json_response(value: &Value) -> Response {
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn capabilities() -> Value {
    json!({
        "supported_package_managers": ["cargo"],
        "supported_features": ["resolutions", "search", "symbols", "documentation_search", "symbol_documentation"],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd",
        "required_search_fields": [
            "name", "qualified_name", "documentation", "signature", "declaration_source"
        ],
        "bounds": {
            "request_body_bytes_max": 4_194_304,
            "response_body_bytes_max": 33_554_432,
            "dependency_entries_max": 20000,
            "query_bytes_max": 4096,
            "query_terms_max": 32,
            "query_term_bytes_max": 256,
            "identifiers_max": 16,
            "identifier_bytes_max": 4096,
            "packages_max": 20000,
            "page_limit_min": 1,
            "page_limit_max": 200,
            "page_limit_default": 20,
            "cursor_bytes_max": 4096,
            "candidate_pool_max": 1000,
            "warnings_max": 32,
            "source_bytes_max": 1_048_576
        }
    })
}

fn resolution() -> Value {
    json!({
        "available_exact": [{"manager":"cargo","name":"demo","version":"1.0.0"}],
        "resolved_requirements": [],
        "missing_exact": [{"manager":"cargo","name":"absent","version":"1.0.0"}],
        "missing_requirements": [{
            "availability":"canonical",
            "manager":"cargo",
            "name":"wanted",
            "requirement":"^1"
        }]
    })
}

fn symbols(fixture: SymbolFixture) -> Value {
    let identity = match fixture {
        SymbolFixture::Valid => "rift://symbol/rust/src/lib.rs/helper_beacon",
        SymbolFixture::InvalidIdentity => "rift://symbol/rust/other.rs/helper_beacon",
    };
    json!({
        "items": [{
            "package": {"manager":"cargo","name":"demo","version":"1.0.0"},
            "symbol": {
                "id": identity,
                "kind": "function",
                "language": "rust",
                "name": "helper_beacon",
                "origin": {
                    "location": "dependency",
                    "package": {"manager":"cargo","name":"demo","version":"1.0.0"},
                    "source_kind": "authored"
                }
            },
            "unit": "rift://source/cargo/demo@1.0.0/src/lib.rs",
            "range": {"start": 0, "end": 4},
            "line": 1,
            "match_class": "qualified_exact"
        }],
        "next_cursor": null,
        "warnings": [{
            "code": "source_truncated",
            "detail": "source exceeded the active bound"
        }],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd"
    })
}

fn search_page(with_item: bool) -> Value {
    let items = if with_item {
        vec![json!({
            "target": "symbol",
            "package": {"manager":"cargo","name":"demo","version":"1.0.0"},
            "symbol": {
                "id": "rift://symbol/rust/src/lib.rs/helper_beacon",
                "kind": "function",
                "language": "rust",
                "name": "helper_beacon",
                "origin": {
                    "location": "dependency",
                    "package": {"manager":"cargo","name":"demo","version":"1.0.0"},
                    "source_kind": "authored"
                }
            },
            "unit": "rift://source/cargo/demo@1.0.0/src/lib.rs",
            "range": {"start": 0, "end": 4},
            "line": 1,
            "contributing_fields": ["qualified_name"],
            "match_class": "qualified_exact"
        })]
    } else {
        Vec::new()
    };
    json!({
        "items": items,
        "next_cursor": null,
        "warnings": if with_item {
            json!([{
                "code": "query_narrowed",
                "detail": "query exceeded the active term bound"
            }])
        } else {
            json!([])
        },
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1",
        "corpus_revision": "corpus-v1",
        "documentation_revision": "0123abcd"
    })
}

async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &'static str,
    args: Value,
) -> TestResult<Value> {
    let result = client.call_tool(tool_request(name, &args)).await?;
    result
        .structured_content
        .ok_or_else(|| format!("{name} must return structured content").into())
}

async fn get_symbol(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: Value,
) -> TestResult<Value> {
    call_tool(client, "get_symbol", args).await
}

#[tokio::test]
async fn refused_global_api_returns_typed_warning_and_local_fallback_counts() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"http://127.0.0.1:{port}/rift/rest\"\n\
         token_env = \"RIFT_TEST_GLOBAL_TOKEN\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\
         [dependencies]\npackages = [{{ manager = \"cargo\", name = \"demo\", version = \"1.0.0\" }}]\n"
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let answer = get_symbol(&client, json!({"name":"local_beacon","scope":"global"})).await?;
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let warning = warnings
        .iter()
        .find(|warning| warning["code"] == "global_api_unavailable")
        .ok_or_else(|| format!("missing global API warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "connection");
    assert_eq!(warning["fallback_indexed"], 0);
    assert_eq!(warning["fallback_unresolved"], 1);
    let rendered = serde_json::to_string(warning)?;
    assert!(!rendered.contains("RIFT_TEST_GLOBAL_TOKEN"));
    assert!(!rendered.contains("local_beacon"));
    assert_eq!(answer["hits"], json!([]));
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn request_deadline_bounds_global_resolution() -> TestResult {
    let fixture = GlobalFixture::start_with_resolution_delay(
        SymbolFixture::Valid,
        Some(Duration::from_secs(4)),
    )
    .await?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"5s\"\nconnect_timeout = \"100ms\"\n\n\
         [dependencies]\npackages = [{{ manager = \"cargo\", name = \"demo\", version = \"1.0.0\" }}]\n",
        fixture.endpoint
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let started = tokio::time::Instant::now();
    let answer = get_symbol(&client, json!({"name":"local_beacon","scope":"global"})).await?;
    assert!(started.elapsed() < Duration::from_secs(3));
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_api_unavailable")
        .ok_or_else(|| format!("missing deadline warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "timeout");
    assert_eq!(fixture.requests().await.len(), 2);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn successful_resolution_keeps_local_only_and_reports_missing_package() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
         [dependencies]\npackages = [\n  {{ manager = \"cargo\", name = \"demo\", version = \"1.0.0\" }},\n\
  {{ manager = \"cargo\", name = \"absent\", version = \"1.0.0\" }},\n\
  {{ manager = \"cargo\", name = \"wanted\", requirement = \"^1\" }}\n]\n",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = get_symbol(&client, json!({"name":"helper_beacon","scope":"all"})).await?;
    let hits = answer["hits"].as_array().ok_or("hits are an array")?;
    assert!(
        hits.iter().any(|hit| {
            hit["unit"] == "rift://source/cargo/helper@0.1.0/src/lib.rs"
                && hit["symbol"]["origin"]["package"]["name"] == "helper"
        }),
        "{answer:#}"
    );
    assert!(
        hits.iter()
            .any(|hit| hit["unit"] == "rift://source/cargo/demo@1.0.0/src/lib.rs"),
        "{answer:#}"
    );
    let missing = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "package_absent")
        .ok_or_else(|| format!("missing package warning: {answer:#}"))?;
    assert_eq!(
        missing["package"],
        json!({"manager":"cargo","name":"absent","version":"1.0.0"})
    );
    let missing_requirement = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "package_requirement_absent")
        .ok_or_else(|| format!("missing requirement warning: {answer:#}"))?;
    assert_eq!(
        missing_requirement["entry"],
        json!({
            "manager":"cargo",
            "name":"wanted",
            "requirement":"^1",
            "availability":"canonical"
        })
    );
    assert!(
        answer["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "package_unavailable" && warning["package"]["name"] == "absent"
            })
        }),
        "{answer:#}"
    );
    assert!(answer.to_string().contains("helper_beacon"));
    let page_warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_page_warning")
        .ok_or_else(|| format!("missing page warning: {answer:#}"))?;
    assert_eq!(page_warning["warning_code"], "source_truncated");
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn successful_search_uses_remote_phases_without_local_request_fields() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
         [dependencies]\npackages = [\n  {{ manager = \"cargo\", name = \"demo\", version = \"1.0.0\" }},\n\
  {{ manager = \"cargo\", name = \"absent\", version = \"1.0.0\" }},\n\
  {{ manager = \"cargo\", name = \"wanted\", requirement = \"^1\" }}\n]\n",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = call_tool(
        &client,
        "search",
        json!({"query":"helper_beacon demo","scope":"global"}),
    )
    .await?;
    assert!(
        answer["results"].as_array().is_some_and(|results| {
            results
                .iter()
                .any(|hit| hit["unit"] == "rift://source/cargo/demo@1.0.0/src/lib.rs")
        }),
        "{answer:#}"
    );
    assert!(answer["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|warning| {
            warning["code"] == "global_page_warning" && warning["warning_code"] == "query_narrowed"
        })
    }));

    let requests = fixture.requests().await;
    let resolution = requests
        .iter()
        .find(|request| request.uri.ends_with("/v1/resolutions"))
        .and_then(|request| request.body.as_ref())
        .ok_or("resolution request is recorded")?;
    let resolution_text = resolution.to_string();
    assert!(resolution_text.contains("demo"));
    assert!(resolution_text.contains("absent"));
    assert!(!resolution_text.contains("helper"));
    assert!(!resolution_text.contains("local_only"));

    let searches = requests
        .iter()
        .filter(|request| request.uri.contains("/v1/search?"))
        .collect::<Vec<_>>();
    assert_eq!(searches.len(), 2, "{requests:#?}");
    assert_eq!(
        searches[0].body.as_ref().map(|body| &body["phase"]),
        Some(&json!("precise"))
    );
    assert_eq!(
        searches[1].body.as_ref().map(|body| &body["phase"]),
        Some(&json!("broad"))
    );
    for request in searches {
        let body = request.body.as_ref().ok_or("search body is recorded")?;
        assert_eq!(
            body["packages"],
            json!([{"manager":"cargo","name":"demo","version":"1.0.0"}])
        );
        let rendered = body.to_string();
        assert!(!rendered.contains("src/lib.rs"));
        assert!(!rendered.contains("local_beacon"));
        assert!(!rendered.contains("RIFT_TEST_GLOBAL_TOKEN"));
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn local_file_and_map_reads_make_no_global_request() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let _ = get_symbol(&client, json!({"name":"local_beacon"})).await?;
    let _ = call_tool(
        &client,
        "search",
        json!({"query":"local_beacon","scope":"global","target":"file"}),
    )
    .await?;
    let _ = client
        .read_resource(ReadResourceRequestParams::new("rift://map".to_owned()))
        .await?;
    assert!(fixture.requests().await.is_empty());
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn invalid_remote_page_discards_lane_and_reports_local_fallback() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::InvalidIdentity).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
         [dependencies]\npackages = [{{ manager = \"cargo\", name = \"demo\", version = \"1.0.0\" }}]\n",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = get_symbol(&client, json!({"name":"helper_beacon","scope":"global"})).await?;
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_response_invalid")
        .ok_or_else(|| format!("missing invalid-response warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "invalid_response");
    assert_eq!(warning["fallback_indexed"], 1);
    assert_eq!(warning["fallback_unresolved"], 1);
    assert!(answer["hits"].as_array().is_some_and(|hits| {
        hits.iter().any(|hit| {
            hit["unit"] == "rift://source/cargo/helper@0.1.0/src/lib.rs"
                && hit["symbol"]["origin"]["package"]["name"] == "helper"
        })
    }));
    assert!(
        answer["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "package_unavailable" && warning["package"]["name"] == "demo"
            })
        }),
        "{answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
