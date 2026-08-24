//! Shared scaffolding for the engine integration suites.
//!
//! Every suite builds a workspace whose own `rift.toml` carries an
//! `[engines.<name>]` table - the scripted fake engine, or the real one -
//! and drives the tools through a live rmcp client. Engine-specific
//! helpers live beside this module: `fake_engine.rs` for the scripted
//! suites, `live_engine_gate.rs` for the gated live suite.

use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// One served workspace: the directory that holds it, the client speaking
/// to it, and the task serving it.
pub(crate) type ServedWorkspace = (
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
);

/// Builds one workspace of `files`, optionally with an engine table, and
/// serves it to one client.
pub(crate) async fn served_workspace(
    files: &[(&str, &str)],
    engine: Option<String>,
) -> TestResult<ServedWorkspace> {
    let directory = laid_out_workspace(files, engine)?;
    let (client, server_task) = served_root(directory.path()).await?;
    Ok((directory, client, server_task))
}

/// The same workspace, served under a root spelled relative to the process
/// working directory.
///
/// This is the spelling the CLI hands the server: `rift mcp` and
/// `rift server start` both serve the working directory, which they name
/// `.`. Reads and writes below the root resolve against that directory
/// either way, so only the engine tier can tell the two spellings apart -
/// it is addressed in `file://` URIs, which carry no working directory.
/// A test cannot change the process directory without disturbing the
/// suites running beside it, so it spells the same relative form the long
/// way, as `..` segments down to the filesystem root and back up.
pub(crate) async fn served_relative_workspace(
    files: &[(&str, &str)],
    engine: Option<String>,
) -> TestResult<ServedWorkspace> {
    let directory = laid_out_workspace(files, engine)?;
    let (client, server_task) = served_root(&relative_spelling(directory.path())?).await?;
    Ok((directory, client, server_task))
}

/// One temporary workspace holding `files` and, when given, a `rift.toml`.
fn laid_out_workspace(
    files: &[(&str, &str)],
    engine: Option<String>,
) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        fs::write(directory.path().join(name), source)?;
    }
    if let Some(configuration) = engine {
        fs::write(directory.path().join("rift.toml"), configuration)?;
    }
    Ok(directory)
}

/// Serves the workspace at `root` to one client over an in-process duplex.
async fn served_root(
    root: &Path,
) -> TestResult<(
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    Ok((client, server_task))
}

/// The path from the process working directory to `target`, as one `..`
/// segment per directory above the working directory followed by the
/// target's own segments.
fn relative_spelling(target: &Path) -> TestResult<PathBuf> {
    let named = |component: &Component<'_>| matches!(component, Component::Normal(_));
    let current = std::env::current_dir()?;
    let mut spelling = PathBuf::new();
    for _ in current.components().filter(named) {
        spelling.push("..");
    }
    spelling.extend(target.components().filter(named));
    Ok(spelling)
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
