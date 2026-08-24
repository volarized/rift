//! Integration tests: engine diagnostics attached to applied changes.
//!
//! Every change tool shares one post-apply attach point, proven here for
//! `replace_symbol` and `rename_symbol` against the scripted fake engine:
//! mapped findings for each severity, the bound, the capability-honest
//! silence, and the one warning an engine death degrades to.

#![cfg(unix)]

#[path = "support/fake_engine.rs"]
mod fake_engine;
mod support;

use std::fs;

use fake_engine::engine_configuration;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use support::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

const LIBRARY: &str = "pub fn beacon() {}\n";

/// Attempts the exhaustion test gives one pull through its own
/// `[engines.fake.retry]` table.
const CONFIGURED_ATTEMPTS: u64 = 3;

/// The same engine table with an `[engines.fake.retry]` policy on it.
///
/// The exhaustion test states its own bound, so it is small and the waits
/// between attempts are short enough to keep the suite quick; the growth
/// of those waits is proven by the policy's own unit tests.
fn retrying(configuration: &str, attempts: u64, delay: &str) -> String {
    format!(
        "{configuration}\n[engines.fake.retry]\nattempts = {attempts}\n\
         delay = \"{delay}\"\ndelay_limit = \"{delay}\"\n"
    )
}

fn replace_request(body: &str) -> CallToolRequestParams {
    tool_request(
        "replace_symbol",
        &json!({
            "symbol": "rift://symbol/rust/lib.rs/beacon",
            "body": body
        }),
    )
}

/// The diagnostics the engine contributed: every finding stamped with the
/// engine's language, which Rift's own findings never carry.
fn engine_findings(structured: &Value) -> Vec<&Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["language"]["name"] == json!("rust"))
                .collect()
        })
        .unwrap_or_default()
}

/// The warnings one failed engine degraded to.
fn engine_warnings(structured: &Value) -> Vec<&Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!("rift.engine.failed"))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn applied_replace_gains_mapped_findings_for_each_severity() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("diagnostic-severities", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(
        &client,
        replace_request("pub fn beacon() -> u8 {\n    7\n}"),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = engine_findings(&structured);
    assert_eq!(findings.len(), 4, "{structured:#}");
    let severity_of = |message: &str| {
        findings
            .iter()
            .find(|finding| finding["message"] == json!(message))
            .unwrap_or_else(|| panic!("finding {message} must ride the summary"))["severity"]
            .clone()
    };
    assert_eq!(severity_of("scripted error"), json!("error"));
    assert_eq!(severity_of("scripted warning"), json!("warning"));
    assert_eq!(severity_of("scripted information"), json!("info"));
    assert_eq!(severity_of("scripted hint"), json!("hint"));
    let coded = findings
        .iter()
        .find(|finding| finding["message"] == json!("scripted error"))
        .expect("the error finding rides");
    assert_eq!(coded["code"], json!("E100"), "string codes carry over");
    let numeric = findings
        .iter()
        .find(|finding| finding["message"] == json!("scripted information"))
        .expect("the information finding rides");
    assert!(
        numeric.get("code").is_none_or(Value::is_null),
        "numeric codes drop: {numeric:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A two-file unified diff over the fixture's `lib.rs` and `main.rs`.
const TWO_FILE_PATCH: &str = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n\
                              -pub fn beacon() {}\n+pub fn beacon() -> u8 { 1 }\n\
                              --- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n\
                              -pub fn caller() {}\n+pub fn caller() -> u8 { 2 }\n";

/// The bound holds across paths - `patch` here also proves a third change
/// tool routes through the same attach point: the first flooded path fills
/// the budget, and the second path is never pulled.
#[tokio::test]
async fn engine_findings_are_bounded_per_change_across_paths() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", "pub fn caller() {}\n")],
        Some(engine_configuration("diagnostic-flood", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": TWO_FILE_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        engine_findings(&structured).len(),
        rift_server::ENGINE_DIAGNOSTICS_PER_CHANGE_MAX,
        "a flooding engine is truncated at the named bound: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// One engine death yields one warning even when the change touched
/// several of its files: the dead engine is not asked again.
#[tokio::test]
async fn a_dead_engine_is_not_asked_again_for_later_paths() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", "pub fn caller() {}\n")],
        Some(engine_configuration("dies-on-diagnostic", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": TWO_FILE_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        engine_warnings(&structured).len(),
        1,
        "the second changed path must not raise a second warning: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_death_after_apply_degrades_to_one_warning() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("dies-on-diagnostic", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, replace_request("pub fn beacon() -> u8 { 7 }")).await?;
    assert_eq!(
        structured["status"],
        json!("applied"),
        "an engine death after apply never fails the call: {structured:#}"
    );
    let warnings = engine_warnings(&structured);
    assert_eq!(warnings.len(), 1, "{structured:#}");
    assert_eq!(warnings[0]["severity"], json!("warning"));
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fake"),
        "the warning names the engine: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn beacon() -> u8 { 7 }\n",
        "the change stays applied"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A cancelled pull is the engine asking to be asked again: the resend
/// lands the findings, and nothing degrades. The engine stamps each pull's
/// ordinal into its answer, so the message proves the second pull served.
#[tokio::test]
async fn a_cancelled_pull_is_resent_and_its_findings_ride_the_change() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("cancels-first-diagnostic", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, replace_request("pub fn beacon() -> u8 { 7 }")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = engine_findings(&structured);
    assert_eq!(findings.len(), 1, "{structured:#}");
    assert_eq!(
        findings[0]["message"],
        json!("settled on pull 2"),
        "the second pull is the one that answered: {structured:#}"
    );
    assert!(
        engine_warnings(&structured).is_empty(),
        "a refusal the resend recovered from never degrades: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// An engine that keeps cancelling exhausts the attempt budget and then
/// degrades exactly as a terminal failure does: one warning, the change
/// still applied. The engine stamps each pull's ordinal into its refusal,
/// so the last one names how many pulls ran, and the table's own
/// `attempts` - not the shipped default - is the number it names.
#[tokio::test]
async fn a_cancelling_engine_degrades_once_the_attempts_run_out() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(retrying(
            &engine_configuration("cancels-every-diagnostic", "20s"),
            CONFIGURED_ATTEMPTS,
            "1ms",
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, replace_request("pub fn beacon() -> u8 { 7 }")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = engine_warnings(&structured);
    assert_eq!(
        warnings.len(),
        1,
        "an exhausted budget degrades to one warning: {structured:#}"
    );
    let expected = format!("declined pull {CONFIGURED_ATTEMPTS}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains(&expected),
        "the warning carries the last of {CONFIGURED_ATTEMPTS} configured attempts: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn beacon() -> u8 { 7 }\n",
        "the change stays applied"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A refusal the engine did not invite again is never resent: the warning
/// carries the first pull's ordinal, so the engine was asked exactly once.
#[tokio::test]
async fn a_terminal_refusal_is_never_resent() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("refuses-diagnostic", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, replace_request("pub fn beacon() -> u8 { 7 }")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = engine_warnings(&structured);
    assert_eq!(warnings.len(), 1, "{structured:#}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("declined pull 1"),
        "a terminal refusal is answered once and degraded: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_without_pulls_stays_silent() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("no-pull-diagnostics", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, replace_request("pub fn beacon() -> u8 { 7 }")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["diagnostics"],
        json!([]),
        "no pull capability means no attempt and no warning: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The rename tool routes through the same attach point: an applied rename
/// carries the engine's pulled findings for its rewritten files.
#[tokio::test]
async fn applied_rename_gains_the_same_engine_findings() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[
            ("lib.rs", LIBRARY),
            ("main.rs", "pub fn caller() { beacon(); }\n"),
        ],
        Some(engine_configuration("renames-word-diagnostics", "20s")),
    )
    .await?;

    let rename = tool_request(
        "rename_symbol",
        &json!({
            "symbol": "rift://symbol/rust/lib.rs/beacon",
            "new_name": "flare"
        }),
    );
    let structured = call_retrying_acceptance(&client, rename).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = engine_findings(&structured);
    assert!(
        findings.len() >= 4,
        "the rename's changed paths pull the scripted findings: {structured:#}"
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding["message"] == json!("scripted hint")),
        "{structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
