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
use workspace_client::{ToolFailure, failed_call};

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

    let failing_requests = [
        ("search", json!({ "query": "" })),
        (
            "search",
            json!({ "query": "beacon", "paths": { "include": ["[unclosed"] } }),
        ),
        ("get_symbol", json!({ "name": "beacon", "limit": 0 })),
        ("nodes", json!({ "path": "missing.rs", "position": 0 })),
        (
            "get_symbol",
            json!({ "name": "beacon", "include": ["history"] }),
        ),
        ("get_symbol", json!({ "name": "beacon", "rev": "main" })),
        ("search", json!({ "query": "beacon", "rev": "main" })),
        (
            "nodes",
            json!({ "path": "lib.rs", "position": 0, "rev": "HEAD~1" }),
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
        )?;
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

#[tokio::test]
async fn revision_read_without_a_repository_names_the_remedy() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure = failing_tool_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "rev": "main" }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    assert_eq!(failure.retry, RetryDirective::OperatorAction);
    let message = &failure.message;
    assert!(
        message.contains("requires a git repository - run `git init`, or omit `rev`"),
        "the refusal must name the remedy: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn symbol_history_without_a_repository_names_the_remedy() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure = failing_tool_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "include": ["history"] }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    assert_eq!(failure.retry, RetryDirective::OperatorAction);
    let message = &failure.message;
    assert!(
        message.contains("requires a git repository - run `git init`"),
        "the refusal must name the remedy: {message}"
    );
    Ok(())
}

#[tokio::test]
async fn symbol_history_with_history_disabled_is_refused() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        format!(
            "{}[providers.history]\nenabled = false\n",
            hermetic_search::HERMETIC_TABLES
        ),
    )?;
    let failure = failing_tool_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "include": ["history"] }),
    )
    .await?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    let message = &failure.message;
    assert!(
        message.contains("symbol history (providers.history disabled)"),
        "the refusal must name the disabling configuration: {message}"
    );
    Ok(())
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
    let failure = failing_tool_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "rev": "main" }),
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

/// The bounded query parser refuses a quote that opens and never closes, and the refusal
/// reaches the caller as this request's own `invalid_request` naming `query` - never a
/// degraded ranking that answers as though the query had parsed.
#[tokio::test]
async fn an_unterminated_quote_refuses_the_search_naming_query() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let failure =
        failing_tool_error(directory.path(), "search", json!({ "query": "\"beacon" })).await?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest);
    assert_eq!(failure.retry, RetryDirective::Never);
    let message = &failure.message;
    assert!(
        message.contains("field query"),
        "the refusal must name the parameter at fault: {message}"
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
