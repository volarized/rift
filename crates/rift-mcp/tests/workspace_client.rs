//! Shared scaffolding for the engine integration suites.
//!
//! Every suite builds a workspace whose own `rift.toml` carries an
//! `[engines.<name>]` table - the scripted fake engine, or the real one -
//! and drives the tools through a live rmcp client. Engine-specific
//! helpers live beside this module: `fake_engine.rs` for the scripted
//! suites, `live.rs` for the gated live suite.

use std::error::Error;
use std::fs;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Builds one workspace of `files`, optionally with an engine table, and
/// serves it to one client.
pub(crate) async fn served_workspace(
    files: &[(&str, &str)],
    engine: Option<String>,
) -> TestResult<(
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        fs::write(directory.path().join(name), source)?;
    }
    if let Some(configuration) = engine {
        fs::write(directory.path().join("rift.toml"), configuration)?;
    }
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    Ok((directory, client, server_task))
}

/// One tool call's request parameters from a JSON argument object.
pub(crate) fn tool_request(name: &'static str, arguments: &Value) -> CallToolRequestParams {
    let arguments = arguments
        .as_object()
        .cloned()
        .expect("tool arguments are an object");
    CallToolRequestParams::new(name).with_arguments(arguments)
}

/// Most attempts one request retries before giving up on acceptance.
const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// Calls the tool, retrying the refusal the server advertises as
/// `retry: same_request`: a change the engine itself wrote to the tree can
/// move the index between one request's snapshot and its acceptance.
pub(crate) async fn call_retrying_acceptance(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    params: CallToolRequestParams,
) -> TestResult<Value> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        match client.call_tool(params.clone()).await {
            Ok(result) => {
                return result
                    .structured_content
                    .ok_or_else(|| "the tool must return structured content".into());
            }
            Err(rmcp::ServiceError::McpError(error))
                if error
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("the server kept refusing a retryable request".into())
}
