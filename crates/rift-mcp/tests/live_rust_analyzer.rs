//! Live integration: the change tools over a cargo project served by
//! rust-analyzer.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_rust_analyzer`
//! runs the suite; without the variable every test skips visibly. Each
//! test serves one tempdir workspace whose own `rift.toml` claims rust
//! through the real engine, and drives the tools through a live rmcp
//! client: the rename proposal, the will-rename import updates, the pulled
//! diagnostics, and the engine's own refusal all cross the wire the
//! scripted suites only imitate. Every asserted shape was observed on a
//! live rust-analyzer answer first, then pinned.
//!
//! Each test makes one call and asserts what a caller sees. The server
//! waits out an engine that is loading: while the engine has announced
//! work it discards what came back, and before the first announcement it
//! sends an empty answer's operation again once, so the request that races
//! the announcement is not the one the caller is answered from.
//!
//! The rename and the refusal below run cold. The move and the patch warm
//! the engine first, and both for the same measured reason: rust-analyzer
//! announces its project load, ends the announcement, and still answers a
//! will-rename with no edit and a pull with no items for a file it has not
//! finished reading. An engine that says its work is done and then answers
//! nothing is the one thing the readiness gate cannot read, so the warm-up
//! is what puts those two past it.
//!
//! The fixture is a cargo project, because rust-analyzer resolves nothing
//! outside one: a manifest whose `[lib]` path keeps every module file at
//! the tree root, and whose empty `[workspace]` table stops cargo climbing
//! out of the tempdir. It mirrors rift-lsp's `fixtures/rust` project,
//! where the same engine's session contract is pinned.

#![cfg(unix)]

mod hermetic_search;
mod live_engine_gate;
mod rust_engine;
mod workspace_client;

use std::fs;
use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rust_engine::{require_rust_analyzer, rust_engine_configuration};
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

/// The cargo project: a manifest, a crate root, the `hub` module holding
/// the declaration, and the `caller` module importing and calling it. The
/// module name differs from the declaration name, so a rename of the
/// declaration leaves no occurrence of the old name behind.
const MANIFEST: &str = "[package]\nname = \"rift_live_fixture\"\nversion = \"0.0.0\"\n\
                        edition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n\
                        [workspace]\n";
const CRATE_ROOT: &str = "pub mod caller;\npub mod hub;\n";
const HUB: &str = "pub fn beacon(value: i32) -> i32 {\n    value\n}\n";
const CALLER: &str = "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n";
const BEACON_SYMBOL: &str = "rift://symbol/rust/hub.rs/beacon";

fn project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", MANIFEST),
        ("lib.rs", CRATE_ROOT),
        ("hub.rs", HUB),
        ("caller.rs", CALLER),
    ]
}

fn rename_request(new_name: &str) -> CallToolRequestParams {
    tool_request(
        "rename_symbol",
        &json!({ "symbol": BEACON_SYMBOL, "new_name": new_name }),
    )
}

fn refusal_detail(structured: &Value) -> String {
    structured["diagnostics"][0]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

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

/// Most warm-up attempts one test makes, and the pause between them: at
/// most a minute of waiting, then the test fails instead of hanging.
const WARMUP_ATTEMPTS_MAX: usize = 240;
const WARMUP_PAUSE: Duration = Duration::from_millis(250);

/// Drives the rename tool until rust-analyzer has loaded the cargo project.
///
/// The probe renames the declaration to the name it already has. Once
/// rust-analyzer resolves it, the proposal edits every occurrence to the
/// bytes already there, so the compiled plan holds no rewrite and the tool
/// refuses with `proposed no edits` - the readiness signal, with the tree
/// untouched either way.
///
/// The server absorbs a loading engine on its own while the engine
/// announces its work: locally the announcement runs for the first 830ms
/// and the engine cancels requests until about 2.3s, and every attempt
/// inside that window is resent under the `retry` table. It also sends one
/// empty answer's operation again before the first announcement arrives,
/// which is what lets the rename and the refusal below run cold. What no
/// signal covers is an answer with nothing in it after the announced load
/// has ended: nothing separates that from a clean file or from a move with
/// no reference to update. This probe puts the engine past it. It never
/// proves how far a proposal reaches; the assertions do.
async fn warmed_engine(client: &RunningService<RoleClient, ()>) -> TestResult {
    let started = Instant::now();
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let structured = call_retrying_acceptance(client, rename_request("beacon")).await?;
        if refusal_detail(&structured).contains("proposed no edits") {
            eprintln!(
                "rust-analyzer resolved the declaration after {:?}",
                started.elapsed()
            );
            return Ok(());
        }
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    Err("rust-analyzer never resolved the declaration".into())
}

/// The engine resolves every reference itself, so the rewrite covers both
/// files and the word-boundary sweep finds no survivor to report.
///
/// The workspace is served under a root spelled relative to the process
/// working directory, the spelling `rift mcp` and `rift server start` hand
/// the server. A real engine is the strictest witness that the spelling
/// reaches it resolved: rust-analyzer answers in `file://` URIs of its own
/// making, and every one of them must fall under the root Rift named.
#[tokio::test]
async fn applied_rename_rewrites_the_module_and_its_caller() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_relative_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(&client, rename_request("flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.rs", "hub.rs"]),
        "the declaration and its cross-file reference both carry the rename: {structured:#}"
    );
    assert_eq!(
        structured["summary"]["edits"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        2,
        "one whole-file edit per rewritten file: {structured:#}"
    );
    assert!(
        coded_findings(&structured, "rift.rename.survivor").is_empty(),
        "an engine-resolved rename sweeps clean: {structured:#}"
    );
    assert!(
        coded_findings(&structured, "rift.engine.failed").is_empty(),
        "the engine served every request: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("hub.rs"))?,
        "pub fn flare(value: i32) -> i32 {\n    value\n}\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.rs"))?,
        "use crate::hub::flare;\n\npub fn total() -> i32 {\n    flare(2)\n}\n",
        "rust-analyzer rewrote the import and the call it resolved from its own index"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The moved file is a module file, so rust-analyzer's will-rename answer
/// renames the module: the `mod` declaration in the crate root and the
/// `use` path in the sibling both follow the new stem, and no
/// references-not-updated warning rides the summary.
///
/// The engine's own findings are not pinned here. rust-analyzer learns of
/// the new file through its file watcher, which the post-apply pull can
/// outrun; the observed run carried `E0583 unresolved module` for the
/// destination that already existed on disk.
///
/// This test warms the engine for the same reason the patch test does. Run
/// cold it passed idle, saturated, and instrumented, and then failed once
/// in nine saturated runs with `paths` holding only the move and no
/// warning beside it: rust-analyzer had announced its project load and
/// ended it, and still proposed no reference update. An engine that says
/// its work is done and then answers nothing is the one thing the
/// readiness gate cannot read.
#[tokio::test]
async fn applied_move_rewrites_the_module_declaration_and_the_import() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());
    warmed_engine(&client).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("move_file", &json!({ "from": "hub.rs", "to": "spoke.rs" })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        coded_findings(&structured, "rift.move.references_not_updated").is_empty(),
        "an engine covering the moved file carries no warning: {structured:#}"
    );
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.rs", "hub.rs", "lib.rs", "spoke.rs"]),
        "the rewrites, the old path, and the new path all ride the summary: {structured:#}"
    );
    assert!(!directory.path().join("hub.rs").exists());
    assert_eq!(fs::read_to_string(directory.path().join("spoke.rs"))?, HUB);
    assert_eq!(
        fs::read_to_string(directory.path().join("lib.rs"))?,
        "pub mod caller;\npub mod spoke;\n",
        "the module declaration follows the new file stem"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.rs"))?,
        "use crate::spoke::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n",
        "the sibling's import path follows the renamed module"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A patch handing the function one argument too many.
const ARGUMENT_PATCH: &str =
    "--- a/caller.rs\n+++ b/caller.rs\n@@ -4 +4 @@\n-    beacon(2)\n+    beacon(2, 3)\n";

/// The inverse of [`ARGUMENT_PATCH`], restoring the single argument.
const ARGUMENT_REVERT_PATCH: &str =
    "--- a/caller.rs\n+++ b/caller.rs\n@@ -4 +4 @@\n-    beacon(2, 3)\n+    beacon(2)\n";

/// Lands the arity error until rust-analyzer reports it, answering with the
/// applied change that carried the finding.
async fn reported_arity_error(client: &RunningService<RoleClient, ()>) -> TestResult<Value> {
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let landed = tool_request("patch", &json!({ "patch": ARGUMENT_PATCH }));
        let structured = call_retrying_acceptance(client, landed).await?;
        if !coded_findings(&structured, "E0107").is_empty() {
            return Ok(structured);
        }
        let reverted = tool_request("patch", &json!({ "patch": ARGUMENT_REVERT_PATCH }));
        call_retrying_acceptance(client, reverted).await?;
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    Err("rust-analyzer reported no arity error within the warm-up bound".into())
}

/// The applied change carries rust-analyzer's own finding for the file it
/// changed: the pull runs on the document Rift just wrote, so the answer
/// is the engine's reading of the landed bytes.
///
/// The server absorbs every wait it has evidence for: a pull answered
/// while the engine reports outstanding `$/progress` is provisional and
/// sent again. This finding is beyond that evidence. rust-analyzer derives
/// it from a cargo check it runs on its own schedule and never announces
/// as progress, so on a loaded runner it reports progress begun, progress
/// ended, and an empty pull for a file it has not checked yet - settled
/// and clean, as far as any observer can tell. Waiting on that would mean
/// waiting on every clean file for a signal that never comes.
///
/// So the scenario repeats instead: the error lands, and an empty report
/// reverts it and lands it again, each attempt a real change with a real
/// pull, bounded by the warm-up budget.
#[tokio::test]
async fn applied_patch_carries_the_engine_diagnostic() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());
    warmed_engine(&client).await?;

    let structured = reported_arity_error(&client).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(structured["summary"]["paths"], json!(["caller.rs"]));
    let findings = coded_findings(&structured, "E0107");
    assert_eq!(
        findings.len(),
        1,
        "the arity error rides the applied change: {structured:#}"
    );
    let finding = findings[0];
    assert_eq!(finding["severity"], json!("error"));
    assert_eq!(
        finding["language"],
        json!({ "name": "rust" }),
        "the engine's language stamps the finding: {finding:#}"
    );
    assert_eq!(finding["message"], json!("expected 1 argument, found 2"));
    assert_eq!(finding["span"]["unit"], json!("rift://file/caller.rs"));
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.rs"))?,
        "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2, 3)\n}\n",
        "the change stays applied with its finding attached"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The refusal carries rust-analyzer's own words. `crate` is a keyword the
/// engine refuses outright; `fn` is not - rust-analyzer proposes the raw
/// identifier `r#fn` and the rename applies.
#[tokio::test]
async fn engine_refused_new_name_carries_the_engine_words() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(&client, rename_request("crate")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    let detail = refusal_detail(&structured);
    assert!(
        detail.contains("the engine declined the rename")
            && detail.contains("cannot rename to a keyword"),
        "the refusal keeps the engine's words: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("hub.rs"))?,
        HUB,
        "a refused rename leaves the tree as it was"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
