//! The served tool failures, read as completed error results through a live
//! client. Registry-wire agreement needs no test here: the registry composes
//! the wire `ErrorCode` enum directly, so the two cannot name different code
//! sets.

mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::error::Error;
use std::fs;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rift_protocol::error::{ErrorCode, RetryDirective};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use workspace_client::{ToolFailure, failed_call, query};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[tokio::test]
async fn served_tool_failures_are_completed_error_results() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    workspace_client::await_workspace_ready(&client).await?;
    let ready = query::retrieval(&serde_json::from_value(json!({ "query": "beacon" }))?)?;
    workspace_client::query_after_population(&client, &ready).await?;

    let failing_requests = [
        ("search", json!({ "query": "" })),
        (
            "search",
            serde_json::to_value(query::retrieval(&serde_json::from_value(json!({
                "query": "beacon", "paths": { "include": ["[unclosed"] }
            }))?)?)?,
        ),
        ("get_symbol", json!({ "name": "beacon", "max_results": 0 })),
        ("facets", json!({ "path": "missing.rs", "position": 0 })),
        (
            "get_symbol",
            json!({ "name": "beacon", "include": {"history": true} }),
        ),
        (
            "get_symbol",
            json!({ "name": "beacon", "revision": "rift://revision/git/main" }),
        ),
        (
            "search",
            serde_json::to_value(query::retrieval(&serde_json::from_value(json!({
                "query": "beacon", "revision": "rift://revision/git/main"
            }))?)?)?,
        ),
        (
            "facets",
            json!({ "path": "lib.rs", "position": 0, "revision": "rift://revision/git/HEAD~1" }),
        ),
    ];
    for (tool, request) in failing_requests {
        let arguments = request
            .as_object()
            .cloned()
            .ok_or("request must be an object")?;
        let failure = failed_call(
            client
                .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
                .await,
        )
        .map_err(|error| format!("{tool} request {request}: {error}"))?;
        assert!(
            !failure.message.is_empty(),
            "{tool} failure message must not be empty: {}",
            failure.text
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// One tool call expected to complete with an error result, returning its facts.
async fn failing_tool_error(
    root: &std::path::Path,
    tool: &'static str,
    request: serde_json::Value,
) -> TestResult<ToolFailure> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    let arguments = request
        .as_object()
        .cloned()
        .ok_or("request must be an object")?;
    let outcome = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await;
    client.cancel().await?;
    server_task.await?;
    failed_call(outcome)
}

/// Discovers the fixture's symbol before testing the exact read surface.
async fn observed_symbol_call(
    root: &std::path::Path,
    mut request: serde_json::Value,
) -> TestResult<rmcp::model::CallToolResult> {
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"history-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\n",
    )?;
    let (client, server_task) = workspace_client::served_root(root).await?;
    let id = workspace_client::captured_seed(&client, "beacon", "lib.rs").await?;
    request["id"] = id.into();
    let outcome = client
        .call_tool(workspace_client::tool_request("get_symbol", &request))
        .await;
    client.cancel().await?;
    server_task.await?;
    Ok(outcome?)
}

async fn failing_observed_symbol_error(
    root: &std::path::Path,
    request: serde_json::Value,
) -> TestResult<ToolFailure> {
    let result = observed_symbol_call(root, request).await?;
    workspace_client::tool_failure(&result)
}

async fn assert_symbol_without_history(root: &std::path::Path) -> TestResult {
    let result = observed_symbol_call(root, json!({"include": ["history"]})).await?;
    assert_eq!(result.is_error, Some(false), "{result:?}");
    let answer = result.structured_content.ok_or("found symbol absent")?;
    assert!(answer["symbol"]["id"].is_string(), "{answer}");
    assert!(answer.get("history").is_none(), "{answer}");
    assert!(answer.get("warnings").is_none(), "{answer}");
    Ok(())
}

#[tokio::test]
async fn revision_read_without_a_repository_names_the_remedy() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure = failing_observed_symbol_error(
        directory.path(),
        json!({ "revision": "rift://revision/git/main" }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    assert_eq!(failure.retry, RetryDirective::OperatorAction);
    let message = &failure.message;
    assert!(
        message.contains("requires a git repository - run `git init`, or omit `revision`"),
        "the refusal must name the remedy: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn symbol_without_a_repository_keeps_the_found_declarations() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    assert_symbol_without_history(directory.path()).await
}

/// The symbol request accepts typed section sets and refuses the former boolean map.
#[tokio::test]
async fn symbol_include_refuses_the_former_boolean_map() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure =
        failing_observed_symbol_error(directory.path(), json!({ "include": {"source": true} }))
            .await?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
    assert_eq!(failure.retry, RetryDirective::Never);
    let message = &failure.message;
    assert!(
        message.contains("include"),
        "the refusal must name the field: {message}"
    );
    assert!(
        failure
            .text
            .lines()
            .any(|line| line.trim() == "rift.mcp.parameter_invalid"),
        "the refusal must retain its registered identity: {}",
        failure.text
    );
    Ok(())
}

#[tokio::test]
async fn symbol_history_disabled_keeps_the_found_declarations() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        format!(
            "{}[providers.history]\nenabled = false\n",
            hermetic_search::HERMETIC_TABLES
        ),
    )?;
    assert_symbol_without_history(directory.path()).await
}

#[tokio::test]
async fn revision_read_with_history_disabled_is_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        format!(
            "{}[providers.history]\nenabled = false\n",
            hermetic_search::HERMETIC_TABLES
        ),
    )?;
    let failure = failing_observed_symbol_error(
        directory.path(),
        json!({ "revision": "rift://revision/git/main" }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    let message = &failure.message;
    assert!(
        message.contains("providers.history disabled"),
        "the refusal must name the disabling configuration: {message}"
    );
    Ok(())
}

/// The SPARQL parser refuses a quote that opens and never closes. The refusal reaches the
/// caller as `invalid_request` with the syntax error's position.
#[tokio::test]
async fn an_unterminated_quote_refuses_the_search_with_a_position() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure = failing_tool_error(
        directory.path(),
        "search",
        json!({ "query": "ASK { ?symbol :name \"beacon }" }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
    assert_eq!(failure.retry, RetryDirective::Never);
    let message = &failure.message;
    assert!(
        message.contains("SPARQL query does not parse") && message.contains("line 1"),
        "the refusal must name the query syntax error and its position: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn a_non_string_query_refuses_the_search_with_registered_identity() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure = failing_tool_error(directory.path(), "search", json!({ "query": 42 })).await?;
    assert!(
        failure.message.contains("field query"),
        "the refusal must name the parameter at fault: {failure:?}"
    );
    assert!(
        failure
            .text
            .lines()
            .any(|line| line.trim() == "rift.mcp.parameter_invalid"),
        "the refusal must retain its registered identity: {}",
        failure.text
    );
    Ok(())
}
