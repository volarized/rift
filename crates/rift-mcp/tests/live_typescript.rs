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

mod engine_fixture;
mod hermetic_search;
mod live_engine_gate;
mod typescript_engine;
mod workspace_client;

use std::fs;

use live_engine_gate::engine_live;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use typescript_engine::{install_typescript_engine, typescript_engine_configuration};
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, served_workspace, tool_request,
};

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

#[test]
fn fixture_runs_installed_language_server_directly() {
    let fixture = typescript_engine::fixture();
    assert_eq!(
        fixture.program,
        "node_modules/.bin/typescript-language-server"
    );
    assert_eq!(fixture.arguments, ["--stdio"]);
    assert_eq!(
        fixture.extra_toml,
        "\n[lsp.typescript.initialization_options.tsserver]\nuseSyntaxServer = \"never\"\n"
    );
}

#[test]
fn frozen_fixture_install_runs_in_two_isolated_roots() {
    if !engine_live() {
        return;
    }
    let first = tempfile::tempdir().expect("first fixture root");
    let second = tempfile::tempdir().expect("second fixture root");
    for root in [first.path(), second.path()] {
        fs::write(root.join("package.json"), PACKAGE).expect("package manifest writes");
        fs::write(root.join("bun.lock"), LOCKFILE).expect("lockfile writes");
    }

    std::thread::scope(|scope| {
        scope.spawn(|| install_typescript_engine(first.path()));
        scope.spawn(|| install_typescript_engine(second.path()));
    });

    for root in [first.path(), second.path()] {
        assert!(
            root.join("node_modules/.bin/typescript-language-server")
                .is_file(),
            "local language server must exist in {}",
            root.display()
        );
    }
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

/// The same project, served under a root spelled relative to the process
/// working directory - the spelling `rift mcp` and `rift server start`
/// hand the server.
///
/// This engine reaches the boundary from the other side: it answers in
/// UTF-16 positions and in `file://` URIs it normalizes itself, so a root
/// that is not resolved has one more way to go unmatched here than it does
/// against rust-analyzer.
async fn served_relative_project() -> TestResult<(
    tempfile::TempDir,
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let (directory, client, server_task) =
        served_relative_workspace(&project(), Some(typescript_engine_configuration())).await?;
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
/// The engine resolves every reference itself, across both dialects: the
/// plain importer and the tsx component are rewritten from the same
/// proposal, without either file being opened, and the word-boundary sweep
/// finds no survivor to report.
#[tokio::test]
async fn applied_rename_rewrites_the_module_the_importer_and_the_component() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_relative_project().await?;

    let structured =
        call_retrying_acceptance(&client, rename_request(BEACON_SYMBOL, "flare")).await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    assert_eq!(
        changed_paths(&structured),
        ["caller.ts", "hub.ts", "view.tsx"],
        "the declaration and both importers carry the rename: {structured:#}"
    );
    let renamed_hub = "export function flare(value: number): number {\n  return value;\n}\n";
    let renamed_caller = "import { flare } from \"./hub\";\nimport { Banner } from \"./view\";\n\n\
                          export function total(): number {\n  return flare(2);\n}\n\n\
                          export const heading = Banner;\n";
    let renamed_view = "import { flare } from \"./hub\";\n\nexport function Banner() {\n  \
                        return <span>{flare(3)}</span>;\n}\n";
    assert_eq!(
        structured["summary"]["files"],
        json!([
            {
                "path": "caller.ts",
                "kind": "modified",
                "size_bytes": renamed_caller.len(),
                "line_count": 8,
                "lines_added": 2,
                "lines_removed": 2
            },
            {
                "path": "hub.ts",
                "kind": "modified",
                "size_bytes": renamed_hub.len(),
                "line_count": 3,
                "lines_added": 1,
                "lines_removed": 1
            },
            {
                "path": "view.tsx",
                "kind": "modified",
                "size_bytes": renamed_view.len(),
                "line_count": 5,
                "lines_added": 2,
                "lines_removed": 2
            }
        ]),
        "the import and the call in one importer are two changed lines, the declaration one: \
         {structured:#}"
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
        renamed_hub
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("caller.ts"))?,
        renamed_caller,
        "the plain importer's specifier binding and its call both follow the declaration"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("view.tsx"))?,
        renamed_view,
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
        changed_paths(&structured),
        ["caller.ts", "view.tsx"],
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

/// The will-rename answer may rewrite module specifiers in both dialects
/// or validly propose no edits. The result matches either exact proposal:
/// rewritten importers without a warning, or a move-only change with one.
#[tokio::test]
async fn applied_move_matches_the_typescript_engine_proposal() -> TestResult {
    if !engine_live() {
        return Ok(());
    }
    let (directory, client, server_task) = served_project().await?;
    let structured = call_retrying_acceptance(
        &client,
        tool_request("move_file", &json!({ "from": "hub.ts", "to": "spoke.ts" })),
    )
    .await?;
    assert_eq!(structured["status"], json!("applied"), "{structured:#}");
    let warnings = coded_findings(&structured, "rift.move.references_not_updated");
    assert!(!directory.path().join("hub.ts").exists());
    assert_eq!(fs::read_to_string(directory.path().join("spoke.ts"))?, HUB);
    match warnings.as_slice() {
        [] => {
            assert_eq!(
                changed_paths(&structured),
                ["caller.ts", "hub.ts", "spoke.ts", "view.tsx"],
                "the proposal rewrites, old path, and new path ride the summary: {structured:#}"
            );
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
        }
        [warning] => {
            assert_eq!(warning["severity"], json!("warning"), "{structured:#}");
            assert!(
                warning["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("engine typescript")
                        && message.contains("references were not updated")),
                "the warning names the engine and skipped updates: {structured:#}"
            );
            assert_eq!(
                changed_paths(&structured),
                ["hub.ts", "spoke.ts"],
                "an empty proposal moves only the requested file: {structured:#}"
            );
            assert_eq!(
                fs::read_to_string(directory.path().join("caller.ts"))?,
                CALLER
            );
            assert_eq!(fs::read_to_string(directory.path().join("view.tsx"))?, VIEW);
        }
        _ => panic!("one move carries at most one reference warning: {structured:#}"),
    }

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
    assert_eq!(changed_paths(&structured), ["caller.ts"]);
    assert!(
        structured["summary"].get("diagnostics").is_none(),
        "an engine without pull diagnostics stays silent, so diagnostics is omitted: \
         {structured:#}"
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
