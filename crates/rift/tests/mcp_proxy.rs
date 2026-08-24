//! Real-binary contract of the `rift mcp` stdio proxy.
//!
//! Every test drives the compiled `rift` binary as an MCP stdio child
//! against a throwaway workspace fixture, proving the proxy elects,
//! adopts, shares, and re-elects the workspace's server. The tests
//! serialize on one async mutex: the servers share the loopback election
//! port range. Each fixture's `rift.toml` accepts a 60-second idle timeout
//! as an orphan-safety net, and a drop guard stops any server a failed
//! test leaves behind.

use std::error::Error;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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

/// The eight tools the workspace server advertises, in served order.
const SERVED_TOOL_NAMES: [&str; 8] = [
    "get_symbol",
    "insert_symbol",
    "nodes",
    "patch",
    "rename_symbol",
    "replace_node",
    "replace_symbol",
    "search",
];

/// Serializes the tests: the served port range is machine-global.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A workspace fixture: one Rust source and a `rift.toml` whose
/// `[server]` idle timeout reaps any orphaned server within a minute.
fn workspace() -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        "[server]\nidle_timeout = \"60s\"\n",
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

/// One proxied `get_symbol` round trip for the fixture's `beacon` symbol.
async fn beacon_lookup(client: &RunningService<RoleClient, ()>) -> TestResult<serde_json::Value> {
    let called = within(
        "a proxied get_symbol round trip",
        client.call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        ),
    )
    .await??;
    called
        .structured_content
        .ok_or_else(|| "get_symbol must return structured content".into())
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
/// connections, so the proxy's adoption, spawn, and poll all fail. The
/// warmup and the request each wait out one full start window, so this
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
        data.message.contains("no rift server answered"),
        "{}",
        data.message
    );
    assert!(
        data.message.contains("rift server start --foreground"),
        "{}",
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
