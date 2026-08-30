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

use harness::{
    SERIAL, StopOnDrop, TestResult, proxy_client, require_success, run_rift, within, workspace,
};
use rmcp::model::ReadResourceRequestParams;
use serde_json::Value;

/// The whole recorded set.
const LOGS_URI: &str = "rift://logs";
/// Reads one case spends waiting for the drain to write its first batch.
const RECORD_ATTEMPTS: u32 = 40;
/// Wall-clock span between two of those reads.
const RECORD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
/// The sentence a workspace with no recorded diagnostics prints on stderr.
const NOTHING_RECORDED: &str = "no server diagnostics recorded for this workspace yet";

/// The records one read answered with, as the wire carried them.
fn records(text: &str) -> TestResult<Vec<Value>> {
    let body: Value = serde_json::from_str(text)?;
    Ok(body["records"]
        .as_array()
        .ok_or("a log read must answer with a records array")?
        .clone())
}

/// Reads one log URI until it answers with records, or the attempts run out.
///
/// The drain writes in batches, so a read issued the instant a call returns can
/// legitimately find nothing yet. The bound is the test's: a server that
/// records nothing fails here rather than hanging.
async fn recorded(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    uri: &str,
) -> TestResult<Vec<Value>> {
    for _attempt in 0..RECORD_ATTEMPTS {
        let text = read_resource(client, uri).await?;
        let found = records(&text)?;
        if !found.is_empty() {
            return Ok(found);
        }
        tokio::time::sleep(RECORD_POLL_INTERVAL).await;
    }
    Err(format!("no records reached {uri} within the bound").into())
}

/// One resource read through the proxy, returning its single text content.
async fn read_resource(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    uri: &str,
) -> TestResult<String> {
    let answer = within(
        "a resource read",
        client.read_resource(ReadResourceRequestParams::new(uri.to_owned())),
    )
    .await??;
    match answer.contents.first() {
        Some(rmcp::model::ResourceContents::TextResourceContents { text, .. }) => Ok(text.clone()),
        other => Err(format!("a log read answers with text, not {other:?}").into()),
    }
}

/// The lines one run printed on stdout.
fn printed_lines(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The timestamp column one printed line opens with.
fn printed_timestamp(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or_default()
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
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
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
    Ok(())
}

#[tokio::test]
async fn a_served_workspace_records_its_own_startup() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
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
    Ok(())
}

#[tokio::test]
async fn a_component_read_returns_only_that_component() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;

    let records = recorded(&client, "rift://logs/component/mcp").await?;

    for record in &records {
        assert_eq!(record["component"], "mcp", "{records:?}");
    }
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn the_logs_command_prints_the_recorded_set_oldest_first() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
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
        let stamp = printed_timestamp(line).to_owned();
        assert!(
            stamp.ends_with('Z'),
            "every line opens with a timestamp: {line:?}"
        );
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
    Ok(())
}

#[tokio::test]
async fn the_logs_command_honors_its_tail_and_level() -> TestResult {
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;
    recorded(&client, LOGS_URI).await?;

    let tailed = run_rift(directory.path(), &["server", "logs", "--tail", "1"]).await?;
    let failures = run_rift(directory.path(), &["server", "logs", "--level", "error"]).await?;

    require_success(&tailed, "rift server logs --tail 1")?;
    require_success(&failures, "rift server logs --level error")?;
    assert_eq!(printed_lines(&tailed).len(), 1);
    for line in printed_lines(&failures) {
        assert!(line.contains("🔴 ERROR"), "{line:?}");
    }
    client.cancel().await?;
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
    let _serial = SERIAL.lock().await;
    let directory = workspace()?;
    let _stop = StopOnDrop::new(directory.path());
    let client = proxy_client(directory.path()).await?;
    within("a tool listing", client.list_tools(None)).await??;
    recorded(&client, LOGS_URI).await?;
    let output = tempfile::tempdir()?;
    let transcript = output.path().join("followed.txt");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rift"))
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
    Ok(())
}
