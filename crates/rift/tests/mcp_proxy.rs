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
//!
//! Every entry point named above lives in `harness.rs`, shared with
//! `end_to_end.rs`; this file's own tests prove election, adoption,
//! sharing, and re-election, and add the few helpers only they use.

mod engine_fixture;
mod harness;
mod live_engine_gate;
mod rust_engine;

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use harness::{
    LIBRARY, PROXIED_ENGINE_CALL_MAX, RUST_PROJECT_CALLER, RUST_PROJECT_HUB, RUST_PROJECT_ROOT,
    SERIAL, StopOnDrop, TestResult, arguments, laid_out_workspace, proxied_call,
    proxied_engine_call, proxy_client, proxy_command, require_success, run_rift,
    rust_engine_workspace, within, workspace,
};
use rift_mcp::{PRESENCE_POLL_INTERVAL, START_WAIT_MAX, ServerPresence, claim, probe};
use rift_protocol::lock::{
    ProductIdentity, SERVER_LOCK_FILE_NAME, SERVER_PORT_MAX, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH,
    ServerLock,
};
use rift_protocol::retry::RetryPolicy;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

/// The tools the workspace server advertises, in served order.
const SERVED_TOOL_NAMES: [&str; 12] = [
    "get_symbol",
    "insert_node",
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

const RUST_PROJECT_BEACON_SYMBOL: &str = "rift://symbol/rust/hub.rs/beacon";

#[test]
fn proxied_engine_bound_covers_two_retry_sequences_and_election() {
    let retry = RetryPolicy {
        attempts: rust_engine::RUST_ENGINE_RETRY_ATTEMPTS,
        ..RetryPolicy::default()
    };
    let retry_wait: Duration = (1..retry.attempts)
        .filter_map(|attempt| retry.delay_after(attempt))
        .sum();
    let required = retry_wait * 2 + START_WAIT_MAX;
    assert!(PROXIED_ENGINE_CALL_MAX >= Duration::from_secs(120));
    assert!(PROXIED_ENGINE_CALL_MAX > required);
}

fn rift_binary_identity() -> TestResult<ProductIdentity> {
    let executable = fs::read(env!("CARGO_BIN_EXE_rift"))?;
    Ok(ProductIdentity {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        executable_digest: format!("{:x}", Sha256::digest(executable)),
        schema_digest: format!(
            "{:x}",
            Sha256::digest(rift_mcp::schema::schema_document().as_bytes())
        ),
    })
}

/// Poll attempts while waiting on a server to disappear: 10 seconds at
/// [`PRESENCE_POLL_INTERVAL`].
const GONE_POLL_ATTEMPT_COUNT: u32 = 100;

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

/// One proxied `get_symbol` round trip for the fixture's `beacon` symbol.
async fn beacon_lookup(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult<serde_json::Value> {
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
    let initialized = serde_json::to_value(
        client
            .peer_info()
            .ok_or("the proxy must return initialize data")?,
    )?;
    let advertised_identity = initialized["_meta"]["sh.volar/rift"].clone();

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
    assert_eq!(
        advertised_identity,
        serde_json::to_value(&serving.identity)?,
        "initialize and the lock document must identify the same server"
    );

    let inserted = proxied_call(
        &client,
        "insert_symbol",
        &json!({
            "anchor": "rift://symbol/rust/lib.rs/beacon",
            "position": "after",
            "body": "pub fn lantern() {}",
        }),
    )
    .await?;
    assert_eq!(inserted["status"], json!("applied"), "{inserted:#}");
    let lantern = proxied_call(&client, "get_symbol", &json!({"name": "lantern"})).await?;
    assert_eq!(
        lantern["hits"][0]["symbol"]["name"],
        json!("lantern"),
        "{lantern:#}"
    );

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
        identity: rift_binary_identity()?,
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
        identity: rift_binary_identity()?,
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
        stderr.contains("recorded server did not answer"),
        "the stale server must be diagnosed: {stderr}"
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
/// The pinned port stays held for the whole test. Captured stderr and the
/// absence of the poll-exhaustion refusal prove the spawned process exit
/// supplied the result.
#[tokio::test]
async fn a_spawned_server_that_cannot_bind_its_port_refuses_with_its_captured_stderr() -> TestResult
{
    let _serial = SERIAL.lock().await;
    let held = held_port_in_range()?;
    let port = held.local_addr()?.port();
    let directory = laid_out_workspace(&[("lib.rs", LIBRARY)], &format!("port = {port}\n"))?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let refusal = within(
        "the refusal from a server that could not bind its port",
        client.call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        ),
    )
    .await?
    .expect_err("a spawned server that cannot bind its port must refuse the call");
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
    let structured = proxied_engine_call(
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

/// The engine's will-rename proposal crosses the proxy unchanged. Reference
/// edits land without a warning; an empty proposal moves only the file and
/// carries one warning.
#[tokio::test]
async fn proxied_move_file_matches_the_engine_proposal() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let structured = proxied_engine_call(
        &client,
        "move_file",
        &json!({ "from": "hub.rs", "to": "spoke.rs" }),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!("rift.move.references_not_updated"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(!root.join("hub.rs").exists());
    assert_eq!(fs::read_to_string(root.join("spoke.rs"))?, RUST_PROJECT_HUB);
    match warnings.as_slice() {
        [] => {
            assert_eq!(
                structured["summary"]["paths"],
                json!(["caller.rs", "hub.rs", "lib.rs", "spoke.rs"]),
                "the proposal rewrites, old path, and new path ride the summary: {structured:#}"
            );
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
        }
        [warning] => {
            assert_eq!(warning["severity"], json!("warning"), "{structured:#}");
            assert!(
                warning["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("engine rust")
                        && message.contains("references were not updated")),
                "the warning names the engine and skipped updates: {structured:#}"
            );
            assert_eq!(
                structured["summary"]["paths"],
                json!(["hub.rs", "spoke.rs"]),
                "an empty proposal moves only the requested file: {structured:#}"
            );
            assert_eq!(fs::read_to_string(root.join("lib.rs"))?, RUST_PROJECT_ROOT);
            assert_eq!(
                fs::read_to_string(root.join("caller.rs"))?,
                RUST_PROJECT_CALLER
            );
        }
        _ => panic!("one move carries at most one reference warning: {structured:#}"),
    }

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the move session")?;
    Ok(())
}

/// A patch replacing one file with a malformed declaration.
const RUST_PROJECT_SYNTAX_PATCH: &str = "--- a/caller.rs\n+++ b/caller.rs\n@@ -1,5 +1 @@\n-use crate::hub::beacon;\n-\n-pub fn total() -> i32 {\n-    beacon(2)\n-}\n+fn broken( {\n";

/// A change applied through the whole real chain carries diagnostics for
/// the file it changed and never the warning an unreachable engine degrades
/// to. Engine versions may add findings beside the syntax provider's.
///
/// `rift-mcp`'s own `live_rust_analyzer.rs` proves this same shape without
/// the proxy in the path; this proves it with the proxy, the election, and
/// the daemon all real too.
#[tokio::test]
async fn proxied_change_carries_diagnostics() -> TestResult {
    let _serial = SERIAL.lock().await;
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);

    let client = proxy_client(root).await?;
    let structured = proxied_engine_call(
        &client,
        "patch",
        &json!({ "patch": RUST_PROJECT_SYNTAX_PATCH }),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = structured["summary"]["diagnostics"]
        .as_array()
        .ok_or("the summary must carry findings")?;
    let finding = findings
        .iter()
        .find(|finding| finding["code"] == json!("rift.syntax.error"))
        .ok_or("provider syntax finding must be among findings")?;
    assert_eq!(finding["severity"], json!("error"));
    assert_eq!(finding["reliability"], json!("recovered"));
    let degraded = findings
        .iter()
        .any(|finding| finding["code"] == json!("rift.engine.failed"));
    assert!(
        !degraded,
        "an addressed engine never degrades to a warning: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(root.join("caller.rs"))?,
        "fn broken( {\n",
        "the change stays applied with its finding attached"
    );

    client.cancel().await?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop after the diagnostics session")?;
    Ok(())
}
