//! `rift://logs` end to end: the surface an agent reads the server back through.
//!
//! Every case here drives the compiled binary and the real `rift mcp` proxy,
//! because the proxy is what an agent talks to and it forwards resource
//! traffic of its own. A suite that called the server handler directly would
//! prove nothing about the path that broke: the proxy forwarded tool calls
//! alone until v0.0.21.

// The shared helper files serve every end-to-end suite in this crate; this one
// drives the resource surface and reaches a subset of them.
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod engine_fixture;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod harness;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod rust_engine;

use harness::{SERIAL, StopOnDrop, TestResult, proxy_client, within, workspace};
use rmcp::model::ReadResourceRequestParams;
use serde_json::Value;

/// The whole recorded set.
const LOGS_URI: &str = "rift://logs";
/// Reads one case spends waiting for the drain to write its first batch.
const RECORD_ATTEMPTS: u32 = 40;
/// Wall-clock span between two of those reads.
const RECORD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

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
