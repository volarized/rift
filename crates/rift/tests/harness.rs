//! The shared end-to-end harness: workspace fixtures, the real `rift mcp`
//! child, and the proxied call helpers every end-to-end suite drives
//! through.
//!
//! `mcp_proxy.rs` and `end_to_end.rs` each declare `mod harness;` and reach
//! this module's items through `crate::` from their own test bodies -
//! Cargo compiles each crate's integration test binary separately, so this
//! file is compiled once per binary that declares it, the same way
//! `engine_fixture.rs`, `live_engine_gate.rs`, and `rust_engine.rs` already
//! are. Adding a capability an end-to-end case needs means adding it here,
//! in this module's own vocabulary, not building a second harness.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ServiceExt as _, transport::child_process::TokioChildProcessBuilder};
use serde_json::json;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Bound on one proxied round trip that may include a server election. A
/// refusal can wait out two start windows - the warmup's and the request's
/// own - before it surfaces.
pub(crate) const PROXIED_CALL_MAX: Duration = Duration::from_mins(1);
/// Bound on one proxied call that starts and settles a language engine.
pub(crate) const PROXIED_ENGINE_CALL_MAX: Duration = Duration::from_mins(2);

/// Serializes the tests: the served port range is machine-global.
pub(crate) static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The declaration every non-engine fixture serves, and the file
/// referencing it.
pub(crate) const LIBRARY: &str = "pub fn beacon() {}\n";

/// A workspace fixture: one Rust source and a `rift.toml` whose
/// `[server]` idle timeout reaps any orphaned server within a minute.
pub(crate) fn workspace() -> TestResult<tempfile::TempDir> {
    laid_out_workspace(&[("lib.rs", LIBRARY)], "")
}

/// The cargo project the real rust-analyzer end-to-end cases serve:
/// rust-analyzer resolves nothing outside a cargo project, so the fixture
/// is one, with the same shape `rift-mcp`'s own `live_rust_analyzer.rs`
/// uses - a manifest whose `[lib]` path keeps every module file at the
/// tree root, and whose empty `[workspace]` table stops cargo climbing
/// out of the tempdir. `hub.rs` holds the declaration and `caller.rs`
/// imports and calls it under a different name, so a rename of the
/// declaration leaves no occurrence of the old name behind.
pub(crate) const RUST_PROJECT_MANIFEST: &str = "[package]\nname = \"rift_live_fixture\"\nversion = \"0.0.0\"\n\
     edition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n\
     [workspace]\n";
pub(crate) const RUST_PROJECT_ROOT: &str = "pub mod caller;\npub mod hub;\n";
pub(crate) const RUST_PROJECT_HUB: &str = "pub fn beacon(value: i32) -> i32 {\n    value\n}\n";
pub(crate) const RUST_PROJECT_CALLER: &str =
    "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n";
pub(crate) fn rust_project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", RUST_PROJECT_MANIFEST),
        ("lib.rs", RUST_PROJECT_ROOT),
        ("hub.rs", RUST_PROJECT_HUB),
        ("caller.rs", RUST_PROJECT_CALLER),
    ]
}

/// The cargo project fixture with a real `[engines.rust]` table appended,
/// serving `rust` through rust-analyzer.
pub(crate) fn rust_engine_workspace() -> TestResult<tempfile::TempDir> {
    laid_out_workspace(
        &rust_project(),
        &crate::rust_engine::rust_engine_configuration(),
    )
}

/// The `[search.semantic]` table every fixture here declares.
///
/// Rift ships the semantic tier on, so a fixture carrying no such table would acquire
/// the default model from the hub. A hermetic suite must not write into the developer's
/// own Hugging Face cache, and on a runner with no network a default-on tier would spend
/// its whole retry budget inside a detached task nobody waits on. `rift-mcp`'s
/// `tests/hermetic_search.rs` states the same policy for that crate's suites, and its
/// `live_semantic_search` suite is the one place the shipped default is exercised.
const SEMANTIC_DISABLED: &str = "[search.semantic]\ndisabled = true\n";

/// One fixture workspace holding `files` and a `rift.toml` carrying the disabled
/// semantic tier, the orphan-safety idle timeout, and `extra_toml` - an
/// `[engines.*]` table, a `[source]` policy, `[[hooks]]` entries, or any other
/// table a case needs beyond the two every fixture already carries.
pub(crate) fn laid_out_workspace(
    files: &[(&str, &str)],
    extra_toml: &str,
) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        let target = directory.path().join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, source)?;
    }
    fs::write(
        directory.path().join("rift.toml"),
        format!("{SEMANTIC_DISABLED}[server]\nidle_timeout = \"60s\"\n{extra_toml}"),
    )?;
    Ok(directory)
}

/// Stops the fixture's server when a test unwinds, best effort.
pub(crate) struct StopOnDrop {
    root: PathBuf,
}

impl StopOnDrop {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_rift"))
            .args(["server", "stop"])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Runs the real binary with `arguments` inside the fixture workspace,
/// off the async runtime.
pub(crate) async fn run_rift(root: &Path, arguments: &[&str]) -> TestResult<std::process::Output> {
    let root = root.to_owned();
    let arguments: Vec<String> = arguments.iter().map(|&argument| argument.into()).collect();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_rift"))
            .args(&arguments)
            .current_dir(&root)
            .stdin(Stdio::null())
            .output()
    })
    .await??;
    Ok(output)
}

pub(crate) fn require_success(output: &std::process::Output, what: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} must succeed: status {:?}, stdout {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
    .into())
}

/// Bounds one proxied operation by [`PROXIED_CALL_MAX`].
pub(crate) async fn within<Value>(
    what: &str,
    operation: impl Future<Output = Value>,
) -> TestResult<Value> {
    tokio::time::timeout(PROXIED_CALL_MAX, operation)
        .await
        .map_err(|_elapsed| format!("timed out waiting for {what}").into())
}

/// The base `rift mcp` child command for one fixture workspace, before either
/// the rmcp transport wrapper or a raw-pipe session spawns it.
fn base_command(root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_rift"));
    command
        .arg("mcp")
        .current_dir(root)
        .env("RUST_LOG", "rift=info,rift_mcp=info,rift_server=info");
    command
}

/// The `rift mcp` child command for one fixture workspace.
pub(crate) fn proxy_command(root: &Path) -> TokioChildProcessBuilder {
    TokioChildProcess::builder(base_command(root))
}

/// One connected `rift mcp` child with its stderr discarded.
pub(crate) async fn proxy_client(root: &Path) -> TestResult<RunningService<RoleClient, ()>> {
    let (transport, _stderr) = proxy_command(root).stderr(Stdio::null()).spawn()?;
    Ok(().serve(transport).await?)
}

pub(crate) fn arguments(
    value: &serde_json::Value,
) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// Most attempts one proxied call spends on a retryable refusal.
pub(crate) const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// One proxied tool call returning its structured result, retrying the
/// refusal the server advertises as `retry: same_request`: an applied
/// change moves the index, and a request whose snapshot predates the move
/// is refused rather than served stale.
pub(crate) async fn proxied_call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    proxied_call_within(client, name, call_arguments, PROXIED_CALL_MAX).await
}

/// One proxied live-engine call under [`PROXIED_ENGINE_CALL_MAX`].
pub(crate) async fn proxied_engine_call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    proxied_call_within(client, name, call_arguments, PROXIED_ENGINE_CALL_MAX).await
}

/// One retrying proxied call under its caller-owned wall-clock bound.
async fn proxied_call_within(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
    timeout: Duration,
) -> TestResult<serde_json::Value> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        let params = CallToolRequestParams::new(name).with_arguments(arguments(call_arguments)?);
        match tokio::time::timeout(timeout, client.call_tool(params))
            .await
            .map_err(|_elapsed| format!("timed out waiting for {name}"))?
        {
            Ok(called) => {
                return called
                    .structured_content
                    .ok_or_else(|| format!("{name} must return structured content").into());
            }
            Err(rmcp::ServiceError::McpError(error))
                if error
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(format!("the server kept refusing {name}").into())
}
