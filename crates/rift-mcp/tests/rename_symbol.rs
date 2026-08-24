//! Integration tests: `rename_symbol` against the scripted fake engine.
//!
//! Every test wires rift-lsp's `fake_engine` binary into the workspace's
//! own `rift.toml` through an overlaid `PATH`, exactly as an operator's
//! `[engines.<name>]` table would resolve a real engine, and drives the
//! tool through a live rmcp client.

#![cfg(unix)]

mod fake_engine;
mod workspace_client;

use std::fs;

use fake_engine::{counted, engine_configuration, recorded};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

fn rename_request(symbol: &str, new_name: &str) -> CallToolRequestParams {
    tool_request(
        "rename_symbol",
        &json!({ "symbol": symbol, "new_name": new_name }),
    )
}

/// One tool call expected to fail, returning its wire error payload.
fn wire_error(error: rmcp::ServiceError) -> TestResult<Value> {
    let rmcp::ServiceError::McpError(data) = error else {
        return Err(format!("expected a tool error, got {error:?}").into());
    };
    data.data
        .ok_or_else(|| "wire error data must be present".into())
}

/// The library and the file referencing it, both served by the engine.
const LIBRARY: &str = "pub fn beacon() {}\n";
const CALLER: &str = "pub fn caller() { beacon(); }\n";
const BEACON_SYMBOL: &str = "rift://symbol/rust/lib.rs/beacon";

fn survivor_findings(structured: &Value) -> Vec<&Value> {
    structured["summary"]["diagnostics"]
        .as_array()
        .map(|findings| {
            findings
                .iter()
                .filter(|finding| finding["code"] == json!("rift.rename.survivor"))
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
async fn applied_rename_rewrites_every_referencing_file() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(engine_configuration("renames-word", "20s")),
    )
    .await?;

    let tools = client.list_all_tools().await?;
    let rename_tool = tools
        .iter()
        .find(|tool| tool.name == "rename_symbol")
        .ok_or("rename_symbol must be advertised")?;
    let output_schema = rename_tool
        .output_schema
        .as_ref()
        .map(|schema| Value::Object(schema.as_ref().clone()))
        .ok_or("rename_symbol must advertise an output schema")?;
    let validator = jsonschema::validator_for(&output_schema)?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    let failures: Vec<String> = validator
        .iter_errors(&structured)
        .map(|failure| failure.to_string())
        .collect();
    assert!(
        failures.is_empty(),
        "the live result must validate against the advertised schema: {failures:#?}"
    );
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["paths"],
        json!(["lib.rs", "main.rs"]),
        "both files carry the rename"
    );
    assert_eq!(
        structured["summary"]["edits"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        2,
        "one whole-file edit per rewritten file"
    );
    assert!(
        survivor_findings(&structured).is_empty(),
        "a full rename must sweep clean: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn flare() {}\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("main.rs"))?,
        "pub fn caller() { flare(); }\n"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A workspace served under the CLI's own root spelling renames the same
/// way one served under an absolute path does.
///
/// `rift mcp` and `rift server start` serve the process working directory,
/// which they name `.`. Reads and writes below the root resolve against
/// that directory either way, so the spelling is invisible to them; the
/// engine tier is addressed in `file://` URIs, which carry no working
/// directory, so it is the one caller that can tell the spellings apart.
#[tokio::test]
async fn applied_rename_under_a_relative_root_rewrites_every_referencing_file() -> TestResult {
    let (directory, client, server_task) = served_relative_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(engine_configuration("renames-word", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        survivor_findings(&structured).is_empty(),
        "a full rename must sweep clean: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn flare() {}\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("main.rs"))?,
        "pub fn caller() { flare(); }\n"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn applied_rename_reports_the_surviving_occurrence() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[
            ("lib.rs", LIBRARY),
            ("main.rs", CALLER),
            ("notes.md", "# beacon\nThe beacon stays documented here.\n"),
        ],
        Some(engine_configuration("renames-word", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let survivors = survivor_findings(&structured);
    assert!(
        !survivors.is_empty(),
        "the markdown mention must surface as a survivor: {structured:#}"
    );
    for survivor in survivors {
        assert_eq!(survivor["severity"], json!("warning"));
        assert!(
            survivor["span"]["unit"]
                .as_str()
                .unwrap_or_default()
                .contains("notes.md"),
            "the survivor names its file: {survivor:#}"
        );
    }
    assert_eq!(
        fs::read_to_string(directory.path().join("notes.md"))?,
        "# beacon\nThe beacon stays documented here.\n",
        "the sweep reports and never rewrites"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn unserved_language_refuses_unsupported() -> TestResult {
    let (_directory, client, server_task) = served_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"));
    assert!(
        refusal_detail(&structured).contains("no engine configured for language rust"),
        "the refusal names the unserved language: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_without_the_rename_capability_refuses_unsupported() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("no-rename-capability", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"));
    assert!(
        refusal_detail(&structured).contains("does not advertise"),
        "the refusal names the missing capability: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn declined_prepare_refuses_with_the_engine_verdict() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("declines-prepare", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    assert!(
        refusal_detail(&structured).contains("serves no rename at this declaration"),
        "the refusal carries the prepare verdict: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn out_of_tree_proposal_refuses_and_leaves_the_tree() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("renames-outside-root", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unsupported"));
    assert!(
        refusal_detail(&structured).contains("outside the workspace tree"),
        "the refusal names the escape: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_timeout_is_a_typed_error_and_the_tree_is_untouched() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("parks-on-rename", "1s")),
    )
    .await?;

    let error = client
        .call_tool(rename_request(BEACON_SYMBOL, "flare"))
        .await
        .expect_err("a parked engine must time out as a typed error");
    let wire = wire_error(error)?;
    assert_eq!(wire["code"], json!("temporarily_unavailable"), "{wire:#}");
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn disk_mutation_between_plan_and_apply_refuses_source_unchanged() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("mutates-then-renames", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
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
    assert!(
        !on_disk.contains("flare"),
        "Rift must not write over the drifted bytes: {on_disk}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_refused_rename_carries_the_engine_words() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("refuses-rename", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "1nvalid")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    let detail = refusal_detail(&structured);
    assert!(
        detail.contains("declined the rename") && detail.contains("not an identifier"),
        "the refusal keeps the engine's words: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn engine_refused_prepare_carries_the_engine_words() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("refuses-prepare", "20s")),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    assert!(
        refusal_detail(&structured).contains("cannot rename here"),
        "the refusal keeps the engine's words: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn a_proposal_that_changes_nothing_refuses() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("renames-word", "20s")),
    )
    .await?;

    // Renaming `beacon` to `beacon` proposes edits that leave every byte
    // as it was, so the compiled plan holds no rewrites.
    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "beacon")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    assert!(
        refusal_detail(&structured).contains("proposed no edits"),
        "the refusal names the empty proposal: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn missing_symbol_refuses_before_the_engine_is_asked() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("renames-word", "20s")),
    )
    .await?;

    let structured = call_retrying_acceptance(
        &client,
        rename_request("rift://symbol/rust/lib.rs/vanished", "flare"),
    )
    .await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    assert_eq!(
        structured["preconditions"][0]["kind"],
        json!("target_exists"),
        "{structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn empty_new_name_is_a_typed_invalid_request() -> TestResult {
    let (_directory, client, server_task) = served_workspace(&[("lib.rs", LIBRARY)], None).await?;

    let error = client
        .call_tool(rename_request(BEACON_SYMBOL, ""))
        .await
        .expect_err("the schema-advertised length is enforced at acceptance");
    let wire = wire_error(error)?;
    assert_eq!(wire["code"], json!("invalid_request"), "{wire:#}");

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn prepare_timeout_is_a_typed_error_and_the_tree_is_untouched() -> TestResult {
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(engine_configuration("parks-on-prepare", "1s")),
    )
    .await?;

    let error = client
        .call_tool(rename_request(BEACON_SYMBOL, "flare"))
        .await
        .expect_err("a parked prepare must time out as a typed error");
    let wire = wire_error(error)?;
    assert_eq!(wire["code"], json!("temporarily_unavailable"), "{wire:#}");
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The attempt bound the absorption tests state for themselves, small
/// enough to count off the lifecycle log.
const ABSORPTION_ATTEMPTS: u64 = 3;

/// A refusal the engine invites again never reaches the caller: the server
/// resends the whole conversation and the rename lands.
#[tokio::test]
async fn a_cancelled_rename_is_resent_and_applies() -> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(counted(
            &engine_configuration("cancels-first-rename", "20s"),
            &log,
            ABSORPTION_ATTEMPTS,
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        recorded(&log, "rename"),
        2,
        "the cancelled rename was resent once: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn flare() {}\n"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A refusal that is the engine's verdict on the request surfaces at once,
/// with the tree untouched and the engine asked exactly once.
#[tokio::test]
async fn a_verdict_refusal_surfaces_without_a_resend() -> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(counted(
            &engine_configuration("refuses-rename", "20s"),
            &log,
            ABSORPTION_ATTEMPTS,
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "1nvalid")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(
        recorded(&log, "rename"),
        1,
        "a verdict is never sent a second time: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// An engine that answers while it is still analyzing is asked again, and
/// the settled answer is the one the caller sees.
#[tokio::test]
async fn a_rename_answered_mid_analysis_waits_for_the_settled_answer() -> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(counted(
            &engine_configuration("analyzes-then-serves", "20s"),
            &log,
            ABSORPTION_ATTEMPTS,
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        recorded(&log, "rename"),
        2,
        "the provisional answer was discarded and the rename asked again"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("main.rs"))?,
        "pub fn caller() { flare(); }\n",
        "the settled proposal spans the referencing file"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// An engine that never stops analyzing spends the whole attempt bound,
/// and only then does the plan surface a failure the caller can resend.
/// The tree is untouched: nothing was applied on a provisional answer.
#[tokio::test]
async fn a_rename_that_never_settles_surfaces_after_the_whole_budget() -> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY)],
        Some(counted(
            &engine_configuration("never-ends-progress", "20s"),
            &log,
            ABSORPTION_ATTEMPTS,
        )),
    )
    .await?;

    let error = client
        .call_tool(rename_request(BEACON_SYMBOL, "flare"))
        .await
        .expect_err("an engine that never settles must surface a typed error");
    let wire = wire_error(error)?;
    assert_eq!(wire["code"], json!("temporarily_unavailable"), "{wire:#}");
    assert_eq!(wire["retry"], json!("same_request"), "{wire:#}");
    assert_eq!(
        recorded(&log, "rename"),
        usize::try_from(ABSORPTION_ATTEMPTS)?,
        "the engine was asked for the whole budget before the caller heard"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        LIBRARY,
        "a plan that never settled writes nothing"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// An engine that dies mid-plan is replaced inside its restart budget and
/// the plan runs on the replacement, so the caller sees the rename.
#[tokio::test]
async fn an_engine_that_dies_mid_plan_is_replaced_before_the_caller_sees_it() -> TestResult {
    let logs = tempfile::tempdir()?;
    let log = logs.path().join("lifecycle.log");
    let (directory, client, server_task) = served_workspace(
        &[("lib.rs", LIBRARY), ("main.rs", CALLER)],
        Some(counted(
            &engine_configuration("dies-once-on-rename", "20s"),
            &log,
            ABSORPTION_ATTEMPTS,
        )),
    )
    .await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        recorded(&log, "initialize"),
        2,
        "the dead engine was replaced once: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub fn flare() {}\n"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
