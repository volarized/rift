//! Real-binary contract of the `rift mcp` stdio proxy, and the end-to-end
//! lane: a real MCP client, the real `rift` binary run as an agent runs
//! it, a real elected server, and - gated behind `RIFT_ENGINE_LIVE` - a
//! real language engine, over a real temp-directory workspace.
//!
//! Every test drives the compiled `rift` binary as an MCP stdio child
//! against a throwaway workspace fixture. The proxy tests prove election,
//! adoption, sharing, and re-election; the engine tests prove one applied
//! change's observable outcome - the bytes a real engine rewrote on disk,
//! or the finding it attached - through the whole chain: proxy, server,
//! and engine all real processes, no scripted engine and no in-process
//! shortcut. The tests serialize on one async mutex: the servers share the
//! loopback election port range. Each fixture's `rift.toml` accepts a
//! 60-second idle timeout as an orphan-safety net, and a drop guard stops
//! any server a failed test leaves behind.
//!
//! **Adding a case to the end-to-end lane.** Lay out a workspace with
//! [`laid_out_workspace`] (files, plus an engine table when the case needs
//! one - build it from a fixture in `engine_fixture.rs`/`rust_engine.rs`,
//! following the pattern those two modules already set for rust), connect
//! to it with [`proxy_client`], drive it with [`proxied_call`], and gate
//! the test behind `live_engine_gate::engine_live` when it needs a real
//! engine. `proxy_client` is the one entry point that spawns the real
//! `rift mcp` binary; every case shares it, and no case may spawn a
//! process of its own to stand in for the server or the engine.

mod engine_fixture;
mod live_engine_gate;
mod rust_engine;

use std::error::Error;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use rift_mcp::{PRESENCE_POLL_INTERVAL, ServerPresence, claim, probe};
use rift_protocol::lock::{
    SERVER_LOCK_FILE_NAME, SERVER_PORT_MAX, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, ServerLock,
};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt as _, TokioChildProcess};
use rmcp::{ServiceExt as _, transport::child_process::TokioChildProcessBuilder};
use serde_json::json;
use tokio::io::AsyncReadExt as _;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Poll attempts while waiting on a server to disappear: 10 seconds at
/// [`PRESENCE_POLL_INTERVAL`].
const GONE_POLL_ATTEMPT_COUNT: u32 = 100;
/// Bound on one proxied round trip that may include a server election. A
/// refusal can wait out two start windows - the warmup's and the request's
/// own - before it surfaces.
const PROXIED_CALL_MAX: Duration = Duration::from_mins(1);

/// The tools the workspace server advertises, in served order.
const SERVED_TOOL_NAMES: [&str; 11] = [
    "get_symbol",
    "insert_symbol",
    "move_file",
    "nodes",
    "patch",
    "remove_node",
    "remove_symbol",
    "rename_symbol",
    "replace_node",
    "replace_symbol",
    "search",
];

/// Serializes the tests: the served port range is machine-global.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The declaration every non-engine fixture serves, and the file
/// referencing it.
const LIBRARY: &str = "pub fn beacon() {}\n";

/// A workspace fixture: one Rust source and a `rift.toml` whose
/// `[server]` idle timeout reaps any orphaned server within a minute.
fn workspace() -> TestResult<tempfile::TempDir> {
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
const RUST_PROJECT_MANIFEST: &str = "[package]\nname = \"rift_live_fixture\"\nversion = \"0.0.0\"\n\
                                     edition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n\
                                     [workspace]\n";
const RUST_PROJECT_ROOT: &str = "pub mod caller;\npub mod hub;\n";
const RUST_PROJECT_HUB: &str = "pub fn beacon(value: i32) -> i32 {\n    value\n}\n";
const RUST_PROJECT_CALLER: &str =
    "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n";
const RUST_PROJECT_BEACON_SYMBOL: &str = "rift://symbol/rust/hub.rs/beacon";

fn rust_project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", RUST_PROJECT_MANIFEST),
        ("lib.rs", RUST_PROJECT_ROOT),
        ("hub.rs", RUST_PROJECT_HUB),
        ("caller.rs", RUST_PROJECT_CALLER),
    ]
}

/// The cargo project fixture with a real `[engines.rust]` table appended,
/// serving `rust` through rust-analyzer.
fn rust_engine_workspace() -> TestResult<tempfile::TempDir> {
    laid_out_workspace(&rust_project(), &rust_engine::rust_engine_configuration())
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
/// semantic tier, the orphan-safety idle timeout, and `engine`.
fn laid_out_workspace(files: &[(&str, &str)], engine: &str) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        fs::write(directory.path().join(name), source)?;
    }
    fs::write(
        directory.path().join("rift.toml"),
        format!("{SEMANTIC_DISABLED}[server]\nidle_timeout = \"60s\"\n{engine}"),
    )?;
    Ok(directory)
}

/// Stops the fixture's server when a test unwinds, best effort.
struct StopOnDrop {
    root: PathBuf,
}

impl StopOnDrop {
    fn new(root: &Path) -> Self {
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
async fn run_rift(root: &Path, arguments: &[&str]) -> TestResult<std::process::Output> {
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

fn require_success(output: &std::process::Output, what: &str) -> TestResult {
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

fn document_path(root: &Path) -> PathBuf {
    root.join(".rift").join(SERVER_LOCK_FILE_NAME)
}

fn serving_document(root: &Path) -> Option<ServerLock> {
    match probe(root) {
        ServerPresence::Serving(lock) => Some(lock),
        ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// Polls `condition` every [`PRESENCE_POLL_INTERVAL`] up to `attempts`
/// times.
async fn wait_for<T>(
    attempts: u32,
    what: &str,
    mut condition: impl FnMut() -> Option<T>,
) -> TestResult<T> {
    for _ in 0..attempts {
        if let Some(value) = condition() {
            return Ok(value);
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err(format!("timed out waiting for {what}").into())
}

/// Bounds one proxied operation by [`PROXIED_CALL_MAX`].
async fn within<Value>(what: &str, operation: impl Future<Output = Value>) -> TestResult<Value> {
    tokio::time::timeout(PROXIED_CALL_MAX, operation)
        .await
        .map_err(|_elapsed| format!("timed out waiting for {what}").into())
}

/// The `rift mcp` child command for one fixture workspace.
fn proxy_command(root: &Path) -> TokioChildProcessBuilder {
    TokioChildProcess::builder(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rift")).configure(|command| {
            command
                .arg("mcp")
                .current_dir(root)
                .env("RUST_LOG", "rift=info,rift_mcp=info,rift_server=info");
        }),
    )
}

/// One connected `rift mcp` child with its stderr discarded.
async fn proxy_client(root: &Path) -> TestResult<RunningService<RoleClient, ()>> {
    let (transport, _stderr) = proxy_command(root).stderr(Stdio::null()).spawn()?;
    Ok(().serve(transport).await?)
}

fn arguments(value: &serde_json::Value) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// Most attempts one proxied call spends on a retryable refusal.
const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// One proxied tool call returning its structured result, retrying the
/// refusal the server advertises as `retry: same_request`: an applied
/// change moves the index, and a request whose snapshot predates the move
/// is refused rather than served stale.
async fn proxied_call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        let params = CallToolRequestParams::new(name).with_arguments(arguments(call_arguments)?);
        match within(name, client.call_tool(params)).await? {
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

/// One proxied `get_symbol` round trip for the fixture's `beacon` symbol.
async fn beacon_lookup(client: &RunningService<RoleClient, ()>) -> TestResult<serde_json::Value> {
    proxied_call(client, "get_symbol", &json!({"name": "beacon"})).await
}

fn assert_beacon(lookup: &serde_json::Value) {
    assert_eq!(lookup["hits"][0]["symbol"]["name"], json!("beacon"));
}

/// A loopback port inside the serving range that currently refuses
/// connections: provably bindable a moment ago, then released.
fn refusing_port_in_range() -> TestResult<u16> {
    for port in (SERVER_PORT_MIN..=SERVER_PORT_MAX).rev() {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(port);
        }
    }
    Err("no free port in the serving range".into())
}

/// A loopback port inside the serving range, bound and held open so a
/// server pinned to it exactly fails to bind.
fn held_port_in_range() -> TestResult<TcpListener> {
    for port in SERVER_PORT_MIN..=SERVER_PORT_MAX {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            return Ok(listener);
        }
    }
    Err("no free port in the serving range".into())
}

#[tokio::test]
async fn cold_start_elects_a_server_that_survives_the_proxy() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let listing = within("the proxied tool listing", client.list_tools(None)).await??;
    assert_eq!(
        listing
            .tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        SERVED_TOOL_NAMES
    );
    assert_beacon(&beacon_lookup(&client).await?);

    let serving = serving_document(root).ok_or("the proxy must have elected a server")?;
    client.cancel().await?;
    let survivor = serving_document(root).ok_or("the server must survive the proxy's exit")?;
    assert_eq!(
        survivor.pid, serving.pid,
        "the same server must keep serving"
    );

    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the proxy session")?;
    Ok(())
}

#[tokio::test]
async fn warm_start_adopts_the_running_server() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let started = run_rift(root, &["server", "start"]).await?;
    require_success(&started, "server start before the proxy")?;
    let before = serving_document(root).ok_or("the started server must serve")?;

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let after = serving_document(root).ok_or("the server must stay serving")?;
    assert_eq!(
        after.pid, before.pid,
        "the proxy must adopt the running server, not replace it"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_proxies_share_one_elected_server() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let (first, second) = tokio::join!(proxy_client(root), proxy_client(root));
    let (first, second) = (first?, second?);
    let (first_listing, second_listing) = tokio::join!(
        within("the first proxy's tool listing", first.list_tools(None)),
        within("the second proxy's tool listing", second.list_tools(None)),
    );
    for listing in [first_listing??, second_listing??] {
        assert_eq!(
            listing
                .tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            SERVED_TOOL_NAMES
        );
    }

    let serving = serving_document(root).ok_or("one elected server must serve both")?;
    first.cancel().await?;
    second.cancel().await?;
    let survivor = serving_document(root).ok_or("the shared server must outlive both")?;
    assert_eq!(
        survivor.pid, serving.pid,
        "exactly one server pid throughout"
    );
    Ok(())
}

#[tokio::test]
async fn proxy_session_reconnects_after_a_server_restart() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let first = serving_document(root).ok_or("the first server must serve")?;

    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "server stop mid-session")?;
    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server to leave",
        || serving_document(root).is_none().then_some(()),
    )
    .await?;

    assert_beacon(&beacon_lookup(&client).await?);
    let second = serving_document(root).ok_or("the reconnect must elect a server")?;
    assert_ne!(
        second.pid, first.pid,
        "the same session must be served by a freshly elected process"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn stale_lock_document_yields_a_fresh_election() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    fs::create_dir_all(root.join(".rift"))?;
    let stale = ServerLock {
        port: 12_345,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        version: "0.0.1".to_owned(),
    };
    fs::write(document_path(root), serde_json::to_vec(&stale)?)?;
    assert!(
        serving_document(root).is_none(),
        "a document without an election holder is not serving"
    );

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let serving = serving_document(root).ok_or("a fresh server must replace the stale lock")?;
    assert_ne!(serving.pid, 1, "the stale pid must be replaced");
    client.cancel().await?;
    Ok(())
}

/// The refusal an agent sees when the workspace cannot produce a server.
///
/// The test holds the election itself and records a server that refuses
/// connections, so adoption always fails and every spawn this proxy makes
/// finds the election already held, losing it the same way a concurrent
/// spawn race's loser does. A lost election keeps the poll waiting for a
/// winner that, here, never comes, so the warmup and the request each wait
/// out one full start window before the generic timeout refusal - this
/// test deliberately spends about two windows of wall clock.
#[tokio::test]
async fn held_election_without_a_server_refuses_with_operator_guidance() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let guard = claim(root)?;
    guard.publish(&ServerLock {
        port: refusing_port_in_range()?,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        version: "0.0.1".to_owned(),
    })?;

    let (transport, stderr) = proxy_command(root).stderr(Stdio::piped()).spawn()?;
    let mut stderr = stderr.ok_or("proxy stderr missing")?;
    let stderr_task = tokio::spawn(async move {
        let mut output = String::new();
        stderr.read_to_string(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });
    let client = ().serve(transport).await?;

    let refusal = within(
        "the unserved workspace's refusal",
        client.call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        ),
    )
    .await?
    .expect_err("a workspace that cannot produce a server must refuse");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert!(
        data.message.contains("15s"),
        "the refusal must name the window the caller waited out: {}",
        data.message
    );
    assert!(
        data.message.contains("operator action"),
        "the refusal must name an action the operator can take: {}",
        data.message
    );
    assert!(
        !data.message.contains('`'),
        "the caller has no shell to run a command in: {}",
        data.message
    );

    client.cancel().await?;
    let stderr = stderr_task.await??;
    assert!(
        stderr.contains("different rift version"),
        "the stale-binary skew must be diagnosed: {stderr}"
    );
    assert!(
        stderr.contains("upstream warmup did not connect"),
        "{stderr}"
    );
    drop(guard);
    Ok(())
}

/// The refusal an agent sees when the workspace's own spawned server
/// exists but cannot bind its configured port: the detached spawn
/// succeeds, the child prints its own startup failure to stderr and exits
/// before publishing a lock document, and the proxy answers with that
/// captured stderr instead of waiting out the poll's own window.
///
/// The pinned port stays held for the whole test, so both the warmup
/// task's own spawn attempt and the request's each fail the same way; the
/// elapsed-time bound proves the refusal came from the captured exit, not
/// from exhausting `START_WAIT_MAX`.
#[tokio::test]
async fn a_spawned_server_that_cannot_bind_its_port_refuses_with_its_captured_stderr() -> TestResult
{
    let _serial = SERIAL.lock().await;
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("lib.rs"), LIBRARY)?;
    let held = held_port_in_range()?;
    let port = held.local_addr()?.port();
    fs::write(
        root.join("rift.toml"),
        format!("{SEMANTIC_DISABLED}[server]\nport = {port}\nidle_timeout = \"60s\"\n"),
    )?;
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let started = Instant::now();
    let refusal = within(
        "the refusal from a server that could not bind its port",
        client.call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        ),
    )
    .await?
    .expect_err("a spawned server that cannot bind its port must refuse the call");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a bind failure must refuse near-immediately, not after the poll's own window: {:?}",
        started.elapsed()
    );
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert!(
        data.message
            .contains("every loopback port in the serving range is bound"),
        "the refusal must carry the spawned server's own captured stderr: {}",
        data.message
    );
    assert!(
        !data.message.contains("server_already_serving"),
        "a genuine bind failure must not be mistaken for a lost election: {}",
        data.message
    );

    client.cancel().await?;
    drop(held);
    Ok(())
}

#[tokio::test]
async fn proxy_stderr_carries_lifecycle_lines_and_never_the_token() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let (transport, stderr) = proxy_command(root).stderr(Stdio::piped()).spawn()?;
    let mut stderr = stderr.ok_or("proxy stderr missing")?;
    let stderr_task = tokio::spawn(async move {
        let mut output = String::new();
        stderr.read_to_string(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });

    let client = ().serve(transport).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let token = serving_document(root)
        .ok_or("the elected server must serve")?
        .token;
    client.cancel().await?;

    let stderr = stderr_task.await??;
    assert!(stderr.contains("MCP proxy starting"), "{stderr}");
    assert!(stderr.contains("MCP proxy ready"), "{stderr}");
    assert!(stderr.contains("MCP proxy stopped"), "{stderr}");
    assert!(
        !stderr.contains(&token),
        "the bearer token must never reach stderr"
    );
    assert!(
        !stderr.contains(&root.display().to_string()),
        "tracing exposed the workspace root: {stderr}"
    );
    Ok(())
}

// The engine tier answers through the proxy, against the root the CLI
// serves.
//
// `rift mcp` and `rift server start` serve the process working directory,
// which they name `.`. Reads and writes below the root resolve against
// that directory, so they cannot tell one spelling from another; the
// engine is addressed in `file://` URIs, which carry no working
// directory, so it is the one caller that can. Every test below drives
// the real binary against a real elected server, and - gated behind
// `RIFT_ENGINE_LIVE` - a real language engine, the way an agent reaches
// Rift.

/// Most attempts the warm-up loop below spends waiting for rust-analyzer
/// to finish loading the cargo project, and the pause between them: at
/// most a minute of waiting, on top of the per-call [`PROXIED_CALL_MAX`]
/// bound, then the test fails instead of hanging.
const WARMUP_ATTEMPTS_MAX: usize = 240;
const WARMUP_PAUSE: Duration = Duration::from_millis(250);

/// Drives the rename tool, through the real proxy, until rust-analyzer has
/// loaded the cargo project.
///
/// The probe renames the declaration to the name it already has. Once
/// rust-analyzer resolves it, the proposal edits every occurrence to the
/// bytes already there, so the compiled plan holds no rewrite and the tool
/// refuses with `proposed no edits` - the readiness signal, with the tree
/// untouched either way. `rift-mcp`'s own `live_rust_analyzer.rs` states
/// the full reasoning for this probe; the proxy adds only its own latency
/// on top.
async fn warmed_rust_engine(client: &RunningService<RoleClient, ()>) -> TestResult {
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let structured = proxied_call(
            client,
            "rename_symbol",
            &json!({ "symbol": RUST_PROJECT_BEACON_SYMBOL, "new_name": "beacon" }),
        )
        .await?;
        let refused = structured["diagnostics"][0]["message"]
            .as_str()
            .unwrap_or_default();
        if refused.contains("proposed no edits") {
            return Ok(());
        }
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    Err("rust-analyzer never resolved the declaration through the proxy".into())
}

/// The engine tier answers through the whole real chain: the `rift`
/// binary as `rift mcp`, its elected `rift server`, and rust-analyzer, all
/// real processes over a real cargo project on disk.
#[tokio::test]
async fn proxied_rename_symbol_rewrites_every_referencing_file() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let structured = proxied_call(
        &client,
        "rename_symbol",
        &json!({ "symbol": RUST_PROJECT_BEACON_SYMBOL, "new_name": "flare" }),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.rs", "hub.rs"]),
        "the declaration and its cross-file reference both carry the rename: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(root.join("hub.rs"))?,
        "pub fn flare(value: i32) -> i32 {\n    value\n}\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("caller.rs"))?,
        "use crate::hub::flare;\n\npub fn total() -> i32 {\n    flare(2)\n}\n",
        "rust-analyzer rewrote the import and the call it resolved from its own index"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the rename session")?;
    Ok(())
}

/// The engine's reference rewrite lands through the proxy too, and the
/// move carries no warning.
#[tokio::test]
async fn proxied_move_file_rewrites_the_referencing_file() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    warmed_rust_engine(&client).await?;
    let structured = proxied_call(
        &client,
        "move_file",
        &json!({ "from": "hub.rs", "to": "spoke.rs" }),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warned = structured["summary"]["diagnostics"]
        .as_array()
        .is_some_and(|findings| {
            findings
                .iter()
                .any(|finding| finding["code"] == json!("rift.move.references_not_updated"))
        });
    assert!(
        !warned,
        "an engine-covered move carries no warning: {structured:#}"
    );
    assert!(!root.join("hub.rs").exists());
    assert_eq!(
        fs::read_to_string(root.join("lib.rs"))?,
        "pub mod caller;\npub mod spoke;\n",
        "the module declaration follows the new file stem"
    );
    assert_eq!(
        fs::read_to_string(root.join("caller.rs"))?,
        "use crate::spoke::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n",
        "the sibling's import path follows the renamed module"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the move session")?;
    Ok(())
}

/// A patch handing the function one argument too many.
const RUST_PROJECT_ARGUMENT_PATCH: &str =
    "--- a/caller.rs\n+++ b/caller.rs\n@@ -4 +4 @@\n-    beacon(2)\n+    beacon(2, 3)\n";

/// The inverse of [`RUST_PROJECT_ARGUMENT_PATCH`], restoring the single
/// argument.
const RUST_PROJECT_ARGUMENT_REVERT_PATCH: &str =
    "--- a/caller.rs\n+++ b/caller.rs\n@@ -4 +4 @@\n-    beacon(2, 3)\n+    beacon(2)\n";

/// A change applied through the whole real chain carries the engine's own
/// finding for the file it changed, and never the warning an unreachable
/// engine degrades to.
///
/// Each attempt lands the arity error and reverts it, so the file is
/// exactly as the attempt found it and the next attempt runs against the
/// same starting bytes; the loop ends on the first attempt whose summary
/// carries rust-analyzer's own finding. `rift-mcp`'s own
/// `live_rust_analyzer.rs` proves this same shape without the proxy in the
/// path; this proves it with the proxy, the election, and the daemon all
/// real too.
#[tokio::test]
async fn proxied_change_carries_the_engine_findings() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    warmed_rust_engine(&client).await?;

    let mut structured = None;
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let landed = proxied_call(
            &client,
            "patch",
            &json!({ "patch": RUST_PROJECT_ARGUMENT_PATCH }),
        )
        .await?;
        let carries_arity_error =
            landed["summary"]["diagnostics"]
                .as_array()
                .is_some_and(|findings| {
                    findings
                        .iter()
                        .any(|finding| finding["code"] == json!("E0107"))
                });
        if carries_arity_error {
            structured = Some(landed);
            break;
        }
        proxied_call(
            &client,
            "patch",
            &json!({ "patch": RUST_PROJECT_ARGUMENT_REVERT_PATCH }),
        )
        .await?;
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    let structured =
        structured.ok_or("rust-analyzer reported no arity error within the warm-up bound")?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = structured["summary"]["diagnostics"]
        .as_array()
        .ok_or("the summary must carry findings")?;
    let engine_findings = findings
        .iter()
        .filter(|finding| finding["language"]["name"] == json!("rust"))
        .count();
    assert_eq!(
        engine_findings, 1,
        "the arity error rides the applied change: {structured:#}"
    );
    let finding = findings
        .iter()
        .find(|finding| finding["code"] == json!("E0107"))
        .ok_or("the arity error must be among the findings")?;
    assert_eq!(finding["severity"], json!("error"));
    assert_eq!(finding["message"], json!("expected 1 argument, found 2"));
    let degraded = findings
        .iter()
        .any(|finding| finding["code"] == json!("rift.engine.failed"));
    assert!(
        !degraded,
        "an addressed engine never degrades to a warning: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(root.join("caller.rs"))?,
        "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2, 3)\n}\n",
        "the change stays applied with its finding attached"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the diagnostics session")?;
    Ok(())
}
