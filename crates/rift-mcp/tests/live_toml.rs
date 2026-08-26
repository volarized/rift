//! Live integration: the `patch` tool over a TOML file served by tombi.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_toml` runs the
//! suite; without the variable the test skips visibly. This is the proof
//! that step 6's TOML syntax provider and step 7's engine wiring close the
//! whole chain with no code change of their own: once a `.toml` file
//! carries a `Language`, `engine_change_diagnostics`
//! (`crates/rift-server/src/diagnose.rs`) already resolves the configured
//! `[engines.toml]` entry for it, the same way it resolves `[engines.rust]`
//! for a `.rs` file.
//!
//! tombi announces no `$/progress` work, so the server's readiness gate has
//! nothing to read: a pull answered before tombi has the document ready
//! comes back empty, and an empty answer is indistinguishable from a clean
//! file. The asserting test therefore warms the engine first, the way the
//! rust and typescript live suites do, for the same measured reason - this
//! suite ran with no warm-up until a cold linux runner applied the change
//! and carried no finding.

#![cfg(unix)]

mod engine_fixture;
mod hermetic_search;
mod live_engine_gate;
mod toml_engine;
mod workspace_client;

use std::time::Duration;

use live_engine_gate::engine_live;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use toml_engine::{require_tombi, toml_engine_configuration};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

/// A well-formed table with one key, so the patch below is the file's only
/// violation.
const CONFIG: &str = "[server]\nname = \"primary\"\n";

/// Appends a second `name` key under `[server]`, tombi's `duplicate-key`
/// violation.
const DUPLICATE_KEY_PATCH: &str = "--- a/config.toml\n+++ b/config.toml\n@@ -1,2 +1,3 @@\n \
                                    [server]\n name = \"primary\"\n+name = \"secondary\"\n";

/// [`CONFIG`] with the duplicate key already present.
const CONFIG_WITH_DUPLICATE: &str = "[server]\nname = \"primary\"\nname = \"secondary\"\n";

/// The inverse of [`DUPLICATE_KEY_PATCH`], removing the duplicate line.
const REMOVE_DUPLICATE_KEY_PATCH: &str = "--- a/config.toml\n+++ b/config.toml\n@@ -1,3 +1,2 @@\n \
                                           [server]\n name = \"primary\"\n-name = \"secondary\"\n";

/// The findings one applied change carries under `code`.
fn coded_findings<'summary>(structured: &'summary Value, code: &str) -> Vec<&'summary Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!(code))
                .collect()
        })
        .unwrap_or_default()
}

/// Most warm-up attempts one test makes, and the pause between them: at most
/// fifteen seconds of waiting, then the test fails instead of hanging.
const WARMUP_ATTEMPTS_MAX: usize = 60;
const WARMUP_PAUSE: Duration = Duration::from_millis(250);

/// Drives `patch` until tombi's own finding rides an applied change.
///
/// Each attempt introduces the duplicate key and removes it again, so the
/// file is left exactly as the attempt found it and the next assertion runs
/// against the same starting bytes. The loop ends on the first attempt whose
/// summary carries the finding, which is the only signal that separates a
/// tombi still opening the document from a tombi reporting a clean file.
async fn warmed_engine(client: &RunningService<RoleClient, ()>) -> TestResult {
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let introduce = tool_request("patch", &json!({ "patch": DUPLICATE_KEY_PATCH }));
        let introduced = call_retrying_acceptance(client, introduce).await?;
        let answered = !coded_findings(&introduced, "duplicate-key").is_empty();
        let remove = tool_request("patch", &json!({ "patch": REMOVE_DUPLICATE_KEY_PATCH }));
        call_retrying_acceptance(client, remove).await?;
        if answered {
            return Ok(());
        }
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    Err("tombi never reported the duplicate key".into())
}

/// The applied patch carries tombi's own finding for the file it changed:
/// the pull runs on the document Rift just wrote, and tombi's document-tree
/// construction catches the duplicate key with no schema and no network
/// access, so the finding is deterministic and immediate.
///
/// The workspace is served under a root spelled relative to the process
/// working directory, the spelling `rift mcp` and `rift server start` hand
/// the server. tombi is addressed in `file://` URIs, which carry no working
/// directory, so a real engine is the strictest witness that the spelling
/// reaches it resolved.
#[tokio::test]
async fn applied_patch_carries_the_toml_engine_diagnostic() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_relative_workspace(
        &[("config.toml", CONFIG)],
        Some(toml_engine_configuration()),
    )
    .await?;
    require_tombi(directory.path());
    warmed_engine(&client).await?;

    let introduce = tool_request("patch", &json!({ "patch": DUPLICATE_KEY_PATCH }));
    let structured = call_retrying_acceptance(&client, introduce).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(structured["summary"]["paths"], json!(["config.toml"]));

    let findings = coded_findings(&structured, "duplicate-key");
    assert_eq!(
        findings.len(),
        1,
        "tombi's duplicate-key finding rides the applied change: {structured:#}"
    );
    let finding = findings[0];
    assert_eq!(finding["severity"], json!("error"));
    assert_eq!(
        finding["language"],
        json!({ "name": "toml" }),
        "the engine's language stamps the finding: {finding:#}"
    );
    assert_eq!(finding["message"], json!("duplicate key: name"));
    assert_eq!(finding["span"]["unit"], json!("rift://file/config.toml"));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A patch that removes the file's only violation carries no
/// `duplicate-key` finding: the pull is discriminating, not just always
/// empty, which is what makes the finding in
/// [`applied_patch_carries_the_toml_engine_diagnostic`] evidence that the
/// chain closed rather than a coincidence of a misconfigured engine.
#[tokio::test]
async fn applied_patch_over_a_clean_file_carries_no_finding() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_workspace(
        &[("config.toml", CONFIG_WITH_DUPLICATE)],
        Some(toml_engine_configuration()),
    )
    .await?;
    require_tombi(directory.path());

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": REMOVE_DUPLICATE_KEY_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(structured["summary"]["paths"], json!(["config.toml"]));
    assert!(
        coded_findings(&structured, "duplicate-key").is_empty(),
        "a change that removes the violation leaves no finding: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
