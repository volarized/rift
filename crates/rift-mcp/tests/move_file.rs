//! Integration tests: `move_file` without an engine, and its typed
//! refusals.
//!
//! The engine-covered arms - a proposal, a `null` answer, an out-of-tree
//! escape, absorption across an empty or provisional answer, and every
//! capability-grid shape - are proven against real engines instead of a
//! scripted one: `live_rust_analyzer.rs` and `live_typescript.rs` prove
//! the applied move and its reference rewrite, and the absorption and
//! empty-answer scenarios that stood here belong to the engine readiness
//! rework that follows this step, which lands its own coverage through
//! this crate's live suites once it reworks that policy.

#![cfg(unix)]

mod hermetic_search;
// `served_relative_workspace` is part of `workspace_client`'s shared surface; this binary
// never needs a relative-root fixture, since the tests here carry no engine for the root
// spelling to matter to.
#[allow(dead_code)]
mod workspace_client;

use std::fs;

use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

fn move_request(from: &str, to: &str) -> CallToolRequestParams {
    tool_request("move_file", &json!({ "from": from, "to": to }))
}

/// The moved module and the file referencing it by stem.
const HUB: &str = "pub fn hub() {}\n// hub module\n";
const CALLER: &str = "mod hub;\n";

fn move_warnings(structured: &Value) -> Vec<&Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!("rift.move.references_not_updated"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn applied_move_without_an_engine_carries_the_warning() -> TestResult {
    let (directory, client, server_task) =
        served_workspace(&[("hub.rs", HUB), ("main.rs", CALLER)], None).await?;

    let structured =
        call_retrying_acceptance(&client, move_request("hub.rs", "moved/spoke.rs")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = move_warnings(&structured);
    assert_eq!(warnings.len(), 1, "{structured:#}");
    assert_eq!(warnings[0]["severity"], json!("warning"));
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no engine is configured for language rust"),
        "the warning names the reason: {structured:#}"
    );
    assert!(!directory.path().join("hub.rs").exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("moved/spoke.rs"))?,
        HUB,
        "the move lands into a created directory with the bytes unchanged"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("main.rs"))?,
        CALLER,
        "references stay as they were"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn missing_source_and_occupied_destination_refuse() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB), ("taken.rs", "pub fn taken() {}\n")],
        None,
    )
    .await?;

    let missing =
        call_retrying_acceptance(&client, move_request("vanished.rs", "spoke.rs")).await?;
    assert_eq!(missing["status"], json!("refused"), "{missing:#}");
    assert_eq!(missing["reason"], json!("unmet_precondition"));
    assert_eq!(
        missing["preconditions"][0]["kind"],
        json!("target_exists"),
        "{missing:#}"
    );

    let occupied = call_retrying_acceptance(&client, move_request("hub.rs", "taken.rs")).await?;
    assert_eq!(occupied["status"], json!("refused"), "{occupied:#}");
    assert_eq!(
        occupied["preconditions"][0]["expected"],
        json!({ "kind": "boolean", "value": false }),
        "{occupied:#}"
    );
    assert_eq!(fs::read_to_string(directory.path().join("hub.rs"))?, HUB);

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn illegal_paths_fail_as_typed_invalid_requests() -> TestResult {
    let (_directory, client, server_task) = served_workspace(&[("hub.rs", HUB)], None).await?;

    for (from, to) in [
        ("hub.rs", "hub.rs"),
        ("hub.rs", ".rift/x.rs"),
        ("hub.rs", "../escape.rs"),
    ] {
        let error = client
            .call_tool(move_request(from, to))
            .await
            .expect_err("an illegal move request fails before resolution");
        let rmcp::ServiceError::McpError(data) = error else {
            return Err(format!("expected a tool error, got {error:?}").into());
        };
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(
            wire["code"],
            json!("invalid_request"),
            "{from} -> {to}: {wire:#}"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
