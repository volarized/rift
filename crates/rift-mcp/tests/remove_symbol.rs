//! Integration tests: `remove_symbol` against the scripted fake engine.
//!
//! Every test wires rift-lsp's `fake_engine` binary into the workspace's
//! own `rift.toml` through an overlaid `PATH`, exactly as an operator's
//! `[engines.<name>]` table would resolve a real engine, and drives the
//! tool through a live rmcp client. `live_rust_analyzer.rs` proves the
//! same tool over a real engine's standing-reference refusal; these tests
//! script the reference check itself: an engine that cannot check at all,
//! one that answers the check with a refusal, and one that answers a
//! standing reference across two files.

#![cfg(unix)]

mod fake_engine;
mod hermetic_search;
mod workspace_client;

use std::fs;

use fake_engine::{counted, engine_configuration, recorded};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

fn remove_request(symbol: &str, force: bool) -> CallToolRequestParams {
    tool_request(
        "remove_symbol",
        &json!({ "symbol": symbol, "force": force }),
    )
}

/// The library and the file referencing it, both served by the engine.
const LIBRARY: &str = "pub fn beacon() {}\n";
const CALLER: &str = "pub fn caller() { beacon(); }\n";
const BEACON_SYMBOL: &str = "rift://symbol/rust/lib.rs/beacon";

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

/// The `references-only` engine advertises `referencesProvider` and nothing else - exactly
/// what a removal's reference check needs. `caller.rs` calls `beacon` once, so the scripted
/// engine's own word-boundary scan finds it across both files and the removal refuses,
/// naming the caller's path. The lifecycle log proves the engine was asked for references
/// exactly once - the refusal is the engine's own verdict, not a resend.
#[tokio::test]
async fn remove_symbol_with_a_standing_reference_over_the_fake_engine_refuses_and_names_the_caller()
-> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(counted(
            &engine_configuration("references-only", "20s"),
            &log,
            3,
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, remove_request(BEACON_SYMBOL, false)).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    let preconditions = structured["preconditions"]
        .as_array()
        .expect("a failed condition rides the refusal");
    assert_eq!(preconditions[0]["kind"], json!("no_references"));
    let paths = preconditions[0]["paths"]
        .as_array()
        .expect("the failed condition names the reference paths");
    assert!(
        paths.contains(&json!("main.rs")),
        "the refusal must name the referencing file: {structured:#}"
    );
    assert_eq!(
        recorded(&log, "references"),
        1,
        "the engine's own verdict is never asked for twice"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY,
        "a refused removal leaves the tree untouched"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A workspace served under the CLI's own root spelling removes the same way one served
/// under an absolute path does: reads and writes below the root resolve against the process
/// working directory either way, so only the engine tier - addressed in `file://` URIs -
/// could tell the two spellings apart, and no engine is configured here.
#[tokio::test]
async fn remove_symbol_under_a_relative_root_applies_unchecked() -> TestResult {
    let (directory, client, server_task) =
        served_relative_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let structured =
        call_retrying_acceptance(&client, remove_request(BEACON_SYMBOL, false)).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.unchecked");
    assert_eq!(findings.len(), 1, "{structured:#}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("no engine is configured for language rust"),
        "the warning must name the absent engine: {findings:#?}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "",
        "the sole declaration is removed"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// `no-rename-capability` advertises no capability at all, `referencesProvider` included: the
/// removal cannot ask the question, so it applies and the warning names the absent
/// capability by its wire method name.
#[tokio::test]
async fn remove_symbol_over_an_engine_without_the_references_capability_applies_unchecked()
-> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("no-rename-capability", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, remove_request(BEACON_SYMBOL, false)).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.unchecked");
    assert_eq!(findings.len(), 1, "{structured:#}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("does not advertise textDocument/references"),
        "the warning must name the absent capability: {findings:#?}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "",
        "the sole declaration is removed"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// `refuses-references` advertises the capability but answers the request itself with a
/// JSON-RPC error: the removal cannot read a verdict from that, so it applies unchecked and
/// the warning says the engine did not answer.
#[tokio::test]
async fn remove_symbol_over_an_engine_that_refuses_references_applies_unchecked() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("refuses-references", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, remove_request(BEACON_SYMBOL, false)).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.unchecked");
    assert_eq!(findings.len(), 1, "{structured:#}");
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("did not answer the reference check"),
        "the warning must say the engine did not answer: {findings:#?}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "",
        "the sole declaration is removed"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The engine drifts the file on disk while answering the reference check: the plan compiled
/// clean, but the change lane re-proves the base against the disk before writing, and the
/// drifted bytes refuse the write.
#[tokio::test]
async fn disk_mutation_between_remove_plan_and_apply_refuses_source_unchanged() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration(
            "mutates-then-answers-references",
            "20s",
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, remove_request(BEACON_SYMBOL, false)).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    assert_eq!(
        structured["preconditions"][0]["kind"],
        json!("source_unchanged"),
        "{structured:#}"
    );
    let on_disk = fs::read_to_string(directory.path().join("lib.rs"))?;
    assert!(
        on_disk.contains("the engine drifted this file"),
        "the engine's own write stays: {on_disk}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
