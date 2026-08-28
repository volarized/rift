//! Proves requests are gated on the workspace's `rift.toml` through a live
//! rmcp client: an invalid file fails every tool as `configuration_invalid`,
//! and fixing the file recovers without a restart.

mod hermetic_search;

use std::error::Error;
use std::fs;
use std::path::Path;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A `[[hooks]]` block whose `timeout` breaks its documented bound.
const INVALID_CONFIGURATION: &str = r#"
[[hooks]]
type = "command"
id = "tests"
kind = "test"
program = "cargo"
arguments = ["test"]
changed_paths = "none"
writes = "none"
working_directory = ""
environment = {}
timeout = "0ms"
output_limit = "4kb"
failure_severity = "error"
guarantees = []
determinism = "deterministic"
"#;

/// The same block with its timeout inside the bound.
const VALID_CONFIGURATION: &str = r#"
[[hooks]]
type = "command"
id = "tests"
kind = "test"
program = "cargo"
arguments = ["test"]
changed_paths = "none"
writes = "none"
working_directory = ""
environment = {}
timeout = "120s"
output_limit = "4kb"
failure_severity = "error"
guarantees = []
determinism = "deterministic"
"#;

/// A `[search.text]` block whose `max_chunk` breaks its documented 1kb..16mb bound.
const INVALID_TEXT_CONFIGURATION: &str = r#"
[search.text]
max_chunk = "1b"
"#;

/// A `[search.text]` block with an in-bound chunk limit.
const VALID_TEXT_CONFIGURATION: &str = r#"
[search.text]
max_chunk = "2mb"
"#;

/// One workspace whose `rift.toml` turns the semantic tier off and then carries
/// `configuration`, so the suite still proves what acceptance does with that block.
///
/// A fixture serving a block acceptance refuses falls back to the shipped tables,
/// the disabling one included; the server's own acquisition gate is what keeps that
/// case from reaching the hub.
fn workspace_with(configuration: Option<&str>) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    let mut contents = hermetic_search::SEMANTIC_DISABLED.to_owned();
    if let Some(configuration) = configuration {
        contents.push_str(configuration);
    }
    fs::write(directory.path().join("rift.toml"), contents)?;
    Ok(directory)
}

async fn client_for(root: &Path) -> TestResult<RunningService<RoleClient, ()>> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    Ok(().serve(client_transport).await?)
}

fn arguments(value: &serde_json::Value) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// Calls one tool and returns the typed wire error it must fail with.
async fn refused_call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    tool_arguments: serde_json::Value,
) -> TestResult<serde_json::Value> {
    let error = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments(&tool_arguments)?))
        .await
        .expect_err("the request must be refused while rift.toml is invalid");
    let rmcp::ServiceError::McpError(data) = error else {
        panic!("expected protocol-level McpError, got {error:?}");
    };
    data.data
        .ok_or_else(|| "wire error data must be present".into())
}

#[tokio::test]
async fn invalid_configuration_fails_reads_and_changes_typed() -> TestResult {
    let directory = workspace_with(Some(INVALID_CONFIGURATION))?;
    let client = client_for(directory.path()).await?;

    let read = refused_call(&client, "get_symbol", json!({"name": "beacon"})).await?;
    assert_eq!(read["code"], json!("configuration_invalid"));
    assert_eq!(read["retry"], json!("operator_action"));
    assert_eq!(read["phase"], json!("read"));
    let message = read["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("hooks.timeout") && message.contains("1..=3600000"),
        "the refusal must name the field and its range: {message}"
    );

    let change = refused_call(
        &client,
        "replace_symbol",
        json!({
            "symbol": "rift://symbol/rust/lib.rs/beacon",
            "body": "pub fn beacon() -> u8 { 7 }"
        }),
    )
    .await?;
    assert_eq!(change["code"], json!("configuration_invalid"));
    assert_eq!(change["phase"], json!("change"));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn fixing_the_file_recovers_without_a_restart() -> TestResult {
    let directory = workspace_with(Some(INVALID_CONFIGURATION))?;
    let client = client_for(directory.path()).await?;

    refused_call(&client, "get_symbol", json!({"name": "beacon"})).await?;
    let contents = format!(
        "{}{VALID_CONFIGURATION}",
        hermetic_search::SEMANTIC_DISABLED
    );
    fs::write(directory.path().join("rift.toml"), contents)?;

    let recovered = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        )
        .await?;
    assert_eq!(
        recovered.structured_content.ok_or("structured content")?["hits"][0]["symbol"]["name"],
        json!("beacon")
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn breaking_the_file_after_boot_gates_the_next_request() -> TestResult {
    let directory = workspace_with(Some(VALID_CONFIGURATION))?;
    let client = client_for(directory.path()).await?;

    let served = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        )
        .await?;
    assert!(served.structured_content.is_some());

    let contents = format!(
        "{}{INVALID_CONFIGURATION}",
        hermetic_search::SEMANTIC_DISABLED
    );
    fs::write(directory.path().join("rift.toml"), contents)?;
    let refused = refused_call(&client, "get_symbol", json!({"name": "beacon"})).await?;
    assert_eq!(refused["code"], json!("configuration_invalid"));

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_search_text_configuration_fails_reads_typed() -> TestResult {
    let directory = workspace_with(Some(INVALID_TEXT_CONFIGURATION))?;
    let client = client_for(directory.path()).await?;

    let read = refused_call(&client, "get_symbol", json!({"name": "beacon"})).await?;
    assert_eq!(read["code"], json!("configuration_invalid"));
    assert_eq!(read["retry"], json!("operator_action"));
    let message = read["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("search.text.max_chunk"),
        "the refusal must name the out-of-range field: {message}"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn valid_search_text_configuration_serves_normally() -> TestResult {
    let directory = workspace_with(Some(VALID_TEXT_CONFIGURATION))?;
    fs::write(directory.path().join("guide.rst"), "guide body")?;
    let client = client_for(directory.path()).await?;

    let served = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        )
        .await?;
    assert_eq!(
        served.structured_content.ok_or("structured content")?["hits"][0]["symbol"]["name"],
        json!("beacon")
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn missing_and_valid_files_serve_normally() -> TestResult {
    for configuration in [None, Some(VALID_CONFIGURATION)] {
        let directory = workspace_with(configuration)?;
        let client = client_for(directory.path()).await?;
        let served = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": "beacon"}))?),
            )
            .await?;
        assert!(served.structured_content.is_some());
        client.cancel().await?;
    }
    Ok(())
}
