//! Proves the Streamable-HTTP serving core through live rmcp clients: token
//! auth, tool round trips, the stop route, the idle timeout, and shutdown.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rift_mcp::{HttpServer, serve_http};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::json;
use tokio_util::sync::CancellationToken;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Bounds every shutdown wait so a hung server fails the test, not CI.
const STOP_DEADLINE: Duration = Duration::from_secs(10);

/// A `[[hooks]]` block whose `timeout` breaks its documented bound.
const INVALID_CONFIGURATION: &str = r#"
[[hooks]]
type = "command"
id = "tests"
kind = "test"
program = "cargo"
arguments = ["test"]
changed_paths = "none"
working_directory = ""
environment = {}
timeout = "0ms"
output_limit = "4kb"
guarantees = []
determinism = "deterministic"
"#;

/// A `[server]` table at the shortest accepted idle span.
const SHORT_IDLE_CONFIGURATION: &str = r#"
[server]
idle_timeout = "1s"
"#;

/// A `[server]` table selecting a custom range, away from the default one.
const RANGED_PORT_CONFIGURATION: &str = r"
[server]
port_range = { min = 11500, max = 11510 }
";

/// A `[server]` table pinning one exact port.
const PINNED_PORT_CONFIGURATION: &str = r"
[server]
port = 11777
";

fn workspace_with(configuration: Option<&str>) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    if let Some(contents) = configuration {
        fs::write(directory.path().join("rift.toml"), contents)?;
    }
    Ok(directory)
}

async fn served(root: &Path) -> TestResult<(CancellationToken, HttpServer)> {
    let shutdown = CancellationToken::new();
    let server = serve_http(root, shutdown.clone()).await?;
    Ok((shutdown, server))
}

fn mcp_url(server: &HttpServer) -> String {
    format!("http://127.0.0.1:{}/api/mcp", server.port())
}

fn stop_url(server: &HttpServer) -> String {
    format!("http://127.0.0.1:{}/api/stop", server.port())
}

async fn connected_client(server: &HttpServer) -> TestResult<RunningService<RoleClient, ()>> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(mcp_url(server))
            .auth_header(server.token().to_owned()),
    );
    Ok(().serve(transport).await?)
}

fn arguments(value: &serde_json::Value) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

async fn beacon_lookup(client: &RunningService<RoleClient, ()>) -> TestResult<serde_json::Value> {
    let served = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        )
        .await?;
    served
        .structured_content
        .ok_or_else(|| "structured content must be served".into())
}

async fn stopped_within_deadline(server: HttpServer) -> TestResult {
    tokio::time::timeout(STOP_DEADLINE, server.stopped())
        .await
        .map_err(|_elapsed| format!("server must stop within {STOP_DEADLINE:?}"))??;
    Ok(())
}

#[tokio::test]
async fn serves_tools_and_stops_on_external_cancel() -> TestResult {
    let directory = workspace_with(None)?;
    let (shutdown, server) = served(directory.path()).await?;
    let client = connected_client(&server).await?;

    let listing = client.list_tools(None).await?;
    let names: Vec<&str> = listing
        .tools
        .iter()
        .map(|tool| tool.name.as_ref())
        .collect();
    for expected in [
        "get_symbol",
        "search",
        "nodes",
        "replace_symbol",
        "insert_symbol",
        "replace_node",
        "patch",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }

    let lookup = beacon_lookup(&client).await?;
    assert_eq!(lookup["hits"][0]["symbol"]["name"], json!("beacon"));

    client.cancel().await?;
    shutdown.cancel();
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn wrong_or_missing_bearer_is_refused_with_the_scheme() -> TestResult {
    let directory = workspace_with(None)?;
    let (shutdown, server) = served(directory.path()).await?;
    let http = reqwest::Client::new();

    let wrong_token = http
        .post(mcp_url(&server))
        .header("Authorization", "Bearer not-the-minted-token")
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await?;
    assert_eq!(wrong_token.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        wrong_token
            .headers()
            .get("WWW-Authenticate")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer")
    );

    let missing_header = http
        .post(mcp_url(&server))
        .header("Content-Type", "application/json")
        .body("{}")
        .send()
        .await?;
    assert_eq!(missing_header.status(), reqwest::StatusCode::UNAUTHORIZED);

    let unauthorized_stop = http.post(stop_url(&server)).send().await?;
    assert_eq!(
        unauthorized_stop.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the stop route must sit behind the same bearer gate"
    );

    shutdown.cancel();
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn authorized_stop_route_stops_the_server() -> TestResult {
    let directory = workspace_with(None)?;
    let (_shutdown, server) = served(directory.path()).await?;
    let stop_url = stop_url(&server);
    let authorization = format!("Bearer {}", server.token());
    let http = reqwest::Client::new();

    let accepted = http
        .post(&stop_url)
        .header("Authorization", &authorization)
        .send()
        .await?;
    assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
    assert!(accepted.bytes().await?.is_empty(), "stop answers empty");

    stopped_within_deadline(server).await?;

    // A stop after shutdown must not panic anything. The released port may
    // already carry another workspace's server, so no answer is asserted:
    // a refused connection and a foreign server's refusal are both fine.
    let _ = http
        .post(&stop_url)
        .header("Authorization", &authorization)
        .send()
        .await;
    Ok(())
}

#[tokio::test]
async fn idle_timeout_stops_a_quiet_server() -> TestResult {
    let directory = workspace_with(Some(SHORT_IDLE_CONFIGURATION))?;
    let (_shutdown, server) = served(directory.path()).await?;
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn invalid_configuration_still_serves_typed_refusals() -> TestResult {
    let directory = workspace_with(Some(INVALID_CONFIGURATION))?;
    let (shutdown, server) = served(directory.path()).await?;
    let client = connected_client(&server).await?;

    let error = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        )
        .await
        .expect_err("the request must be refused while rift.toml is invalid");
    let rmcp::ServiceError::McpError(data) = error else {
        panic!("expected protocol-level McpError, got {error:?}");
    };
    let wire = data.data.ok_or("wire error data must be present")?;
    assert_eq!(wire["code"], json!("configuration_invalid"));
    assert_eq!(wire["retry"], json!("operator_action"));

    client.cancel().await?;
    shutdown.cancel();
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn two_concurrent_clients_are_both_served() -> TestResult {
    let directory = workspace_with(None)?;
    let (shutdown, server) = served(directory.path()).await?;
    let first = connected_client(&server).await?;
    let second = connected_client(&server).await?;

    let (first_lookup, second_lookup) = tokio::join!(beacon_lookup(&first), beacon_lookup(&second));
    assert_eq!(first_lookup?["hits"][0]["symbol"]["name"], json!("beacon"));
    assert_eq!(second_lookup?["hits"][0]["symbol"]["name"], json!("beacon"));

    first.cancel().await?;
    second.cancel().await?;
    shutdown.cancel();
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn configured_port_selection_is_honored() -> TestResult {
    let ranged = workspace_with(Some(RANGED_PORT_CONFIGURATION))?;
    let (shutdown, server) = served(ranged.path()).await?;
    assert!(
        (11_500..=11_510).contains(&server.port()),
        "a configured range must bound the bind, got port {}",
        server.port()
    );
    shutdown.cancel();
    stopped_within_deadline(server).await?;

    let pinned = workspace_with(Some(PINNED_PORT_CONFIGURATION))?;
    let (shutdown, server) = served(pinned.path()).await?;
    assert_eq!(
        server.port(),
        11_777,
        "a pinned port must be the bound port"
    );
    shutdown.cancel();
    stopped_within_deadline(server).await
}

#[tokio::test]
async fn foreign_host_and_origin_are_refused_at_the_boundary() -> TestResult {
    let directory = workspace_with(None)?;
    let (shutdown, server) = served(directory.path()).await?;
    let http = reqwest::Client::new();

    let forged_host = http
        .post(stop_url(&server))
        .header(reqwest::header::HOST, "rift.example:12345")
        .bearer_auth(server.token())
        .send()
        .await?;
    assert_eq!(forged_host.status(), reqwest::StatusCode::BAD_REQUEST);

    let forged_origin = http
        .post(stop_url(&server))
        .header(reqwest::header::ORIGIN, "http://rift.example")
        .bearer_auth(server.token())
        .send()
        .await?;
    assert_eq!(forged_origin.status(), reqwest::StatusCode::BAD_REQUEST);

    let loopback_origin = http
        .post(stop_url(&server))
        .header(reqwest::header::ORIGIN, "http://127.0.0.1:5500")
        .bearer_auth(server.token())
        .send()
        .await?;
    assert_eq!(loopback_origin.status(), reqwest::StatusCode::ACCEPTED);
    shutdown.cancel();
    stopped_within_deadline(server).await
}
