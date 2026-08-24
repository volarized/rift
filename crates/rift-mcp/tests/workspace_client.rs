//! Shared scaffolding for the fake-engine integration suites.
//!
//! Every suite wires rift-lsp's `fake_engine` binary into the workspace's
//! own `rift.toml` through an overlaid `PATH`, exactly as an operator's
//! `[engines.<name>]` table would resolve a real engine, and drives the
//! tools through a live rmcp client.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The directory holding the compiled `fake_engine` binary.
///
/// A test binary runs from `target/<profile>/deps`, and Cargo places
/// another crate's binary one level up. Running the suite with `rift-lsp`
/// in the invocation - the workspace suite does - builds the binary before
/// any test runs.
pub(crate) fn fake_engine_directory() -> PathBuf {
    let mut directory = std::env::current_exe().expect("the test binary has a path");
    directory.pop();
    if directory.ends_with("deps") {
        directory.pop();
    }
    assert!(
        directory.join("fake_engine").exists(),
        "fake_engine is missing from {}: build it first with `cargo test -p rift-lsp`",
        directory.display(),
    );
    directory
}

/// One `[engines.fake]` table resolving `fake_engine` through an overlaid
/// `PATH`, claiming `rust`.
pub(crate) fn engine_configuration(behavior: &str, request_timeout: &str) -> String {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let path_overlay = format!("{}:{inherited}", fake_engine_directory().display());
    format!(
        "[engines.fake]\nprogram = \"fake_engine\"\narguments = [\"{behavior}\"]\n\
         languages = [\"rust\"]\nrequest_timeout = \"{request_timeout}\"\n\n\
         [engines.fake.environment]\nPATH = \"{path_overlay}\"\n"
    )
}

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

/// The same engine table with a lifecycle log and a narrow retry budget.
///
/// The log is how a test counts the engine's requests: the behaviors that
/// act once and then serve read their own count back from it, and the
/// assertions read the same lines. The waits are held at a millisecond so
/// the suite spends no time on them; the shape of the growing wait is
/// proven by the policy's own unit tests.
pub(crate) fn counted(configuration: &str, log: &Path, attempts: u64) -> String {
    format!(
        "{configuration}RIFT_FAKE_ENGINE_LIFECYCLE_LOG = \"{}\"\n\n\
         [engines.fake.retry]\nattempts = {attempts}\ndelay = \"1ms\"\n\
         delay_limit = \"1ms\"\n",
        log.display()
    )
}

/// Lines of one lifecycle event the engine recorded.
pub(crate) fn recorded(log: &Path, event: &str) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == event)
        .count()
}
