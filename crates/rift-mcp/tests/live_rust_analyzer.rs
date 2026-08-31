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
//! Every asserted operation runs cold. Engine retry and progress handling
//! absorb startup; fixtures make no preparatory mutation.
//!
//! The fixture is a cargo project, because rust-analyzer resolves nothing
//! outside one: a manifest whose `[lib]` path keeps every module file at
//! the tree root, and whose empty `[workspace]` table stops cargo climbing
//! out of the tempdir. It mirrors rift-lsp's `fixtures/rust` project,
//! where the same engine's session contract is pinned.

#![cfg(unix)]

mod engine_fixture;
mod hermetic_search;
mod live_engine_gate;
mod rust_engine;
mod workspace_client;

use std::fs;
use std::process::Command;

use live_engine_gate::engine_live;
use rmcp::model::CallToolRequestParams;
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

const PROC_MACRO_WORKSPACE: &str =
    "[workspace]\nmembers = [\"app\", \"derive_identity\"]\nresolver = \"2\"\n";
const PROC_MACRO_MANIFEST: &str = "[package]\nname = \"derive_identity\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[lib]\nproc-macro = true\n";
const PROC_MACRO_SOURCE: &str = "extern crate proc_macro;\n\nuse proc_macro::TokenStream;\n\n#[proc_macro_derive(Identity)]\npub fn identity(_input: TokenStream) -> TokenStream {\n    TokenStream::new()\n}\n";
const PROC_MACRO_APP_MANIFEST: &str = "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nderive_identity = { path = \"../derive_identity\" }\n";
const PROC_MACRO_APP_SOURCE: &str = "use derive_identity::Identity;\n\n#[derive(Identity)]\npub struct Beacon;\n\npub fn value() -> u8 {\n    1\n}\n";
const PROC_MACRO_APP_PATCH: &str =
    "--- a/app/src/lib.rs\n+++ b/app/src/lib.rs\n@@ -7 +7 @@\n-    1\n+    2\n";

fn project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", MANIFEST),
        ("lib.rs", CRATE_ROOT),
        ("hub.rs", HUB),
        ("caller.rs", CALLER),
    ]
}

fn proc_macro_project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", PROC_MACRO_WORKSPACE),
        ("derive_identity/Cargo.toml", PROC_MACRO_MANIFEST),
        ("derive_identity/src/lib.rs", PROC_MACRO_SOURCE),
        ("app/Cargo.toml", PROC_MACRO_APP_MANIFEST),
        ("app/src/lib.rs", PROC_MACRO_APP_SOURCE),
    ]
}

fn proc_macro_configuration() -> String {
    format!(
        "{}\n[[hooks]]\nid = \"check\"\nkind = \"build\"\ncommand = [\"cargo\", \"check\", \"--workspace\"]\nchanged_paths = \"none\"\nwrites = \"none\"\nworking_directory = \"\"\nenvironment = {{}}\ntimeout = \"2m\"\noutput_limit = \"4kb\"\nfailure_severity = \"error\"\nguarantees = [{{ kind = \"syntax_validated\", scope = {{ kind = \"reach\", reach = \"project\" }}, detail = \"cargo check passes\" }}]\ndeterminism = \"deterministic\"\n",
        rust_engine_configuration()
    )
}

fn require_command_success(root: &std::path::Path, arguments: &[&str]) -> TestResult {
    let output = Command::new("cargo")
        .args(arguments)
        .current_dir(root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "cargo {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    )
    .into())
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

/// The project paths one applied change reports, in the order it carries them.
fn changed_paths(structured: &Value) -> Vec<&str> {
    structured["summary"]["files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(|file| file["path"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// The engine resolves every reference itself, so the rewrite covers both
/// files and the word-boundary sweep finds no survivor to report. Each
/// rewritten file reports the lines the engine's own edits changed.
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
        changed_paths(&structured),
        ["caller.rs", "hub.rs"],
        "the declaration and its cross-file reference both carry the rename: {structured:#}"
    );
    let files = structured["summary"]["files"]
        .as_array()
        .ok_or("an applied rename must carry its files")?;
    assert_eq!(files[0]["kind"], json!("modified"));
    assert_eq!(
        files[0]["lines_added"]
            .as_u64()
            .zip(files[0]["lines_removed"].as_u64()),
        Some((2, 2)),
        "the caller carries the import and the call, one line each: {structured:#}"
    );
    assert_eq!(
        files[1]["lines_added"]
            .as_u64()
            .zip(files[1]["lines_removed"].as_u64()),
        Some((1, 1)),
        "the declaring file carries the declaration line alone: {structured:#}"
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

/// The moved file is a module file. When rust-analyzer proposes reference
/// edits, the `mod` declaration and `use` path follow the new stem. A valid
/// empty proposal moves only the file and carries the reference warning.
///
/// The engine's own findings are not pinned here. rust-analyzer learns of
/// the new file through its file watcher, which the post-apply pull can
/// outrun; the observed run carried `E0583 unresolved module` for the
/// destination that already existed on disk.
///
#[tokio::test]
async fn applied_move_matches_the_rust_analyzer_proposal() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(
        &client,
        tool_request("move_file", &json!({ "from": "hub.rs", "to": "spoke.rs" })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = coded_findings(&structured, "rift.move.references_not_updated");
    assert!(!directory.path().join("hub.rs").exists());
    assert_eq!(fs::read_to_string(directory.path().join("spoke.rs"))?, HUB);
    match warnings.as_slice() {
        [] => {
            assert_eq!(
                changed_paths(&structured),
                ["caller.rs", "hub.rs", "lib.rs", "spoke.rs"],
                "the proposal rewrites, old path, and new path ride the summary: {structured:#}"
            );
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
        }
        [warning] => {
            assert_eq!(warning["severity"], json!("warning"), "{structured:#}");
            assert!(
                warning["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("engine rust")
                        && message.contains("references were not updated")),
                "the warning names the engine and skipped updates: {structured:#}"
            );
            assert_eq!(
                changed_paths(&structured),
                ["hub.rs", "spoke.rs"],
                "an empty proposal moves only the requested file: {structured:#}"
            );
            assert_eq!(
                fs::read_to_string(directory.path().join("lib.rs"))?,
                CRATE_ROOT
            );
            assert_eq!(
                fs::read_to_string(directory.path().join("caller.rs"))?,
                CALLER
            );
        }
        _ => panic!("one move carries at most one reference warning: {structured:#}"),
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A patch replacing one file with a malformed declaration.
const SYNTAX_PATCH: &str = "--- a/caller.rs\n+++ b/caller.rs\n@@ -1,5 +1 @@\n-use crate::hub::beacon;\n-\n-pub fn total() -> i32 {\n-    beacon(2)\n-}\n+fn broken( {\n";

/// Adds one module declaration and its file in the same change.
const ADD_MODULE_PATCH: &str = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,2 +1,3 @@\n pub mod caller;\n+pub mod fresh;\n pub mod hub;\n--- /dev/null\n+++ b/fresh.rs\n@@ -0,0 +1 @@\n+pub fn ready() {}\n";

/// The applied change carries Rift's provider finding for the file it
/// changed. Settled engine findings may join it, but engine scheduling
/// cannot erase the synchronous provider result.
///
#[tokio::test]
async fn applied_patch_carries_the_provider_diagnostic() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": SYNTAX_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(changed_paths(&structured), ["caller.rs"]);
    let findings = coded_findings(&structured, "rift.syntax.error");
    let finding = findings.first().unwrap_or_else(|| {
        panic!("provider syntax finding must ride applied change: {structured:#}")
    });
    assert_eq!(finding["severity"], json!("error"));
    assert_eq!(finding["reliability"], json!("recovered"));
    assert_eq!(
        finding["language"],
        json!("rust"),
        "the provider language stamps the finding: {finding:#}"
    );
    assert_eq!(finding["span"]["unit"], json!("rift://file/caller.rs"));
    assert!(
        coded_findings(&structured, "rift.engine.failed").is_empty(),
        "an addressed engine never degrades to a failure warning: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.rs"))?,
        "fn broken( {\n",
        "the change stays applied with its finding attached"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// One cold change notifies the parent edit and new module before either
/// diagnostic pull. rust-analyzer therefore accepts the valid new module
/// instead of reporting it missing from stale parent state.
#[tokio::test]
async fn cold_parent_and_new_module_arrive_as_one_engine_batch() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": ADD_MODULE_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(changed_paths(&structured), ["fresh.rs", "lib.rs"]);
    assert!(
        coded_findings(&structured, "E0583").is_empty(),
        "the parent must observe the new module in the same batch: {structured:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Validation rebuilds proc-macro artifacts removed after engine startup.
/// Later engine diagnostics may be settled or explicitly unready, but
/// cannot retain a missing artifact from the removed target directory.
#[tokio::test]
async fn cargo_clean_proc_macro_artifact_is_not_retained_after_validation() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&proc_macro_project(), Some(proc_macro_configuration())).await?;
    require_rust_analyzer(directory.path());
    require_command_success(directory.path(), &["check", "--workspace"])?;

    let started = call_retrying_acceptance(
        &client,
        tool_request(
            "rename_symbol",
            &json!({
                "symbol": "rift://symbol/rust/app/src/lib.rs/value",
                "new_name": "value"
            }),
        ),
    )
    .await?;
    assert_eq!(started["status"], json!("refused"), "{started:#}");
    require_command_success(directory.path(), &["clean"])?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": PROC_MACRO_APP_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["guarantees"][0]["detail"],
        json!("cargo check passes"),
        "validation must rebuild the removed artifact: {structured:#}"
    );
    let stale_artifact =
        structured["summary"]["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| {
                diagnostics.iter().any(|diagnostic| {
                    let message = diagnostic["message"].as_str().unwrap_or_default();
                    message.contains("proc-macro") && message.contains("target")
                })
            });
    assert!(
        !stale_artifact,
        "engine retained removed proc-macro artifact: {structured:#}"
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

fn remove_request(force: bool) -> CallToolRequestParams {
    tool_request(
        "remove_symbol",
        &json!({ "symbol": BEACON_SYMBOL, "force": force }),
    )
}

/// `caller.rs` calls `hub::beacon` once, so the engine's own reference check finds it and
/// refuses the removal, naming the caller's path in the failed `no_references` condition.
#[tokio::test]
async fn remove_symbol_with_a_standing_reference_refuses_and_names_the_caller() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(&client, remove_request(false)).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    let preconditions = structured["preconditions"]
        .as_array()
        .expect("a failed condition rides the refusal");
    assert_eq!(preconditions[0]["kind"], json!("no_references"));
    assert_eq!(
        preconditions[0]["expected"],
        json!({ "kind": "count", "value": 0 })
    );
    let paths = preconditions[0]["paths"]
        .as_array()
        .expect("the failed condition names the reference paths");
    assert!(
        paths.contains(&json!("caller.rs")),
        "the refusal must name the caller's path: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("hub.rs"))?,
        HUB,
        "a refused removal leaves the tree untouched"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The same removal, forced: it applies over the standing reference and carries it as a
/// warning instead of refusing.
#[tokio::test]
async fn forced_remove_symbol_applies_and_carries_the_reference_warning() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project(), Some(rust_engine_configuration())).await?;
    require_rust_analyzer(directory.path());

    let structured = call_retrying_acceptance(&client, remove_request(true)).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.reference");
    assert_eq!(
        findings.len(),
        1,
        "the standing reference rides as a warning instead of refusing: {structured:#}"
    );
    assert!(
        findings[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("caller.rs"),
        "the warning must name the caller's path: {findings:#?}"
    );
    let written = fs::read_to_string(directory.path().join("hub.rs"))?;
    assert!(
        !written.contains("fn beacon"),
        "the declaration must be removed: {written}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A JSON member's declaration, spelled `"port": 8080` with nothing blank around it, has no
/// separator to widen over: the removal takes exactly the addressed span. No JSON language
/// LSP binding exists in this fixture, so the removal applies unchecked, and the warning
/// names why.
const SETTINGS_JSON: &str = "{\"server\": {\"port\": 8080}}\n";

/// The cargo project fixture, plus a JSON file no configured engine serves.
fn project_with_settings() -> Vec<(&'static str, &'static str)> {
    let mut files = project();
    files.push(("settings.json", SETTINGS_JSON));
    files
}

#[tokio::test]
async fn remove_symbol_in_an_unengined_language_applies_unchecked() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) =
        served_workspace(&project_with_settings(), Some(rust_engine_configuration())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "remove_symbol",
            &json!({
                "symbol": "rift://symbol/json/settings.json/server%20%3E%20port",
                "force": false
            }),
        ),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let findings = coded_findings(&structured, "rift.remove.unchecked");
    assert_eq!(
        findings.len(),
        1,
        "the removal must say it was not checked: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("settings.json"))?,
        "{\"server\": {}}\n",
        "the mid-line member is removed exactly, with no separator to widen over"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[test]
fn rust_engine_fixture_pins_1_98() {
    let fixture = rust_engine::fixture();
    assert_eq!(fixture.program, "rustup");
    assert_eq!(fixture.arguments, ["run", "1.98", "rust-analyzer"]);
    assert_eq!(fixture.extra_toml, "\n[lsp.rust.retry]\nattempts = 16\n");
}
