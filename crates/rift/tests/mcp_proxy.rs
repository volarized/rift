//! Real-binary contract of the `rift mcp` stdio proxy, and the end-to-end
//! lane: a real MCP client, the real `rift` binary run as an agent runs
//! it, a real elected server, and - gated behind `RIFT_ENGINE_LIVE` - a
//! real language engine, over a real temp-directory workspace.
//!
//! Every test drives the compiled `rift` binary as an MCP stdio child
//! against a throwaway workspace fixture. The proxy tests prove election,
//! adoption, sharing, and re-election. The engine test reads incoming references
//! through the proxy and verifies the configured engine starts. The tests
//! serialize on one async mutex: the servers share the
//! loopback election port range. Each fixture's `rift.toml` accepts a
//! 60-second idle timeout as an orphan-safety net, and a drop guard stops
//! any server a failed test leaves behind.
//!
//! **Adding a case to the end-to-end lane.** Lay out a workspace with
//! [`laid_out_workspace`] (files, plus LSP configuration when the case needs
//! one - build it from a fixture in `engine_fixture.rs`/`rust_engine.rs`,
//! following the pattern those two modules already set for rust), connect
//! to it with [`proxy_client`], drive it with [`proxied_call`], and gate
//! the test behind `live_engine_gate::engine_live` when it needs a real
//! engine. `proxy_client` is the one entry point that spawns the real
//! `rift mcp` binary - `relayed_proxy_client` when a case asserts on the
//! proxy's stderr - and every case shares it: both relay that stderr onto
//! the test's own, so a failed or timed-out case prints it, and no case may
//! spawn a process of its own to stand in for the server or the engine.
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
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use harness::{
    FIXTURE_READINESS_TIMEOUT, FIXTURE_WORKER_QUEUE_TIMEOUT, FailureWindow, LIBRARY,
    PROXIED_CALL_MAX, PROXIED_ENGINE_CALL_MAX, StopOnDrop, TestResult, arguments,
    await_workspace_ready, await_workspace_text, laid_out_workspace, proxied_call,
    proxied_engine_call, proxied_result, proxy_client, proxy_client_with, relayed_proxy_client,
    require_success, resource_json, resource_text, run_rift, rust_engine_workspace, tool_failure,
    within, workspace,
};
use rift_mcp::{
    BuildCheckout, ElectionGuard, PRESENCE_POLL_INTERVAL, START_WAIT_MAX, ServerPresence, claim,
    probe,
};
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::error as wire;
use rift_protocol::error::{ErrorCode, RetryDirective};
use rift_protocol::lock::{
    ProductIdentity, SERVER_LOCK_FILE_NAME, SERVER_PORT_MAX, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH,
    ServerLock,
};
use rift_protocol::retry::RetryPolicy;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use serde_json::json;
use tokio::sync::Notify;

/// The tools the workspace server advertises, in served order.
const SERVED_TOOL_NAMES: [&str; 3] = ["get_symbol", "nodes", "search"];

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

/// A fixture's `rift.toml`, accepted the way the served configuration is, gives the
/// proxy a forward budget that ends inside [`PROXIED_CALL_MAX`]: a server that stops
/// answering is refused by the proxy, naming that budget, before the case gives up on
/// the call and long before nextest ends the case.
#[test]
fn proxied_forward_budget_ends_inside_the_call_bound() -> TestResult {
    let directory = workspace()?;
    let document = fs::read_to_string(directory.path().join("rift.toml"))?;
    let environment = rift_core::acceptance::ConfigurationEnvironment::default();
    let accepted = rift_core::acceptance::accept_configuration::<WorkspaceConfiguration>(
        Some(&document),
        &environment,
    )?;
    let server = &accepted.configuration().server;
    assert_eq!(server.readiness_timeout, FIXTURE_READINESS_TIMEOUT);
    assert_eq!(server.worker_queue_timeout, FIXTURE_WORKER_QUEUE_TIMEOUT);
    let budget = rift_mcp::forward_budget(server);
    assert!(
        budget < PROXIED_CALL_MAX,
        "the proxy's forward budget must end inside the harness bound on one call: \
         forward_budget={budget:?}, PROXIED_CALL_MAX={PROXIED_CALL_MAX:?}"
    );
    Ok(())
}

/// The identity the `rift` binary under test publishes. The package's build script hands
/// this suite the same checkout it hands the binary, and a dirty build reads the binary's
/// own metadata.
fn rift_binary_identity() -> TestResult<ProductIdentity> {
    let checkout = BuildCheckout::recorded(env!("RIFT_BUILD_COMMIT"), env!("RIFT_BUILD_DIRTY"));
    Ok(rift_mcp::product_identity_of(
        checkout,
        &harness::rift_binary(),
    )?)
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
        ServerPresence::Starting | ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// Polls `condition` every [`PRESENCE_POLL_INTERVAL`] up to `attempts`
/// times, and for no longer than those attempts span at that interval.
///
/// A condition that probes the workspace is not instant: a probe of a port
/// nothing accepts on spends its whole connect timeout, which on Windows is
/// every refused port, so counting alone would stretch the wait several times
/// over.
async fn wait_for<T>(
    attempts: u32,
    what: &str,
    mut condition: impl FnMut() -> Option<T>,
) -> TestResult<T> {
    let deadline = tokio::time::Instant::now() + PRESENCE_POLL_INTERVAL.saturating_mul(attempts);
    for _ in 0..attempts {
        if let Some(value) = condition() {
            return Ok(value);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err(format!("timed out waiting for {what}").into())
}

/// One proxied `get_symbol` round trip for the fixture's `beacon` symbol.
async fn beacon_lookup(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult<serde_json::Value> {
    await_workspace_ready(client).await?;
    proxied_call(client, "get_symbol", &json!({"name": "beacon"})).await
}

fn assert_beacon(lookup: &serde_json::Value) {
    assert_eq!(lookup["hits"][0]["symbol"]["name"], json!("beacon"));
}

/// Retains only the foreground child this fixture started.
struct RepositoryForeground(Child);

impl Drop for RepositoryForeground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn fixture_git(root: &Path, arguments: &[&str]) -> TestResult {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Rift fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@rift.test")
        .env("GIT_COMMITTER_NAME", "Rift fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@rift.test")
        .output()?;
    require_success(&output, "repository fixture Git command")
}

async fn repository_foreground(
    root: &Path,
    state_directory: &Path,
) -> TestResult<(RepositoryForeground, ServerLock)> {
    let mut child = RepositoryForeground(
        Command::new(harness::rift_binary())
            .args(["server", "start", "--foreground", "--repository"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let serving = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "repository foreground startup",
        || {
            if child.0.try_wait().ok().flatten().is_some() {
                return None;
            }
            match rift_mcp::probe_state_directory(state_directory) {
                ServerPresence::Serving(lock) if lock.pid == child.0.id() => Some(lock),
                _ => None,
            }
        },
    )
    .await?;
    Ok((child, serving))
}

async fn stop_repository_foreground(root: &Path, child: &mut RepositoryForeground) -> TestResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let stopped = tokio::time::timeout_at(
        deadline,
        run_rift(root, &["server", "stop", "--repository"]),
    )
    .await??;
    require_success(&stopped, "repository foreground stop")?;
    loop {
        if let Some(status) = child.0.try_wait()? {
            assert!(
                status.success(),
                "repository foreground exits cleanly: {status:?}"
            );
            return Ok(());
        }
        tokio::time::timeout_at(deadline, tokio::time::sleep(PRESENCE_POLL_INTERVAL)).await?;
    }
}

async fn stop_repository_clients(
    root: &Path,
    child: &mut RepositoryForeground,
    clients: Vec<rmcp::service::RunningService<rmcp::service::RoleClient, ()>>,
) -> TestResult {
    for client in clients {
        client.cancel().await?;
    }
    stop_repository_foreground(root, child).await
}

async fn repository_workspace_reads(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
    names: &[&str],
) -> TestResult<String> {
    let map = await_workspace_ready(client).await?;
    let revision = map["revision"]
        .as_str()
        .ok_or("map carries its revision")?
        .to_owned();
    let lookup = proxied_call(client, "get_symbol", &json!({"name": name}))
        .await
        .map_err(|error| format!("positive get_symbol: {error:?}"))?;
    assert_eq!(lookup["hits"][0]["symbol"]["name"], name, "{lookup}");
    let other = names
        .iter()
        .find(|other| **other != name)
        .ok_or("fixture has another symbol")?;
    let absent = proxied_call(client, "get_symbol", &json!({"name": other}))
        .await
        .map_err(|error| format!("negative get_symbol: {error:?}"))?;
    assert!(
        absent["hits"]
            .as_array()
            .ok_or("lookup carries hits")?
            .is_empty(),
        "{absent}"
    );
    let search = proxied_call(
        client,
        "search",
        &json!({"query": name, "target": "symbol"}),
    )
    .await
    .map_err(|error| format!("search: {error:?}"))?;
    assert!(
        search["results"]
            .as_array()
            .ok_or("search carries results")?
            .iter()
            .any(|hit| hit["hit"]["symbol"]["name"] == name),
        "{search}"
    );
    let nodes = proxied_call(client, "nodes", &json!({"path": "lib.rs", "position": 8}))
        .await
        .map_err(|error| format!("nodes: {error:?}"))?;
    assert!(
        nodes["source"]
            .as_array()
            .ok_or("nodes carries source")?
            .iter()
            .any(|source| source.as_str().is_some_and(|source| source.contains(name))),
        "{nodes}"
    );
    Ok(revision)
}

async fn repository_workspace_resource_digest(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult<String> {
    let mut digest = None;
    for uri in ["rift://workspace", "rift://logs"] {
        let answer = within(
            "repository resource",
            client.read_resource(rmcp::model::ReadResourceRequestParams::new(uri)),
        )
        .await??;
        let body = resource_json(&answer, uri)?;
        assert!(body.is_object(), "{uri}: {body}");
        if uri == "rift://workspace" {
            let source = body["source"]
                .as_array()
                .ok_or("workspace carries source catalog")?
                .iter()
                .find(|source| source["path"] == "lib.rs")
                .ok_or("workspace catalog carries lib.rs")?;
            digest = Some(
                source["digest"]
                    .as_str()
                    .ok_or("source carries digest")?
                    .to_owned(),
            );
        }
    }
    digest.ok_or_else(|| "workspace source digest was read".into())
}

async fn a_competing_foreground_start_preserves_repository_logs(
    root: &Path,
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult {
    let before = within(
        "repository logs before competing start",
        client.read_resource(rmcp::model::ReadResourceRequestParams::new("rift://logs")),
    )
    .await??;
    let refused = within(
        "competing foreground start",
        run_rift(root, &["server", "start", "--foreground"]),
    )
    .await??;
    assert!(
        !refused.status.success(),
        "a repository owns this workspace"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("server_already_serving"),
        "{}",
        harness::bounded_tail(&stderr)
    );
    let after = within(
        "repository logs after competing start",
        client.read_resource(rmcp::model::ReadResourceRequestParams::new("rift://logs")),
    )
    .await??;
    assert_eq!(
        resource_json(&before, "rift://logs")?,
        resource_json(&after, "rift://logs")?,
        "a refused process cannot write diagnostics into the owner's database"
    );
    Ok(())
}

async fn repository_workspace_facts(
    root: &Path,
    name: &str,
    names: &[&str],
) -> TestResult<
    Option<(
        rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
        String,
        String,
    )>,
> {
    let workspace = format!("workspace {name} at {}", root.display());
    let client = proxy_client(root)
        .await
        .map_err(|error| format!("{workspace} proxy startup: {error:?}"))?;
    let revision = match repository_workspace_reads(&client, name, names).await {
        Ok(revision) => revision,
        Err(error) if windows_indigo_first_index_failure(cfg!(windows), name, error.as_ref()) => {
            // XFAIL: https://github.com/volarized/rift/issues/530
            // Only the recorded Windows indigo initial-map refusal is expected.
            eprintln!("XFAIL https://github.com/volarized/rift/issues/530: {workspace}: {error:?}");
            client.cancel().await?;
            return Ok(None);
        }
        Err(error) => return Err(format!("{workspace} read: {error:?}").into()),
    };
    let source_digest = repository_workspace_resource_digest(&client)
        .await
        .map_err(|error| format!("{workspace} resource digest: {error:?}"))?;
    assert!(claim(root).is_err(), "repository owns this workspace store");
    assert!(
        !document_path(root).exists(),
        "workspace has no separate serving document"
    );
    Ok(Some((client, revision, source_digest)))
}

fn windows_indigo_first_index_failure(
    windows: bool,
    name: &str,
    error: &(dyn std::error::Error + 'static),
) -> bool {
    let Some(rmcp::ServiceError::McpError(error)) = error.downcast_ref::<rmcp::ServiceError>()
    else {
        return false;
    };
    let message = "workspace server did not finish building its first index within 30s; resend the same request after a short delay";
    windows
        && name == "indigo"
        && error.code == rmcp::model::ErrorCode(-32000)
        && error.message == message
        && error.data.as_ref()
            == Some(&json!({
                "code": "temporarily_unavailable",
                "message": message,
                "retry": "same_request",
                "phase": "read"
            }))
}

#[test]
fn windows_indigo_first_index_xfail_matches_only_the_recorded_refusal() {
    let message = "workspace server did not finish building its first index within 30s; resend the same request after a short delay";
    let refusal = rmcp::model::ErrorData::new(
        rmcp::model::ErrorCode(-32000),
        message,
        Some(json!({
            "code": "temporarily_unavailable",
            "message": message,
            "retry": "same_request",
            "phase": "read"
        })),
    );
    let error = rmcp::ServiceError::McpError(refusal.clone());
    assert!(windows_indigo_first_index_failure(true, "indigo", &error));
    assert!(!windows_indigo_first_index_failure(false, "indigo", &error));
    assert!(!windows_indigo_first_index_failure(true, "cedar", &error));
    assert!(!windows_indigo_first_index_failure(
        true,
        "indigo",
        &rmcp::ServiceError::TransportClosed
    ));
    assert!(!windows_indigo_first_index_failure(
        true,
        "indigo",
        &std::io::Error::other(message)
    ));
    for field in ["code", "message", "retry", "phase"] {
        for replacement in [Some(json!("different refusal")), None] {
            let mut changed = refusal.clone();
            let data = changed
                .data
                .as_mut()
                .and_then(serde_json::Value::as_object_mut)
                .expect("the recorded refusal has object data");
            if let Some(value) = replacement {
                data.insert(field.to_owned(), value);
            } else {
                data.remove(field);
            }
            assert!(!windows_indigo_first_index_failure(
                true,
                "indigo",
                &rmcp::ServiceError::McpError(changed)
            ));
        }
    }
    let mut wrong_code = refusal.clone();
    wrong_code.code = rmcp::model::ErrorCode(-32603);
    let mut wrong_message = refusal.clone();
    wrong_message.message = "a different readiness deadline expired".into();
    let mut missing_data = refusal;
    missing_data.data = None;
    for changed in [wrong_code, wrong_message, missing_data] {
        assert!(!windows_indigo_first_index_failure(
            true,
            "indigo",
            &rmcp::ServiceError::McpError(changed)
        ));
    }
}

/// All internal root routes share one elected process and retain separate workspace facts.
#[tokio::test]
async fn repository_foreground_routes_four_linked_workspaces_and_restarts_changed_settings()
-> TestResult {
    let names = ["amber", "cedar", "indigo", "quartz"];
    let main = laid_out_workspace(
        &[("lib.rs", "pub fn amber() {}\n")],
        &harness::assigned_port_key()?,
    )?;
    fixture_git(main.path(), &["init", "-q"])?;
    fixture_git(main.path(), &["add", "lib.rs", "rift.toml"])?;
    fixture_git(
        main.path(),
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "add workspace sources",
        ],
    )?;
    let linked_parent = tempfile::tempdir()?;
    let mut roots = vec![main.path().to_path_buf()];
    for name in names.iter().skip(1) {
        let root = linked_parent.path().join(format!("worktree-é-{name}"));
        fixture_git(
            main.path(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                root.to_str().ok_or("fixture path must be UTF-8")?,
                "HEAD",
            ],
        )?;
        fs::write(root.join("lib.rs"), format!("pub fn {name}() {{}}\n"))?;
        roots.push(root);
    }
    let common = rift_mcp::repository::discover_common_directory(main.path())
        .ok_or("fixture repository has a common Git directory")?;
    let state_directory =
        rift_mcp::repository::repository_election_directory(&common, &rift_binary_identity()?)?;
    // The linked workspace starts first; authority still comes from the main worktree.
    let (mut child, before) = repository_foreground(&roots[3], &state_directory).await?;
    let mut clients: Vec<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> = Vec::new();
    let mut revisions = std::collections::BTreeSet::new();
    let mut source_digests = std::collections::BTreeSet::new();
    for (root, name) in roots.iter().zip(names) {
        let Some((client, revision, source_digest)) =
            repository_workspace_facts(root, name, &names).await?
        else {
            return stop_repository_clients(&roots[3], &mut child, clients).await;
        };
        revisions.insert(revision);
        source_digests.insert(source_digest);
        clients.push(client);
    }
    a_competing_foreground_start_preserves_repository_logs(&roots[0], &clients[0]).await?;
    assert_eq!(revisions.len(), 4, "map revisions follow workspace bytes");
    assert_eq!(
        source_digests.len(),
        4,
        "source catalog follows workspace bytes"
    );
    let after = match rift_mcp::probe_state_directory(&state_directory) {
        ServerPresence::Serving(lock) => lock,
        other => return Err(format!("repository must remain serving: {other:?}").into()),
    };
    assert_eq!(
        before.pid, after.pid,
        "all proxies adopt one repository process"
    );
    let changed = fs::read_to_string(main.path().join("rift.toml"))?
        .replace("[server]\n", "[server]\nnum_workers = 2\n");
    fs::write(main.path().join("rift.toml"), &changed)?;
    let pinned = proxied_call(&clients[3], "get_symbol", &json!({"name": "quartz"}))
        .await
        .map_err(|error| format!("workspace quartz after settings change get_symbol: {error:?}"))?;
    assert_eq!(pinned["hits"][0]["symbol"]["name"], "quartz", "{pinned}");
    require_success(
        &run_rift(&roots[3], &["server", "status", "--repository"]).await?,
        "status after authority settings change",
    )?;
    stop_repository_clients(&roots[3], &mut child, clients).await?;
    for root in &roots {
        assert!(
            claim(root).is_ok(),
            "stopped process releases workspace store"
        );
        fs::write(root.join("rift.toml"), &changed)?;
    }
    let (mut reopened, serving) = repository_foreground(&roots[1], &state_directory).await?;
    assert_eq!(
        serving
            .server
            .ok_or("repository records accepted server settings")?
            .num_workers,
        2
    );
    stop_repository_foreground(&roots[1], &mut reopened).await?;
    Ok(())
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
async fn warm_start_adopts_the_running_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

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
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn concurrent_proxies_share_one_elected_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

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
    failure_window.passed();
    Ok(())
}

/// The beacon lookup's whole result, once the fixture workspace answers reads.
///
/// Readiness is read from the map text, which a `text` proxy returns without the JSON body.
async fn beacon_result(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult<CallToolResult> {
    await_workspace_text(client).await?;
    proxied_result(client, "get_symbol", &json!({"name": "beacon"})).await
}

/// Every tool one proxy lists, within the proxied-call bound.
async fn listed_tools(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
) -> TestResult<Vec<Tool>> {
    Ok(within("a tool listing", client.list_all_tools()).await??)
}

/// Content that is one non-empty text block; its wording is not asserted.
fn assert_one_text_block(result: &CallToolResult) {
    let [block] = result.content.as_slice() else {
        panic!("content must be one block, got {:?}", result.content);
    };
    let text = block.as_text().map(|text| text.text.as_str());
    assert!(
        text.is_some_and(|text| !text.is_empty()),
        "the block must be non-empty text, got {block:?}"
    );
}

fn assert_text_shape(tools: &[Tool], result: &CallToolResult) {
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        SERVED_TOOL_NAMES
    );
    assert!(
        tools.iter().all(|tool| tool.output_schema.is_none()),
        "a text proxy must list no output schema: {tools:?}"
    );
    assert_one_text_block(result);
    assert_eq!(result.structured_content, None, "{result:?}");
}

fn assert_all_shape(tools: &[Tool], result: &CallToolResult) {
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        SERVED_TOOL_NAMES
    );
    assert!(
        tools.iter().all(|tool| tool.output_schema.is_some()),
        "an all proxy must list the output schema of every tool: {tools:?}"
    );
    assert_one_text_block(result);
    assert_beacon(
        result
            .structured_content
            .as_ref()
            .expect("an all proxy must return structured content"),
    );
}

#[tokio::test]
async fn text_output_proxy_lists_no_output_schema_and_returns_no_structured_content() -> TestResult
{
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client_with(root, &["--output=text"]).await?;
    let tools = listed_tools(&client).await?;
    let result = beacon_result(&client).await?;
    assert_text_shape(&tools, &result);
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn default_proxy_lists_output_schemas_and_returns_structured_content() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client(root).await?;
    let tools = listed_tools(&client).await?;
    let result = beacon_result(&client).await?;
    assert_all_shape(&tools, &result);
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// A `rift://map` read through a `text` proxy returns the compact text alone.
#[tokio::test]
async fn text_output_proxy_returns_the_map_as_one_text_content() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client_with(root, &["--output=text"]).await?;
    await_workspace_text(&client).await?;
    let answer = within(
        "map resource",
        client.read_resource(rmcp::model::ReadResourceRequestParams::new("rift://map")),
    )
    .await??;
    let [content] = answer.contents.as_slice() else {
        return Err(format!("a text proxy answers one content: {:?}", answer.contents).into());
    };
    let rmcp::model::ResourceContents::TextResourceContents {
        mime_type, text, ..
    } = content
    else {
        return Err(format!("the content must be text: {content:?}").into());
    };
    assert_eq!(mime_type.as_deref(), Some("text/plain"), "{content:?}");
    assert!(text.starts_with("map "), "{text}");
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// The same read through the default proxy returns the compact text, then the JSON body.
#[tokio::test]
async fn default_proxy_returns_the_map_as_text_then_json() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client_with(root, &[]).await?;
    await_workspace_ready(&client).await?;
    let answer = within(
        "map resource",
        client.read_resource(rmcp::model::ReadResourceRequestParams::new("rift://map")),
    )
    .await??;
    assert_eq!(answer.contents.len(), 2, "{:?}", answer.contents);
    let text = resource_text(&answer, "rift://map")?;
    assert!(text.starts_with("map "), "{text}");
    let body = resource_json(&answer, "rift://map")?;
    assert!(body["revision"].is_string(), "{body}");
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn all_and_text_proxies_share_one_server_and_keep_their_own_shape() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let (all, text) = tokio::join!(
        proxy_client_with(root, &["--output", "all"]),
        proxy_client_with(root, &["--output", "text"]),
    );
    let (all, text) = (all?, text?);
    let (all_tools, text_tools) = (listed_tools(&all).await?, listed_tools(&text).await?);
    let (all_result, text_result) = tokio::join!(beacon_result(&all), beacon_result(&text));
    let (all_result, text_result) = (all_result?, text_result?);
    assert_all_shape(&all_tools, &all_result);
    assert_text_shape(&text_tools, &text_result);

    let serving = serving_document(root).ok_or("one elected server must serve both")?;
    all.cancel().await?;
    text.cancel().await?;
    let survivor = serving_document(root).ok_or("the shared server must outlive both")?;
    assert_eq!(
        survivor.pid, serving.pid,
        "exactly one server pid throughout"
    );
    failure_window.passed();
    Ok(())
}

/// One request per tool: a search naming `source` and `score`, a `get_symbol` with source, and
/// a `nodes` read, all against the fixture's `beacon` declaration.
fn representative_requests() -> [(&'static str, serde_json::Value); 3] {
    [
        (
            "search",
            json!({"query": "beacon", "target": "symbol", "include": ["source", "score"]}),
        ),
        (
            "get_symbol",
            json!({"name": "beacon", "include": ["source"]}),
        ),
        ("nodes", json!({"path": "lib.rs", "position": 8})),
    ]
}

/// Most polls of a search before the lexical population pass has landed.
const SEARCH_POPULATION_POLLS_MAX: usize = 60;

/// Calls `search` until its warnings no longer report a lexical pass still catching up, so
/// two later calls answer from one settled index.
async fn await_search_population(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    request: &serde_json::Value,
) -> TestResult {
    for _poll in 0..SEARCH_POPULATION_POLLS_MAX {
        let answer = proxied_call(client, "search", request).await?;
        let pending = answer["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|warning| {
                matches!(
                    warning["code"].as_str(),
                    Some("lexical_ranking_unavailable" | "stale_index")
                )
            });
        if !pending {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("the lexical pass never settled for the search".into())
}

/// The identities the text of a request must state: the symbol ids of the `search` and
/// `get_symbol` hits, and the node ids of the `nodes` answer.
///
/// The layout leaves other identities, such as file ids, to the structured content.
fn stated_identities<'value>(
    name: &str,
    structured: &'value serde_json::Value,
) -> Vec<&'value str> {
    let (key, pointer) = match name {
        "search" => ("results", "/hit/symbol/id"),
        "get_symbol" => ("hits", "/symbol/id"),
        _ => ("nodes", "/id"),
    };
    structured
        .get(key)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.pointer(pointer)?.as_str())
        .collect()
}

/// The text of a result that is one text block.
fn text_of(result: &CallToolResult) -> TestResult<&str> {
    let [block] = result.content.as_slice() else {
        return Err(format!("content must be one block, got {:?}", result.content).into());
    };
    Ok(block
        .as_text()
        .ok_or("the block must be text")?
        .text
        .as_str())
}

/// The text block is the same through the `all` and the `text` proxy of one server, the `text`
/// result carries no structured content, and the text states the identity of each symbol hit
/// and each node of the `all` result's structured content as a bare token.
///
/// The text-against-structured checks of `rift-mcp` (`crates/rift-mcp/tests/text_complete`) live
/// in that crate's test binary and are not shared here: they run over the whole surface corpus
/// there.
#[tokio::test]
async fn both_proxies_write_the_same_text_and_the_text_states_every_identity() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let (all, text) = tokio::join!(
        proxy_client_with(root, &["--output", "all"]),
        proxy_client_with(root, &["--output", "text"]),
    );
    let (all, text) = (all?, text?);
    await_workspace_ready(&all).await?;
    for (name, request) in representative_requests() {
        if name == "search" {
            await_search_population(&all, &request).await?;
        }
        let (all_result, text_result) = tokio::join!(
            proxied_result(&all, name, &request),
            proxied_result(&text, name, &request),
        );
        let (all_result, text_result) = (all_result?, text_result?);
        assert_ne!(all_result.is_error, Some(true), "{name}: {all_result:?}");
        assert_ne!(text_result.is_error, Some(true), "{name}: {text_result:?}");
        assert_eq!(
            text_result.structured_content, None,
            "{name}: {text_result:?}"
        );
        let all_text = text_of(&all_result)?;
        assert_eq!(all_text, text_of(&text_result)?, "{name}: texts differ");

        let structured = all_result
            .structured_content
            .as_ref()
            .ok_or_else(|| format!("{name}: the all proxy must return structured content"))?;
        let identities = stated_identities(name, structured);
        assert!(
            !identities.is_empty(),
            "{name}: no identity to state: {structured}"
        );
        for identity in identities {
            assert!(
                all_text.split_whitespace().any(|token| token == identity),
                "{name}: identity {identity} is no bare token of the text:\n{all_text}"
            );
        }
    }

    all.cancel().await?;
    text.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// A failing tool call completes as an error result through both proxies: `is_error`, one
/// text block, the same text in both, and no structured content, which the `text` proxy
/// has none to strip. An empty `query` is the `invalid_request` the `error_contract` suite
/// of `rift-mcp` provokes.
#[tokio::test]
async fn a_failing_tool_call_returns_the_same_error_text_through_both_proxies() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let request = json!({"query": ""});

    let (all, text) = tokio::join!(
        proxy_client_with(root, &["--output", "all"]),
        proxy_client_with(root, &["--output", "text"]),
    );
    let (all, text) = (all?, text?);
    await_workspace_ready(&all).await?;
    let (all_result, text_result) = tokio::join!(
        proxied_result(&all, "search", &request),
        proxied_result(&text, "search", &request),
    );
    let (all_result, text_result) = (all_result?, text_result?);

    for result in [&all_result, &text_result] {
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert!(result.structured_content.is_none(), "{result:?}");
    }
    let (all_failure, text_failure) = (tool_failure(&all_result)?, tool_failure(&text_result)?);
    assert_eq!(
        all_failure.code,
        ErrorCode::InvalidRequest,
        "{all_failure:?}"
    );
    assert_eq!(all_failure.retry, RetryDirective::Never, "{all_failure:?}");
    assert!(!all_failure.message.is_empty(), "{all_failure:?}");
    assert_eq!(all_failure.text, text_failure.text);
    assert_eq!(all_failure.message, text_failure.message);

    all.cancel().await?;
    text.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// Stops the serving server mid-session and returns its document once it has left.
async fn stop_serving_server(root: &Path) -> TestResult<ServerLock> {
    let first = serving_document(root).ok_or("the first server must serve")?;
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "server stop mid-session")?;
    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server to leave",
        || serving_document(root).is_none().then_some(()),
    )
    .await?;
    Ok(first)
}

/// The fixture keeps the default serving range: a second server is elected in the
/// same workspace, and the range lets it bind whether or not the first server's
/// port is free yet.
#[tokio::test]
async fn proxy_session_reconnects_after_a_server_restart() -> TestResult {
    let directory = laid_out_workspace(&[("lib.rs", LIBRARY)], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let first = stop_serving_server(root).await?;

    assert_beacon(&beacon_lookup(&client).await?);
    let second = serving_document(root).ok_or("the reconnect must elect a server")?;
    assert_ne!(
        second.pid, first.pid,
        "the same session must be served by a freshly elected process"
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// The same restart on a text proxy: the reconnected session keeps its output selection.
#[tokio::test]
async fn text_proxy_session_keeps_its_output_after_a_server_restart() -> TestResult {
    let directory = laid_out_workspace(&[("lib.rs", LIBRARY)], "")?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let client = proxy_client_with(root, &["--output=text"]).await?;
    let tools = listed_tools(&client).await?;
    let result = beacon_result(&client).await?;
    assert_text_shape(&tools, &result);
    let first = stop_serving_server(root).await?;

    let tools = listed_tools(&client).await?;
    let result = beacon_result(&client).await?;
    assert_text_shape(&tools, &result);
    let second = serving_document(root).ok_or("the reconnect must elect a server")?;
    assert_ne!(
        second.pid, first.pid,
        "the same session must be served by a freshly elected process"
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn stale_lock_document_yields_a_fresh_election() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    fs::create_dir_all(root.join(".rift"))?;
    let stale = ServerLock {
        port: 12_345,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        identity: rift_binary_identity()?,
        server: None,
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
    failure_window.passed();
    Ok(())
}

/// The zero-byte file under `.rift` a server claims the election on, by
/// locking it exclusively.
const ELECTION_FILE_NAME: &str = "server.lock";

/// Polls the proxy's stderr while waiting for a spawned server's refusal,
/// at [`PRESENCE_POLL_INTERVAL`], inside the proxy's start window.
const RECORD_READ_ATTEMPT_COUNT: u32 = 50;

/// A claim that meets any lock on the election file loses the start election,
/// a shared one included. This test keeps a shared lock on the file the way a
/// probe's lock outlives the probe on Windows, which releases a closed handle's
/// locks lazily, so the proxy's first spawned server loses an election no
/// process holds and exits. Once the lock goes, the proxy spawns again inside
/// its start window, and the call is served.
// Startup refusal observation: https://github.com/volarized/rift/issues/490
#[tokio::test]
async fn a_start_lost_to_a_lingering_shared_lock_spawns_again() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    fs::create_dir_all(root.join(".rift"))?;
    let lingering = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".rift").join(ELECTION_FILE_NAME))?;
    lingering.try_lock_shared()?;

    let (client, stderr) = relayed_proxy_client(root).await?;
    let released = async {
        let lost = wait_for(
            RECORD_READ_ATTEMPT_COUNT,
            "the lost election's stderr refusal",
            || {
                let printed = stderr.snapshot();
                printed
                    .contains("error[server_already_serving]")
                    .then_some(printed)
            },
        )
        .await;
        assert!(
            !root.join(".rift/index").exists(),
            "a refused child cannot open the held workspace database"
        );
        lingering.unlock()?;
        drop(lingering);
        lost.map(|_stderr| ())
    };
    let (lookup, lost) = tokio::join!(beacon_lookup(&client), released);
    lost?;
    assert_beacon(&lookup?);
    serving_document(root).ok_or("a spawn after the lost election must serve")?;
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// A workspace server this test holds in place of a `rift server` of another release.
///
/// It holds the election, publishes its lock document, and answers an authorized
/// `POST /api/stop` the way a server does - `202 Accepted`, or a reset connection from
/// one already stopping - then closes its port and releases the election, so another
/// server can be elected.
struct RecordedServer {
    accepted: Arc<AtomicUsize>,
    stop: Arc<Notify>,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RecordedServer {
    /// Holds the election, binds a port, and publishes a lock recording `version`, in the
    /// shape a release that digested its executable wrote: `executable_digest` beside the
    /// version. Answers the guard, the bound port, and the token the lock records.
    async fn published(
        root: &Path,
        version: &str,
    ) -> TestResult<(ElectionGuard, tokio::net::TcpListener, String)> {
        let guard = claim(root)?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let token = "r".repeat(SERVER_TOKEN_LENGTH);
        let document = json!({
            "port": listener.local_addr()?.port(),
            "token": token,
            "pid": std::process::id(),
            "identity": {
                "version": version,
                "executable_digest": "e".repeat(64),
                "schema_digest": "b".repeat(64),
            },
        });
        fs::write(document_path(root), serde_json::to_vec(&document)?)?;
        Ok((guard, listener, token))
    }

    /// Starts a server whose lock records `version` and that accepts a stop request.
    async fn start(root: &Path, version: &str) -> TestResult<Self> {
        let (guard, listener, token) = Self::published(root, version).await?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(Notify::new());
        let expected = format!("Bearer {token}");
        let (counted, stopping) = (Arc::clone(&accepted), Arc::clone(&stop));
        let router = axum::Router::new().route(
            "/api/stop",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let (counted, stopping) = (Arc::clone(&counted), Arc::clone(&stopping));
                let authorized = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .is_some_and(|value| value.as_bytes() == expected.as_bytes());
                async move {
                    if !authorized {
                        return axum::http::StatusCode::UNAUTHORIZED;
                    }
                    counted.fetch_add(1, Ordering::SeqCst);
                    stopping.notify_one();
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        );
        let shutdown = Arc::clone(&stop);
        let served = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.notified().await })
                .await?;
            // The port closed first; the election releases with the guard, as a
            // stopping server's does.
            drop(guard);
            Ok::<(), std::io::Error>(())
        });
        Ok(Self {
            accepted,
            stop,
            served,
        })
    }

    /// Starts a server whose lock records `version` and that is already stopping when the
    /// stop request arrives: it resets that request's connection without an answer, which
    /// reaches the proxy as a transport failure, as the exit of a stopping server does on
    /// Windows. Then it closes its port and releases the election.
    async fn start_resetting_stop(root: &Path, version: &str) -> TestResult<Self> {
        let (guard, listener, token) = Self::published(root, version).await?;
        let accepted = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(Notify::new());
        let expected = format!("authorization: bearer {token}");
        let (counted, shutdown) = (Arc::clone(&accepted), Arc::clone(&stop));
        let served = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! {
                    () = shutdown.notified() => break,
                    connection = listener.accept() => connection?,
                };
                // A presence probe connects and sends nothing; only the stop request
                // arrives with a head.
                let mut head = vec![0_u8; 4_096];
                let read = tokio::io::AsyncReadExt::read(&mut stream, &mut head)
                    .await
                    .unwrap_or(0);
                let head = String::from_utf8_lossy(&head[..read]).to_ascii_lowercase();
                if head.starts_with("post /api/stop") && head.contains(&expected) {
                    counted.fetch_add(1, Ordering::SeqCst);
                    stream.set_zero_linger()?;
                    break;
                }
            }
            drop(listener);
            drop(guard);
            Ok::<(), std::io::Error>(())
        });
        Ok(Self {
            accepted,
            stop,
            served,
        })
    }

    /// How many authorized stop requests the server acted on.
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }

    /// Waits for the server to close its port and release the election, stopping it
    /// first when no request did.
    async fn stopped(self) -> TestResult {
        self.stop.notify_one();
        within("the recorded server to release its election", self.served).await???;
        Ok(())
    }
}

/// A proxy asks the server of an older release to stop, starts one from its own binary
/// once the election releases, and serves the request. The older server's lock carries
/// `executable_digest`, as every lock before the build identity did, so this is also what
/// an existing `.rift/` meets after an upgrade.
#[tokio::test]
async fn an_older_server_is_replaced_and_the_request_served() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let older = RecordedServer::start(root, "0.0.1").await?;

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    assert!(
        older.accepted() >= 1,
        "the proxy asks the older server to stop"
    );
    older.stopped().await?;
    let serving = serving_document(root).ok_or("a server of this build must serve")?;
    assert_eq!(serving.identity, rift_binary_identity()?);
    assert_ne!(serving.pid, std::process::id());
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// A stop request that meets an older server already stopping fails in transport, and
/// does not decide the outcome: the proxy reads the election again, finds it released,
/// starts a server from its own binary, and serves the request.
#[tokio::test]
async fn a_stop_reset_by_a_leaving_older_server_still_ends_with_a_new_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let older = RecordedServer::start_resetting_stop(root, "0.0.1").await?;

    let client = proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    assert_eq!(
        older.accepted(),
        1,
        "the proxy asks the older server to stop once"
    );
    older.stopped().await?;
    let serving = serving_document(root).ok_or("a server of this build must serve")?;
    assert_eq!(serving.identity, rift_binary_identity()?);
    assert_ne!(serving.pid, std::process::id());
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// Serves a proxy against a recorded server of `version`, and requires its call to be
/// refused with both identities and the operator's next step, the server never asked to
/// stop.
async fn assert_refused_without_a_stop(version: &str) -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let recorded = RecordedServer::start(root, version).await?;

    let client = proxy_client(root).await?;
    let refusal = within(
        "the refusal from a server this proxy does not replace",
        client.call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "beacon"}))?),
        ),
    )
    .await?
    .expect_err("the recorded server must refuse the call");
    let rmcp::ServiceError::McpError(data) = refusal else {
        panic!("expected a protocol-level refusal, got {refusal:?}");
    };
    assert!(
        data.message
            .contains("workspace server identity differs from this rift process"),
        "{}",
        data.message
    );
    assert!(data.message.contains(version), "{}", data.message);
    assert!(
        data.message.contains("run rift server stop, then retry"),
        "{}",
        data.message
    );
    assert_eq!(recorded.accepted(), 0, "the server is never asked to stop");
    client.cancel().await?;
    recorded.stopped().await?;
    failure_window.passed();
    Ok(())
}

/// A proxy never displaces a newer server.
#[tokio::test]
async fn a_newer_server_is_refused_with_operator_guidance() -> TestResult {
    assert_refused_without_a_stop("999.0.0").await
}

/// Two development builds of one version never replace each other: a server of this
/// version from another commit is refused, as a newer one is.
#[tokio::test]
async fn another_build_of_this_version_is_refused_with_operator_guidance() -> TestResult {
    let another_build = format!(
        "{}+71ea9ed284538bd4b5429df592afd7424e2bad13",
        env!("CARGO_PKG_VERSION")
    );
    assert_refused_without_a_stop(&another_build).await
}

/// Two proxies of one build race to replace the same older server. Both may ask it to
/// stop and both may start a server, and the election keeps exactly one: each proxy
/// connects to that one server, whose pid the lock records, and both requests are
/// served by it.
#[tokio::test]
async fn two_proxies_replacing_one_older_server_end_with_one_new_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let older = RecordedServer::start(root, "0.0.1").await?;

    let (first, second) = tokio::join!(relayed_proxy_client(root), relayed_proxy_client(root));
    let ((first, first_stderr), (second, second_stderr)) = (first?, second?);
    let (first_lookup, second_lookup) = tokio::join!(beacon_lookup(&first), beacon_lookup(&second));
    assert_beacon(&first_lookup?);
    assert_beacon(&second_lookup?);
    assert!(older.accepted() >= 1, "the older server is asked to stop");
    older.stopped().await?;

    let serving = serving_document(root).ok_or("one server of this build must serve")?;
    assert_eq!(serving.identity, rift_binary_identity()?);
    first.cancel().await?;
    second.cancel().await?;
    let survivor = serving_document(root).ok_or("the shared server must outlive both")?;
    assert_eq!(survivor.pid, serving.pid);
    let stopped = run_rift(root, &["server", "stop"]).await?;
    require_success(&stopped, "stop the replacing server")?;

    for stderr in [first_stderr.text().await?, second_stderr.text().await?] {
        let connected = connected_pids(&stderr);
        assert_eq!(
            connected,
            [serving.pid],
            "each proxy connects to the one elected server: {}",
            harness::bounded_tail(&stderr)
        );
    }
    failure_window.passed();
    Ok(())
}

/// The pid of every server a proxy's stderr says it connected to, in order.
fn connected_pids(stderr: &str) -> Vec<u32> {
    stderr
        .lines()
        .filter(|line| line.contains("proxy connected to workspace server"))
        .filter_map(|line| {
            let (_, after) = line.split_once(" pid=")?;
            after.split_whitespace().next()?.parse().ok()
        })
        .collect()
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
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let guard = claim(root)?;
    guard.publish(&ServerLock {
        port: refusing_port_in_range()?,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        identity: rift_binary_identity()?,
        server: None,
    })?;

    let (client, stderr) = relayed_proxy_client(root).await?;

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
        data.message.contains(&format!("{START_WAIT_MAX:?}")),
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
    let stderr = stderr.text().await?;
    assert!(
        stderr.contains("recorded server did not answer"),
        "the stale server must be diagnosed: {}",
        harness::bounded_tail(&stderr)
    );
    assert!(
        stderr.contains("upstream warmup did not connect"),
        "{}",
        harness::bounded_tail(&stderr)
    );
    drop(guard);
    failure_window.passed();
    Ok(())
}

/// The refusal an agent sees when the workspace's own spawned server
/// exists but cannot bind its configured port: the detached spawn
/// succeeds, the child prints its own startup failure to stderr and exits
/// before publishing a lock document, and the proxy reports a redacted summary
/// of captured stderr instead of waiting out the poll's own window.
///
/// The pinned port stays held for the whole test. The redacted stderr summary
/// and absence of the poll-exhaustion refusal prove the spawned process exit
/// supplied the result.
#[tokio::test]
async fn a_spawned_server_that_cannot_bind_its_port_refuses_with_redacted_stderr() -> TestResult {
    let held = held_port_in_range()?;
    let port = held.local_addr()?.port();
    let directory = laid_out_workspace(&[("lib.rs", LIBRARY)], &format!("port = {port}\n"))?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

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
    assert_eq!(
        data.message,
        "spawned server exited before serving: stderr [redacted], stderr_truncated false; report this internal failure with its full context"
    );
    let wire_error: wire::ErrorData = serde_json::from_value(
        data.data
            .expect("spawn refusal retains typed wire classification"),
    )?;
    assert_eq!(wire_error.code, wire::ErrorCode::TemporarilyUnavailable);
    assert_eq!(wire_error.retry, wire::RetryDirective::SameRequest);
    assert!(
        !data.message.contains('\u{1b}'),
        "a server whose stderr is a pipe writes no terminal escape codes into it: {}",
        data.message
    );
    assert!(
        !data.message.contains("server_already_serving"),
        "a genuine bind failure must not be mistaken for a lost election: {}",
        data.message
    );

    client.cancel().await?;
    drop(held);
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn proxy_stderr_carries_lifecycle_lines_and_never_the_token() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);

    let (client, stderr) = relayed_proxy_client(root).await?;
    assert_beacon(&beacon_lookup(&client).await?);
    let token = serving_document(root)
        .ok_or("the elected server must serve")?
        .token;
    client.cancel().await?;

    let stderr = stderr.text().await?;
    for line in ["MCP proxy starting", "MCP proxy ready", "MCP proxy stopped"] {
        assert!(
            stderr.contains(line),
            "{line}: {}",
            harness::bounded_tail(&stderr)
        );
    }
    assert!(
        !stderr.contains(&token),
        "the bearer token must never reach stderr"
    );
    assert!(
        !stderr.contains(&root.display().to_string()),
        "tracing exposed the workspace root: {}",
        harness::bounded_tail(&stderr)
    );
    failure_window.passed();
    Ok(())
}

/// Incoming references use the configured engine through the real CLI proxy.
#[tokio::test]
async fn live_proxied_read_resolves_incoming_references() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let directory = rust_engine_workspace()?;
    let root = directory.path();
    rust_engine::require_rust_analyzer(root);
    let _cleanup = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let client = proxy_client(root).await?;
    await_workspace_ready(&client).await?;
    let declaration = proxied_call(&client, "get_symbol", &json!({"name": "beacon"})).await?;
    let seed = declaration["hits"][0]["symbol"]["id"]
        .as_str()
        .ok_or("beacon declaration must carry its symbol identity")?;
    let result = proxied_engine_call(
        &client,
        "search",
        &json!({"traversal": {
            "seed": seed, "direction": "incoming", "facets": ["references"], "depth": 1
        }}),
    )
    .await?;
    let caller = result["results"]
        .as_array()
        .ok_or("search must return its results")?
        .iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == "total")
        .ok_or_else(|| format!("incoming references must reach the caller: {result:#}"))?;
    assert_eq!(caller["traversal_path"][0]["direction"], "incoming");
    assert_eq!(caller["traversal_path"][0]["relationship"]["to"], seed);
    assert_eq!(
        caller["traversal_path"][0]["relationship"]["derivation"],
        "resolution"
    );
    let workspace = within(
        "workspace resource",
        client.read_resource(rmcp::model::ReadResourceRequestParams::new(
            "rift://workspace",
        )),
    )
    .await??;
    let configuration = resource_json(&workspace, "rift://workspace")?;
    let rust = configuration["languages"]
        .as_array()
        .ok_or("workspace must list languages")?
        .iter()
        .find(|language| language["language"] == "rust")
        .ok_or("workspace must report Rust")?;
    assert_eq!(rust["lsp"]["state"], "ready", "{configuration}");
    assert_eq!(
        fs::read_to_string(root.join("hub.rs"))?,
        harness::RUST_PROJECT_HUB
    );
    client.cancel().await?;
    require_success(
        &run_rift(root, &["server", "stop"]).await?,
        "stop after reference read",
    )?;
    failure_window.passed();
    Ok(())
}
