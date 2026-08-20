//! Contract between the workspace error registry and the wire error surface.
//!
//! The registry in `rift-core` owns every stable code; the `ErrorCode` enum
//! in `rift-protocol` owns what the wire admits. The MCP layer is where the
//! two meet, so this suite holds them to one set of codes.

use std::collections::BTreeSet;
use std::error::Error;
use std::fs;

use rift_core::ErrorRegistry;
use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rift_protocol::error::{ErrorCode, ErrorData};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use schemars::schema_for;
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

#[test]
fn every_wire_code_has_a_registry_entry_and_cli_codes_stay_off_the_wire() {
    let registry: BTreeSet<&str> = ErrorRegistry::entries()
        .iter()
        .map(|descriptor| descriptor.code())
        .collect();
    let schema = serde_json::to_value(schema_for!(ErrorCode)).expect("ErrorCode schema serializes");
    let wire: BTreeSet<String> = schema["oneOf"]
        .as_array()
        .expect("ErrorCode schema must list its values under oneOf")
        .iter()
        .map(|value| {
            value["const"]
                .as_str()
                .expect("wire codes are strings")
                .to_owned()
        })
        .collect();
    for code in &wire {
        assert!(
            registry.contains(code.as_str()),
            "wire code {code} has no registry entry; add it to the registry \
             in the same change that puts it on the wire"
        );
    }
    for code in registry {
        assert!(
            wire.contains(code) || code.starts_with("update_") || code == "artifact_stale",
            "registry code {code} is neither on the wire nor in the CLI-only \
             namespace (`update_*`, `artifact_stale`); put it on the wire or \
             name its surface"
        );
    }
}

#[tokio::test]
async fn served_wire_errors_validate_against_the_error_data_schema() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())?;
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
            json!({ "query": "beacon", "scope": "dependencies" }),
        ),
        ("get_symbol", json!({ "name": "beacon", "limit": 0 })),
        ("nodes", json!({ "path": "missing.rs", "position": 0 })),
        (
            "replace_symbol",
            json!({ "symbol": "not-an-address", "body": "x" }),
        ),
        (
            "get_symbol",
            json!({ "name": "beacon", "include_history": true }),
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
