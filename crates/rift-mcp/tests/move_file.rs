//! Integration tests: `move_file` against the scripted fake engine.
//!
//! The engine arms cover the capability grid the live pair exhibits:
//! will-rename present and absent, filters covering and missing the file,
//! and a proposal, a `null` answer, and an out-of-tree escape.

#![cfg(unix)]

mod workspace_client;

use std::fs;

use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, engine_configuration, served_workspace, tool_request,
};

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

fn refusal_detail(structured: &Value) -> String {
    structured["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
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
async fn applied_move_with_an_engine_rewrites_the_referencing_file() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB), ("main.rs", CALLER)],
        Some(engine_configuration("moves-imports", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(&client, move_request("hub.rs", "spoke.rs")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        move_warnings(&structured).is_empty(),
        "an engine-covered move carries no warning: {structured:#}"
    );
    assert_eq!(
        structured["summary"]["paths"],
        json!(["hub.rs", "main.rs", "spoke.rs"]),
        "the summary carries the old path, the rewrite, and the new path"
    );
    assert!(!directory.path().join("hub.rs").exists());
    assert_eq!(
        fs::read_to_string(directory.path().join("spoke.rs"))?,
        "pub fn spoke() {}\n// spoke module\n",
        "edits addressed to the moved file land on the moved bytes"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("main.rs"))?,
        "mod spoke;\n"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn null_proposal_moves_the_file_without_a_warning() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB)],
        Some(engine_configuration("moves-null", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(&client, move_request("hub.rs", "spoke.rs")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        move_warnings(&structured).is_empty(),
        "a consulted engine that proposes nothing is not a skip: {structured:#}"
    );
    assert_eq!(fs::read_to_string(directory.path().join("spoke.rs"))?, HUB);

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_without_will_rename_moves_with_the_capability_warning() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB)],
        Some(engine_configuration("no-file-operations", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(&client, move_request("hub.rs", "spoke.rs")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = move_warnings(&structured);
    assert_eq!(warnings.len(), 1, "{structured:#}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("does not advertise workspace/willRenameFiles"),
        "the warning names the absent capability: {structured:#}"
    );
    assert_eq!(fs::read_to_string(directory.path().join("spoke.rs"))?, HUB);

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn filters_missing_the_file_move_with_the_filter_warning() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB)],
        Some(engine_configuration("python-filters", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(&client, move_request("hub.rs", "spoke.rs")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = move_warnings(&structured);
    assert_eq!(warnings.len(), 1, "{structured:#}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("filters do not cover the moved file"),
        "the warning names the filter mismatch: {structured:#}"
    );
    assert_eq!(fs::read_to_string(directory.path().join("spoke.rs"))?, HUB);

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn out_of_tree_proposal_refuses_and_nothing_moves() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB)],
        Some(engine_configuration("moves-outside-root", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(&client, move_request("hub.rs", "spoke.rs")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"));
    assert!(
        refusal_detail(&structured).contains("outside the workspace tree"),
        "the refusal names the escape: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("hub.rs"))?,
        HUB,
        "the refusal leaves the tree untouched, the move included"
    );
    assert!(!directory.path().join("spoke.rs").exists());

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
async fn engine_timeout_is_a_typed_error_and_nothing_moves() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("hub.rs", HUB)],
        Some(engine_configuration("parks-on-move", "1s")),
    )
    .await?;

    let error = client
        .call_tool(move_request("hub.rs", "spoke.rs"))
        .await
        .expect_err("a parked will-rename request must time out as a typed error");
    let rmcp::ServiceError::McpError(error) = error else {
        return Err(format!("expected a tool error, got {error:?}").into());
    };
    let code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str);
    assert_eq!(code, Some("temporarily_unavailable"), "{error:?}");
    assert_eq!(fs::read_to_string(directory.path().join("hub.rs"))?, HUB);
    assert!(!directory.path().join("spoke.rs").exists());

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
        let rmcp::ServiceError::McpError(error) = error else {
            return Err(format!("expected a tool error, got {error:?}").into());
        };
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some("invalid_request"), "{from} -> {to}: {error:?}");
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
