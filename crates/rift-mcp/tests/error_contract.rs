//! The served wire errors, validated against the advertised `ErrorData`
//! schema through a live client. Registry-wire agreement needs no test here:
//! the registry composes the wire `ErrorCode` enum directly, so the two
//! cannot name different code sets.

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
