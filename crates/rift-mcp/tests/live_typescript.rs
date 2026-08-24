//! Live integration: the change tools over a bun project served by
//! typescript-language-server.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-mcp --test live_typescript` runs
//! the suite; without the variable every test skips visibly. Each test
//! serves one tempdir copy of `tests/fixtures/typescript`, installs the
//! pinned `typescript` the engine resolves, and drives the tools through a
//! live rmcp client. Every asserted shape was observed on a live
//! typescript-language-server answer first, then pinned.
//!
//! This engine holds the opposite capability arms from rust-analyzer: it
//! negotiates utf-16, it serves `workspace/willRenameFiles` over both
//! TypeScript dialects, and it has no pull diagnostics at all - so an
//! applied change carries the engine's silence where the rust suite pins a
//! mapped finding. rift-lsp's `live_typescript` suite pins the session
//! contract behind these tools.

#![cfg(unix)]

mod live_engine_gate;
mod typescript_engine;
mod workspace_client;

use std::fs;
use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use typescript_engine::{install_typescript_engine, typescript_engine_configuration};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

/// The manifest and lockfile `bun install --frozen-lockfile` reads.
const PACKAGE: &str = include_str!("fixtures/typescript/package.json");
const LOCKFILE: &str = include_str!("fixtures/typescript/bun.lock");
/// The compiler options that put both dialects in one project.
const COMPILER_OPTIONS: &str = include_str!("fixtures/typescript/tsconfig.json");
/// The global declarations: the JSX intrinsic elements the component
/// needs, and one interface merged into the standard library.
const AMBIENT: &str = include_str!("fixtures/typescript/ambient.ts");
/// The module declaring the exported function.
const HUB: &str = include_str!("fixtures/typescript/hub.ts");
/// The plain importer: it calls the function and holds the component.
const CALLER: &str = include_str!("fixtures/typescript/caller.ts");
/// The tsx importer: it calls the same function inside JSX.
const VIEW: &str = include_str!("fixtures/typescript/view.tsx");

/// The exported function, addressed through the plain TypeScript segment.
const BEACON_SYMBOL: &str = "rift://symbol/typescript/hub.ts/beacon";
/// The component, addressed through the tsx dialect segment: the engine
/// answers it only because `languages` claims `typescript:tsx` too. Its
/// name differs from its file stem, so renaming it leaves no `"./view"`
/// specifier carrying the old name behind.
const BANNER_SYMBOL: &str = "rift://symbol/typescript:tsx/view.tsx/Banner";
/// The interface that merges with the standard library's own, which
/// the engine refuses to rename.
const STANDARD_LIBRARY_SYMBOL: &str = "rift://symbol/typescript/ambient.ts/String";

/// The bun project, written into one tempdir per test.
fn project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("package.json", PACKAGE),
        ("bun.lock", LOCKFILE),
        ("tsconfig.json", COMPILER_OPTIONS),
        ("ambient.ts", AMBIENT),
        ("hub.ts", HUB),
        ("caller.ts", CALLER),
        ("view.tsx", VIEW),
    ]
}

/// One fixture tempdir with the pinned `typescript` installed beside the
/// sources, served to one client.
///
/// The install runs after the index is built, on purpose: the engine
/// spawns on the first tool request, and `[source] exclude` keeps the 132
/// installed files out of the index and out of every later scan.
async fn served_project() -> TestResult<(
    tempfile::TempDir,
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let (directory, client, server_task) =
        served_workspace(&project(), Some(typescript_engine_configuration())).await?;
    install_typescript_engine(directory.path());
    Ok((directory, client, server_task))
}

fn rename_request(symbol: &str, new_name: &str) -> CallToolRequestParams {
    tool_request(
        "rename_symbol",
        &json!({ "symbol": symbol, "new_name": new_name }),
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

/// Drives the rename tool until the engine has loaded the bun project.
///
/// The probe renames the declaration to the name it already has. Once the
/// engine resolves it, the proposal edits every occurrence to the bytes
/// already there, so the compiled plan holds no rewrite and the tool
/// refuses with `proposed no edits` - the readiness signal, with the tree
/// untouched either way.
///
/// One semantic server answers the rename only once its project is
/// loaded, so the first attempt normally carries the signal: locally every
/// run resolved the declaration inside that attempt, about 400ms in. The
/// loop bounds the wait a cold runner can add. It never proves how far the
/// proposal reaches - the assertions that follow do.
///
/// This engine reports no work-done progress at all, so the server cannot
/// wait it out the way it waits out rust-analyzer. What it does instead is
/// send an empty first answer's operation again once, which is enough for
/// the renames, the patch, and the refusal below: each ran cold ten times,
/// idle and saturated, without a failure. It is not enough for the move,
/// which keeps this probe.
async fn warmed_engine(client: &RunningService<RoleClient, ()>) -> TestResult {
    let started = Instant::now();
    for _attempt in 0..WARMUP_ATTEMPTS_MAX {
        let structured =
            call_retrying_acceptance(client, rename_request(BEACON_SYMBOL, "beacon")).await?;
        if refusal_detail(&structured).contains("proposed no edits") {
            eprintln!(
                "typescript-language-server resolved the declaration after {:?}",
                started.elapsed()
            );
            return Ok(());
        }
        tokio::time::sleep(WARMUP_PAUSE).await;
    }
    Err("typescript-language-server never resolved the declaration".into())
}

/// The engine resolves every reference itself, across both dialects: the
/// plain importer and the tsx component are rewritten from the same
/// proposal, without either file being opened, and the word-boundary sweep
/// finds no survivor to report.
#[tokio::test]
async fn applied_rename_rewrites_the_module_the_importer_and_the_component() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.ts", "hub.ts", "view.tsx"]),
        "the declaration and both importers carry the rename: {structured:#}"
    );
    assert_eq!(
        structured["summary"]["edits"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        3,
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
        fs::read_to_string(directory.path().join("hub.ts"))?,
        "export function flare(value: number): number {\n  return value;\n}\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.ts"))?,
        "import { flare } from \"./hub\";\nimport { Banner } from \"./view\";\n\n\
         export function total(): number {\n  return flare(2);\n}\n\n\
         export const heading = Banner;\n",
        "the plain importer's specifier binding and its call both follow the declaration"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("view.tsx"))?,
        "import { flare } from \"./hub\";\n\nexport function Banner() {\n  \
         return <span>{flare(3)}</span>;\n}\n",
        "the tsx component's call inside JSX follows the declaration too"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The component is declared in a `tsx` file, so its address carries the
/// `typescript:tsx` segment and the pool finds the engine only through the
/// dialect entry in `languages`: without that entry the same request
/// refuses `unsupported`, `no engine configured for language
/// typescript:tsx`. The proposal rewrites the declaration and the plain
/// module that imports it.
#[tokio::test]
async fn applied_rename_of_the_component_routes_the_tsx_dialect() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BANNER_SYMBOL, "Marker")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.ts", "view.tsx"]),
        "the tsx declaration and its plain-dialect importer: {structured:#}"
    );
    assert!(
        coded_findings(&structured, "rift.rename.survivor").is_empty(),
        "the component's name differs from its file stem, so no specifier survives: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("view.tsx"))?,
        "import { beacon } from \"./hub\";\n\nexport function Marker() {\n  \
         return <span>{beacon(3)}</span>;\n}\n"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.ts"))?,
        "import { beacon } from \"./hub\";\nimport { Marker } from \"./view\";\n\n\
         export function total(): number {\n  return beacon(2);\n}\n\n\
         export const heading = Marker;\n",
        "the importer's binding and its use both follow the component"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The will-rename answer rewrites the module specifier in every importer,
/// in both dialects: this engine's filters claim `ts` and `tsx` alike, and
/// a module here is a path, so the moved file needs no declaration
/// rewritten anywhere.
///
/// This is the one test here that still warms the engine. Run cold it
/// failed all ten runs, idle and saturated alike, and it failed honestly:
/// the server asked twice, the engine proposed nothing both times, and the
/// move applied carrying `rift.move.references_not_updated`. The resend
/// makes the gap visible; only a loaded engine closes it.
#[tokio::test]
async fn applied_move_rewrites_the_import_specifiers() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;
    warmed_engine(&client).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("move_file", &json!({ "from": "hub.ts", "to": "spoke.ts" })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert!(
        coded_findings(&structured, "rift.move.references_not_updated").is_empty(),
        "an engine covering the moved file carries no warning: {structured:#}"
    );
    assert_eq!(
        structured["summary"]["paths"],
        json!(["caller.ts", "hub.ts", "spoke.ts", "view.tsx"]),
        "the rewrites, the old path, and the new path all ride the summary: {structured:#}"
    );
    assert!(!directory.path().join("hub.ts").exists());
    assert_eq!(fs::read_to_string(directory.path().join("spoke.ts"))?, HUB);
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.ts"))?,
        "import { beacon } from \"./spoke\";\nimport { Banner } from \"./view\";\n\n\
         export function total(): number {\n  return beacon(2);\n}\n\n\
         export const heading = Banner;\n",
        "the plain importer's specifier follows the moved file"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("view.tsx"))?,
        "import { beacon } from \"./spoke\";\n\nexport function Banner() {\n  \
         return <span>{beacon(3)}</span>;\n}\n",
        "the tsx importer's specifier follows it as well"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A patch handing the function one argument too many.
const ARGUMENT_PATCH: &str = "--- a/caller.ts\n+++ b/caller.ts\n@@ -5 +5 @@\n\
                              -  return beacon(2);\n+  return beacon(2, 3);\n";

/// The applied change carries no engine finding, because this engine
/// advertises no pull: it publishes diagnostics instead, and Rift asks
/// only engines that serve `textDocument/diagnostic`.
///
/// The patched call is a real error - `tsc --noEmit` answers `caller.ts(5,20):
/// error TS2554: Expected 1 arguments, but got 2` on these exact bytes - so
/// the silence is the capability's, not the fixture's. It is the arm the
/// rust suite cannot reach, where a mapped `E0107` rides the same summary.
/// An absent capability stays silent: it raises no `rift.engine.failed`
/// warning either.
#[tokio::test]
async fn applied_patch_carries_no_engine_diagnostics() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("patch", &json!({ "patch": ARGUMENT_PATCH })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(structured["summary"]["paths"], json!(["caller.ts"]));
    assert_eq!(
        structured["summary"]["diagnostics"],
        json!([]),
        "an engine without pull diagnostics stays silent: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.ts"))?,
        "import { beacon } from \"./hub\";\nimport { Banner } from \"./view\";\n\n\
         export function total(): number {\n  return beacon(2, 3);\n}\n\n\
         export const heading = Banner;\n",
        "the change stays applied, unreported by the engine"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The refusal carries the engine's own words. This engine accepts every
/// spelling of a new name - `class`, `3bad`, and `has space` all came back
/// as proposals - so the arm it does refuse is the declaration, not the
/// name: `interface String` merges with the standard library's own
/// declaration, and the engine declines that at the prepare step.
#[tokio::test]
async fn engine_refused_rename_of_a_standard_library_declaration() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(STANDARD_LIBRARY_SYMBOL, "Text")).await?;
    assert_eq!(structured["status"], json!("refused"), "{structured:#}");
    assert_eq!(structured["reason"], json!("unmet_precondition"));
    let detail = refusal_detail(&structured);
    assert!(
        detail.contains("the engine declined the rename")
            && detail.contains(
                "You cannot rename elements that are defined in the standard TypeScript library."
            ),
        "the refusal keeps the engine's words: {structured:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("ambient.ts"))?,
        AMBIENT,
        "a refused rename leaves the tree as it was"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
