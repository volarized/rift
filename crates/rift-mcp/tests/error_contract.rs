//! The served wire errors, validated against the advertised `ErrorData`
//! schema through a live client. Registry-wire agreement needs no test here:
//! the registry composes the wire `ErrorCode` enum directly, so the two
//! cannot name different code sets.

mod hermetic_search;

use std::error::Error;
use std::fs;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rift_protocol::error::ErrorData;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use schemars::schema_for;
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[tokio::test]
async fn served_wire_errors_validate_against_the_error_data_schema() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
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
    let schema = serde_json::to_value(schema_for!(ErrorData))?;
    let validator = jsonschema::validator_for(&schema)?;

    let failing_requests = [
        ("search", json!({ "query": "" })),
        (
            "search",
            json!({ "query": "beacon", "paths": { "include": ["[unclosed"] } }),
        ),
        ("get_symbol", json!({ "name": "beacon", "limit": 0 })),
        ("nodes", json!({ "path": "missing.rs", "position": 0 })),
        (
            "replace_symbol",
            json!({ "symbol": "not-an-address", "body": "x" }),
        ),
        ("patch", json!({ "patch": "not a diff" })),
        (
            "replace_node",
            json!({ "node": "not-an-address", "body": "x" }),
        ),
        (
            "insert_symbol",
            json!({ "anchor": "not-an-address", "position": "after", "body": "x" }),
        ),
        // Both node writers resolve their address through `resolve_node_range`, so a range
        // that names no indexed node refuses identically on each: in bounds but landing on
        // no node, and past the end of the file. The witness is well formed in every one,
        // so only the range can explain the refusal.
        (
            "replace_node",
            json!({ "node": "rift://node/rust/lib.rs@0-5#00000000", "body": "x" }),
        ),
        (
            "remove_node",
            json!({ "node": "rift://node/rust/lib.rs@0-5#00000000", "force": false }),
        ),
        (
            "replace_node",
            json!({ "node": "rift://node/rust/lib.rs@0-999#00000000", "body": "x" }),
        ),
        (
            "remove_node",
            json!({ "node": "rift://node/rust/lib.rs@0-999#00000000", "force": false }),
        ),
        (
            "get_symbol",
            json!({ "name": "beacon", "include_history": true }),
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
        let error = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
            .await
            .expect_err("the request must be rejected");
        let rmcp::ServiceError::McpError(data) = error else {
            panic!("expected protocol-level McpError, got {error:?}");
        };
        let wire = data.data.ok_or("wire error data must be present")?;
        let failures: Vec<String> = validator
            .iter_errors(&wire)
            .map(|failure| failure.to_string())
            .collect();
        assert!(
            failures.is_empty(),
            "{tool} wire error must validate against the ErrorData schema: \
             {failures:#?}\ninstance: {wire:#}"
        );
        let parsed: ErrorData = serde_json::from_value(wire)?;
        assert!(
            !parsed.message.is_empty(),
            "wire error message must not be empty"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// One tool call expected to fail, returning its wire `ErrorData` payload.
async fn failing_wire_error(
    root: &std::path::Path,
    tool: &'static str,
    request: serde_json::Value,
) -> TestResult<serde_json::Value> {
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
    let error = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await
        .expect_err("the request must be rejected");
    client.cancel().await?;
    server_task.await?;
    let rmcp::ServiceError::McpError(data) = error else {
        panic!("expected protocol-level McpError, got {error:?}");
    };
    Ok(data.data.ok_or("wire error data must be present")?)
}

#[tokio::test]
async fn raw_patch_without_headers_names_the_minimal_envelope() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;

    let wire =
        failing_wire_error(directory.path(), "patch", json!({ "patch": "not a diff" })).await?;
    let parsed: ErrorData = serde_json::from_value(wire)?;
    assert!(
        parsed.message.contains("--- a/src/lib.rs"),
        "patch error must show the minimal original header: {}",
        parsed.message
    );
    assert!(
        parsed.message.contains("+++ b/src/lib.rs"),
        "patch error must show the minimal replacement header: {}",
        parsed.message
    );
    Ok(())
}

#[tokio::test]
async fn apply_patch_envelope_is_refused_naming_the_unified_diff_form() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;

    let envelope = "*** Begin Patch\n*** Update File: lib.rs\n@@\n-pub fn beacon() {}\n+pub fn beacon() { }\n*** End Patch\n";
    let wire = failing_wire_error(directory.path(), "patch", json!({ "patch": envelope })).await?;
    let parsed: ErrorData = serde_json::from_value(wire)?;
    assert!(
        parsed.message.contains("*** Begin Patch"),
        "patch error must name the envelope that arrived: {}",
        parsed.message
    );
    assert!(
        parsed.message.contains("--- a/"),
        "patch error must name the unified-diff form to send: {}",
        parsed.message
    );
    Ok(())
}

#[tokio::test]
async fn revision_read_without_a_repository_names_the_remedy() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    let wire = failing_wire_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "rev": "main" }),
    )
    .await?;
    assert_eq!(wire["code"], json!("capability_unavailable"));
    assert_eq!(wire["retry"], json!("operator_action"));
    let message = wire["message"].as_str().ok_or("message must be a string")?;
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
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    let wire = failing_wire_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "include_history": true }),
    )
    .await?;
    assert_eq!(wire["code"], json!("capability_unavailable"));
    assert_eq!(wire["retry"], json!("operator_action"));
    let message = wire["message"].as_str().ok_or("message must be a string")?;
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
            hermetic_search::SEMANTIC_DISABLED
        ),
    )?;
    let wire = failing_wire_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "include_history": true }),
    )
    .await?;
    assert_eq!(wire["code"], json!("capability_unavailable"));
    let message = wire["message"].as_str().ok_or("message must be a string")?;
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
            hermetic_search::SEMANTIC_DISABLED
        ),
    )?;
    let wire = failing_wire_error(
        directory.path(),
        "get_symbol",
        json!({ "name": "beacon", "rev": "main" }),
    )
    .await?;
    assert_eq!(wire["code"], json!("capability_unavailable"));
    let message = wire["message"].as_str().ok_or("message must be a string")?;
    assert!(
        message.contains("providers.history disabled"),
        "the refusal must name the disabling configuration: {message}"
    );
    Ok(())
}
