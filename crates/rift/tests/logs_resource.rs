//! Reading the server back end to end: `rift://logs` for an agent, and
//! `rift server logs` for an operator.
//!
//! Every case here drives the compiled binary and the real `rift mcp` proxy,
//! because the proxy is what an agent talks to and it forwards resource
//! traffic of its own. A suite that called the server handler directly would
//! prove nothing about the path that broke: the proxy forwarded tool calls
//! alone until v0.0.21. The command cases drive the same binary, because a
//! stopped server's records are exactly what an operator reads back.

// The shared helper files serve every end-to-end suite in this crate; this one
// drives the resource surface and reaches a subset of them.
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod engine_fixture;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod harness;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod rust_engine;
#[allow(
    dead_code,
    reason = "shared test identity helper, used by sibling suites"
)]
mod test_case;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use harness::{
    FailureWindow, LIBRARY, RelayedStderr, StopOnDrop, TestResult, await_workspace_ready,
    laid_out_workspace, proxied_call, proxy_client, require_success, run_rift, within, workspace,
};
use rift_mcp::{BuildCheckout, PRESENCE_POLL_INTERVAL, ServerPresence};
use rmcp::model::ReadResourceRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::Value;

/// The whole recorded set.
const LOGS_URI: &str = "rift://logs";
/// Longest one case waits for a log read to answer with records, the reads included.
///
/// The drain writes every 250 milliseconds, and a read waits at most
/// [`rift_tracing::LOG_SETTLE_TIMEOUT`] for it, so this covers several of those waits.
/// With [`harness::WINDOW_READ_MAX`] and the proxy's [`rift_mcp::START_WAIT_MAX`] before
/// it, the case stays inside nextest's one-minute deadline, so a server that records
/// nothing, or stops answering, fails with its failure window instead of being ended
/// silently.
const RECORDED_WAIT_MAX: Duration = Duration::from_secs(15);
/// Polls one case spends waiting for the follower to print a record.
const RECORD_ATTEMPTS: u32 = 40;
/// Wall-clock span between two reads, or two polls of the follower's transcript.
const RECORD_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The sentence a workspace with no recorded diagnostics prints on stderr.
const NOTHING_RECORDED: &str = "no server diagnostics recorded for this workspace yet";

/// The records one read answered with, as the wire carried them.
fn records(body: &Value) -> TestResult<Vec<Value>> {
    Ok(body["records"]
        .as_array()
        .ok_or("a log read must answer with a records array")?
        .clone())
}

/// How the reads of one log URI went before the case gave up on them.
#[derive(Debug, Default)]
struct ReadHistory {
    /// Reads that answered without a record.
    answered_empty: u32,
    /// The longest any of those reads took to answer.
    longest_read: Duration,
    /// The last of those answers, as the wire carried it.
    last_answer: Option<String>,
    /// Whether the read in flight when the wait ended had not answered.
    stalled: bool,
}

/// Reads one log URI until it answers with records, or [`RECORDED_WAIT_MAX`] passes.
///
/// The drain writes in batches, so a read issued the instant a call returns can
/// legitimately find nothing yet. The bound is wall-clock and covers the reads
/// themselves, since each one can wait for the drain on the server's side: at most
/// `RECORDED_WAIT_MAX` over [`RECORD_POLL_INTERVAL`] reads run. A server that records
/// nothing, or stops answering, fails here with what the reads answered; the case's
/// [`harness::FailureWindow`] then prints what the store holds.
async fn recorded(client: &RunningService<RoleClient, ()>, uri: &str) -> TestResult<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + RECORDED_WAIT_MAX;
    let mut history = ReadHistory::default();
    loop {
        let started = tokio::time::Instant::now();
        let Ok(read) = tokio::time::timeout_at(deadline, read_resource(client, uri)).await else {
            history.stalled = true;
            break;
        };
        let body = read?;
        let found = records(&body)?;
        if !found.is_empty() {
            return Ok(found);
        }
        history.answered_empty += 1;
        history.longest_read = history.longest_read.max(started.elapsed());
        history.last_answer = Some(body.to_string());
        if tokio::time::Instant::now() + RECORD_POLL_INTERVAL >= deadline {
            break;
        }
        tokio::time::sleep(RECORD_POLL_INTERVAL).await;
    }
    Err(unrecorded(uri, &history).into())
}

/// Why a case's reads of `uri` ended without a record.
fn unrecorded(uri: &str, history: &ReadHistory) -> String {
    let answered = format!(
        "{} reads answered without a record, the longest after {:?}",
        history.answered_empty, history.longest_read
    );
    let outcome = if history.stalled {
        format!("a read was still unanswered when the wait ended; {answered}")
    } else {
        answered
    };
    let last_answer = history.last_answer.as_deref().unwrap_or("none");
    format!(
        "no record reached {uri} within {RECORDED_WAIT_MAX:?}: {outcome}\nlast answer: \
         {last_answer}"
    )
}

/// One resource read through the proxy, returning its JSON body.
///
/// The first content is the compact text, whose header counts the records.
async fn read_resource(client: &RunningService<RoleClient, ()>, uri: &str) -> TestResult<Value> {
    let answer = client
        .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
        .await?;
    let text = harness::resource_text(&answer, uri)?;
    let header = text.lines().next().unwrap_or_default();
    if !(header.ends_with(" record") || header.ends_with(" records")) {
        return Err(format!("{uri}: the text header must count records: {text}").into());
    }
    harness::resource_json(&answer, uri)
}

/// The record lines one run printed on stdout, without the blank lines between groups.
fn printed_lines(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Polls `transcript` for `needle`, bounded by [`RECORD_ATTEMPTS`] reads.
///
/// The follower writes as it prints, so a read that has not seen the line yet
/// only means the record has not landed; the bound is the test's, so a
/// follower that prints nothing fails here rather than hanging.
async fn awaited(transcript: &std::path::Path, needle: &str) -> bool {
    for _attempt in 0..RECORD_ATTEMPTS {
        let seen = std::fs::read_to_string(transcript).is_ok_and(|text| text.contains(needle));
        if seen {
            return true;
        }
        tokio::time::sleep(RECORD_POLL_INTERVAL).await;
    }
    false
}

#[tokio::test]
async fn the_proxy_lists_the_log_resource_and_its_templates() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;

    let listed = within("resources/list", client.list_resources(None)).await??;
    let templates = within(
        "resources/templates/list",
        client.list_resource_templates(None),
    )
    .await??;

    assert!(
        listed
            .resources
            .iter()
            .any(|resource| resource.uri == LOGS_URI),
        "{:?}",
        listed.resources
    );
    let spellings: Vec<&str> = templates
        .resource_templates
        .iter()
        .map(|template| template.uri_template.as_str())
        .collect();
    assert!(
        spellings.contains(&"rift://logs/level/{level}"),
        "{spellings:?}"
    );
    assert!(
        spellings.contains(&"rift://logs/component/{component}"),
        "{spellings:?}"
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn a_served_workspace_records_its_own_startup() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;
    // One call proves the server is serving, so the records it wrote exist to be read.
    within("a search", client.list_tools(None)).await??;

    let records = recorded(&client, LOGS_URI).await?;

    assert!(
        records
            .iter()
            .any(|record| record["component"] == "mcp" || record["component"] == "index"),
        "{records:?}"
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn a_component_read_returns_only_that_component() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;

    let records = recorded(&client, "rift://logs/component/mcp").await?;

    for record in &records {
        assert_eq!(record["component"], "mcp", "{records:?}");
    }
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn the_logs_command_prints_the_recorded_set_oldest_first() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;
    let seen = recorded(&client, LOGS_URI).await?;
    let oldest = seen
        .last()
        .and_then(|record| record["message"].as_str())
        .ok_or("the recorded set must carry a message")?
        .to_owned();

    let printed = run_rift(directory.path(), &["server", "logs"]).await?;

    require_success(&printed, "rift server logs")?;
    let lines = printed_lines(&printed);
    assert!(!lines.is_empty(), "{lines:?}");
    let mut previous = String::new();
    for line in &lines {
        let stamp = harness::printed_timestamp(line);
        assert!(
            stamp.is_some(),
            "every line opens with a UTC timestamp: {line:?}"
        );
        let stamp = stamp.unwrap_or_default().to_owned();
        assert!(stamp >= previous, "records print oldest first: {lines:?}");
        previous = stamp;
    }
    assert!(
        lines.len() >= seen.len(),
        "the command prints at least what the resource read answered: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains(&oldest)),
        "the command prints the record the resource read named oldest: \
         {oldest:?} missing from {lines:?}"
    );
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn the_logs_command_honors_its_tail_and_level() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;
    recorded(&client, LOGS_URI).await?;

    let tailed = run_rift(directory.path(), &["server", "logs", "--tail", "1"]).await?;
    let failures = run_rift(directory.path(), &["server", "logs", "--level", "error"]).await?;

    require_success(&tailed, "rift server logs --tail 1")?;
    require_success(&failures, "rift server logs --level error")?;
    assert_eq!(printed_lines(&tailed).len(), 1);
    for line in printed_lines(&failures) {
        assert_eq!(line.split_whitespace().nth(2), Some("ERROR"), "{line:?}");
    }
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

/// The lifecycle records a failure window reads, which an inherited `RUST_LOG` must not
/// filter out of the store.
const LIFECYCLE_RECORDS: [&str; 2] = ["MCP server ready", "MCP server stopped"];

/// An inherited `RUST_LOG=warn` narrows a server's stderr and leaves its store alone: the
/// store records under `[logs] capture`, so the lifecycle records a window reads are kept
/// while stderr carries warnings alone (#479). Replays the inherited filter of the
/// tracing plan's failure list.
#[test]
fn an_inherited_warn_filter_leaves_the_lifecycle_records_in_the_store() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _stop = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    // How long each command ran, so a store missing its stop records shows whether the
    // stop ran to its deadline.
    let mut commands = Vec::new();
    for arguments in [["server", "start"], ["server", "stop"]] {
        let mut command = std::process::Command::new(harness::rift_binary());
        let started = std::time::Instant::now();
        let output = harness::with_child_log_variables(&mut command)
            .env("RUST_LOG", "warn")
            .args(arguments)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .output()?;
        commands.push(format!(
            "rift {} exited {:?} after {} ms",
            arguments.join(" "),
            output.status,
            started.elapsed().as_millis()
        ));
        require_success(&output, &format!("rift {}", arguments.join(" ")))?;
    }

    let mut command = std::process::Command::new(harness::rift_binary());
    let printed = harness::with_child_log_variables(&mut command)
        .args(["server", "logs"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()?;

    require_success(&printed, "rift server logs")?;
    let lines = printed_lines(&printed);
    for lifecycle in LIFECYCLE_RECORDS {
        assert!(
            lines.iter().any(|line| line.contains(lifecycle)),
            "the store keeps {lifecycle:?}: {lines:?}; {}",
            commands.join("; ")
        );
    }
    let stderr = std::fs::read_to_string(rift_mcp::stderr_file_path(root))?;
    assert!(
        !stderr.lines().any(|line| line.contains(" INFO ")),
        "the inherited filter keeps information records off stderr: {}",
        harness::bounded_tail(&stderr)
    );
    failure_window.passed();
    Ok(())
}

/// A stop publishes the table of operations in flight as it begins, and the store keeps
/// it: the stop half of the held write replay of the tracing plan's failure list. Which
/// operations are open when the stop lands depends on the server's background work, so
/// the case asserts the record and its fields, not its entries.
#[test]
fn a_stop_records_the_operations_still_in_flight() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _stop = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    for arguments in [["server", "start"], ["server", "stop"]] {
        let mut command = std::process::Command::new(harness::rift_binary());
        let output = harness::with_child_log_variables(&mut command)
            .args(arguments)
            .current_dir(root)
            .stdin(std::process::Stdio::null())
            .output()?;
        require_success(&output, &format!("rift {}", arguments.join(" ")))?;
    }

    let mut command = std::process::Command::new(harness::rift_binary());
    let printed = harness::with_child_log_variables(&mut command)
        .args(["server", "logs"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()?;

    require_success(&printed, "rift server logs")?;
    let lines = printed_lines(&printed);
    let published: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("operations in flight") && line.contains("reason=stop"))
        .collect();
    assert_eq!(published.len(), 1, "one table at the stop: {lines:?}");
    for field in ["in_flight=", "left_out=", "untracked=", "operations=["] {
        assert!(published[0].contains(field), "{field} in {}", published[0]);
    }
    failure_window.passed();
    Ok(())
}

/// `[search] busy_timeout` of the case that holds the index database's write lock: past
/// [`RECORDED_WAIT_MAX`], so a log read that waited on that lock would outlast its bound.
const HELD_INDEX_BUSY_TIMEOUT: &str = "30s";

/// Another process's write lock on `.rift/index` delays no log read: while a second
/// connection holds `BEGIN IMMEDIATE` on the index database of a serving workspace,
/// `rift://logs` and `rift server logs` both answer with records inside the bound this
/// suite gives a log read. The index database waits `[search] busy_timeout` for that lock,
/// and the fixture sets it past that bound.
#[tokio::test]
async fn a_held_index_write_lock_delays_no_log_read() -> TestResult {
    let directory = harness::laid_out_workspace(
        &[("lib.rs", harness::LIBRARY)],
        &format!(
            "{}[search]\nbusy_timeout = \"{HELD_INDEX_BUSY_TIMEOUT}\"\n",
            harness::assigned_port_key()?
        ),
    )?;
    let root = directory.path();
    let _stop = StopOnDrop::new(root);
    let failure_window = FailureWindow::begin(root);
    let client = proxy_client(root).await?;
    within("a tool listing", client.list_tools(None)).await??;
    let holder = rusqlite::Connection::open(root.join(".rift").join("index"))?;
    holder.busy_timeout(RECORDED_WAIT_MAX)?;
    holder.execute_batch("BEGIN IMMEDIATE")?;

    let read = recorded(&client, LOGS_URI).await?;
    let printed = tokio::time::timeout(
        RECORDED_WAIT_MAX,
        run_rift(root, &["server", "logs", "--tail", "5"]),
    )
    .await
    .map_err(|_elapsed| "rift server logs waited past its bound")??;
    let still_held = !holder.is_autocommit();
    holder.execute_batch("ROLLBACK")?;
    drop(holder);

    assert!(
        still_held,
        "the index write lock stayed held through both reads"
    );
    assert!(!read.is_empty(), "{read:?}");
    require_success(&printed, "rift server logs")?;
    assert!(!printed_lines(&printed).is_empty(), "{printed:?}");
    client.cancel().await?;
    failure_window.passed();
    Ok(())
}

#[tokio::test]
async fn an_unrecorded_workspace_says_so_and_creates_no_state() -> TestResult {
    let directory = tempfile::tempdir()?;

    let printed = run_rift(directory.path(), &["server", "logs"]).await?;

    require_success(&printed, "rift server logs without a recorded database")?;
    assert!(printed.stdout.is_empty(), "{:?}", printed.stdout);
    let reported = String::from_utf8_lossy(&printed.stderr);
    assert!(reported.contains(NOTHING_RECORDED), "{reported}");
    assert!(reported.contains("rift server start"), "{reported}");
    assert!(
        !directory.path().join(".rift").exists(),
        "a logs read never creates the state directory"
    );
    Ok(())
}

#[tokio::test]
async fn a_followed_read_prints_a_record_the_server_writes_later() -> TestResult {
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let failure_window = FailureWindow::begin(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;
    recorded(&client, LOGS_URI).await?;
    let output = tempfile::tempdir()?;
    let transcript = output.path().join("followed.txt");
    let mut command = std::process::Command::new(harness::rift_binary());
    let mut child = harness::with_child_log_variables(&mut command)
        .args(["server", "logs", "--follow"])
        .current_dir(directory.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(&transcript)?)
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let started = awaited(&transcript, "MCP server ready").await;

    // The serving process records its own stop before the drain writes what is
    // left and the process exits, so this is a record that lands after the
    // follower printed everything the store already held.
    let stopped = run_rift(directory.path(), &["server", "stop"]).await;
    let followed = awaited(&transcript, "MCP server stopped").await;

    child.kill()?;
    child.wait()?;
    let _ = client.cancel().await;
    require_success(&stopped?, "rift server stop")?;
    assert!(
        started,
        "the follower must print the set the store already held"
    );
    assert!(
        followed,
        "the follower must print the record the server wrote after it started"
    );
    failure_window.passed();
    Ok(())
}

/// Probes one repository foreground start waits for its election document, at
/// [`PRESENCE_POLL_INTERVAL`]: ten seconds.
const REPOSITORY_START_ATTEMPTS: u32 = 100;
/// Longest a repository foreground stop may take, the `rift server stop` command and the
/// process exit included: the server's own stop keeps four seconds of it.
const REPOSITORY_STOP_MAX: Duration = Duration::from_secs(5);

/// The foreground repository server one case started; killed if the case ends first.
struct RepositoryForeground(Child);

impl Drop for RepositoryForeground {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs one Git command in `root` under a fixed identity, unsigned.
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

/// The repository's election directory for the `rift` binary under test.
fn repository_state_directory(main: &Path) -> TestResult<PathBuf> {
    let common = rift_mcp::repository::discover_common_directory(main)
        .ok_or("the fixture repository has a common Git directory")?;
    let checkout = BuildCheckout::recorded(env!("RIFT_BUILD_COMMIT"), env!("RIFT_BUILD_DIRTY"));
    let identity = rift_mcp::product_identity_of(checkout, &harness::rift_binary())?;
    Ok(rift_mcp::repository::repository_election_directory(
        &common, &identity,
    )?)
}

/// Starts `rift server start --foreground --repository` in `root` and waits until its
/// election document names it; answers the child and its relayed stderr.
async fn repository_foreground(
    root: &Path,
    state_directory: &Path,
) -> TestResult<(RepositoryForeground, RelayedStderr)> {
    let mut command = Command::new(harness::rift_binary());
    let mut child = RepositoryForeground(
        harness::with_child_log_variables(&mut command)
            .args(["server", "start", "--foreground", "--repository"])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let stderr =
        harness::relayed_child_stderr(&mut child.0, "rift server start --foreground --repository")?;
    for _ in 0..REPOSITORY_START_ATTEMPTS {
        if child.0.try_wait()?.is_some() {
            return Err(format!(
                "the repository server exited before serving: {}",
                stderr.snapshot()
            )
            .into());
        }
        if let ServerPresence::Serving(lock) = rift_mcp::probe_state_directory(state_directory)
            && lock.pid == child.0.id()
        {
            return Ok((child, stderr));
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err("the repository server did not publish its election document".into())
}

/// Stops the repository server through `rift server stop --repository` in `root` and
/// waits for a clean exit, both within [`REPOSITORY_STOP_MAX`].
async fn stop_repository_foreground(root: &Path, child: &mut RepositoryForeground) -> TestResult {
    let deadline = tokio::time::Instant::now() + REPOSITORY_STOP_MAX;
    let stopped = tokio::time::timeout_at(
        deadline,
        run_rift(root, &["server", "stop", "--repository"]),
    )
    .await??;
    require_success(&stopped, "repository foreground stop")?;
    loop {
        if let Some(status) = child.0.try_wait()? {
            harness::record_exit(child.0.id(), status);
            assert!(
                status.success(),
                "the repository server exits cleanly: {status:?}"
            );
            return Ok(());
        }
        tokio::time::timeout_at(deadline, tokio::time::sleep(PRESENCE_POLL_INTERVAL)).await?;
    }
}

/// Whether any string inside `value` contains `text`.
fn mentions(value: &Value, text: &str) -> bool {
    match value {
        Value::String(string) => string.contains(text),
        Value::Array(items) => items.iter().any(|item| mentions(item, text)),
        Value::Object(members) => members.values().any(|member| mentions(member, text)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// Under repository serving, each workspace's `rift://logs` answers the records its own
/// requests produced, naming its own root and never the other workspace's; after the
/// server stops, `rift server logs` in that root reads the same store; and the stop ends
/// every stage, the `log drain` stage included, with the outcome `ok`.
#[tokio::test]
async fn a_repository_served_workspace_answers_its_own_records() -> TestResult {
    let main = laid_out_workspace(&[("lib.rs", LIBRARY)], &harness::assigned_port_key()?)?;
    fixture_git(main.path(), &["init", "-q"])?;
    fixture_git(main.path(), &["add", "lib.rs", "rift.toml"])?;
    fixture_git(
        main.path(),
        &["-c", "commit.gpgsign=false", "commit", "-qm", "add sources"],
    )?;
    let linked_parent = tempfile::tempdir()?;
    let linked = linked_parent.path().join("linked");
    fixture_git(
        main.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().ok_or("the fixture path is UTF-8")?,
            "HEAD",
        ],
    )?;
    let roots = [
        std::fs::canonicalize(main.path())?,
        std::fs::canonicalize(&linked)?,
    ];
    let failure_window = FailureWindow::begin_over(&[roots[0].as_path(), roots[1].as_path()]);
    let state_directory = repository_state_directory(main.path())?;
    let (mut child, stderr) = repository_foreground(&roots[0], &state_directory).await?;

    let mut clients = Vec::new();
    for root in &roots {
        let client = proxy_client(root).await?;
        await_workspace_ready(&client).await?;
        let lookup = proxied_call(
            &client,
            "get_symbol",
            &serde_json::json!({"name": "beacon"}),
        )
        .await?;
        assert_eq!(lookup["hits"][0]["symbol"]["name"], "beacon", "{lookup}");
        clients.push(client);
    }
    for (index, client) in clients.iter().enumerate() {
        let own = roots[index].display().to_string();
        let other = roots[1 - index].display().to_string();
        let records = recorded(client, LOGS_URI).await?;
        assert!(
            records.iter().any(|record| mentions(record, &own)),
            "{own} answers a record naming it: {records:?}"
        );
        assert!(
            !records.iter().any(|record| mentions(record, &other)),
            "{own} answers no record of {other}: {records:?}"
        );
    }
    for client in clients {
        client.cancel().await?;
    }
    stop_repository_foreground(&roots[0], &mut child).await?;

    let stop_lines = stderr.text().await?;
    let stages = stop_lines
        .lines()
        .filter(|line| line.contains("stop stage ended"))
        .collect::<Vec<_>>();
    assert!(
        stages.iter().any(|line| line.contains("stage=log drain")),
        "the repository stop runs a log drain stage: {stages:#?}"
    );
    for stage in &stages {
        // XFAIL: https://github.com/volarized/rift/issues/568
        // Retain the recorded Intel macOS export timeout; every other stage must end ok.
        if cfg!(all(target_os = "macos", target_arch = "x86_64"))
            && stage.contains("stage=otlp export")
            && stage.contains("outcome=timeout")
            && stage.contains("remaining=0ns")
        {
            eprintln!("XFAIL https://github.com/volarized/rift/issues/568: {stage}");
        } else {
            assert!(
                stage.contains("outcome=ok"),
                "every stop stage ends ok: {stages:#?}"
            );
        }
    }
    for (index, root) in roots.iter().enumerate() {
        let output = run_rift(root, &["server", "logs"]).await?;
        require_success(&output, "rift server logs after the stop")?;
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            printed.contains(&root.display().to_string()),
            "the stopped workspace's store answers its records: {printed}"
        );
        assert!(
            !printed.contains(&roots[1 - index].display().to_string()),
            "the stopped workspace's store holds no record of the other: {printed}"
        );
    }
    failure_window.passed();
    Ok(())
}
