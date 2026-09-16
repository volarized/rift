//! Live integration: the session against typescript-language-server.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_typescript` runs
//! the suite; without the variable every test skips visibly. The engine is
//! started from its fixture-local executable after a frozen install, so
//! package resolution cannot read a shared runner cache. Spawn policy,
//! framing, and encoding negotiation are checked against another engine
//! with cross-file references and a clean shutdown.

#![cfg(unix)]

mod engine_fixture;
mod live_engine_gate;
mod typescript_engine;

use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use rift_core::ProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{EngineLaunch, EngineSession};
use typescript_engine::{install_typescript_engine, typescript_package_files};

/// The live launch, built from the shared fixture's typescript-language-server
/// data - `tsserver.useSyntaxServer = "never"` keeps the engine to one
/// semantic server for cross-file references.
fn launch() -> EngineLaunch {
    typescript_engine::fixture().launch()
}

/// One bun project on disk with the pinned `typescript` installed.
fn bun_project() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("tempdir");
    for (name, source) in typescript_package_files() {
        std::fs::write(workspace.path().join(name), source).expect("fixture writes");
    }
    for (name, source) in [
        (
            "tsconfig.json",
            include_str!("fixtures/typescript/tsconfig.json"),
        ),
        ("hub.ts", include_str!("fixtures/typescript/hub.ts")),
        ("caller.ts", include_str!("fixtures/typescript/caller.ts")),
        ("view.tsx", include_str!("fixtures/typescript/view.tsx")),
    ] {
        std::fs::write(workspace.path().join(name), source).expect("source fixture writes");
    }
    install_typescript_engine(workspace.path());
    workspace
}

#[test]
fn fixture_runs_installed_language_server_directly() {
    let fixture = typescript_engine::fixture();
    assert_eq!(
        fixture.program,
        "node_modules/.bin/typescript-language-server"
    );
    assert_eq!(fixture.arguments, ["--stdio"]);
}

#[tokio::test]
async fn typescript_language_server_falls_back_to_utf16_and_advertises_the_pinned_capability_grid()
{
    if !engine_live() {
        return;
    }
    let workspace = bun_project();
    let started_at = Instant::now();
    let mut session = EngineSession::start(launch(), workspace.path())
        .await
        .expect("typescript-language-server starts and negotiates");
    eprintln!("initialize answered in {:?}", started_at.elapsed());
    let record = session.capabilities();
    assert_eq!(
        record.position_encoding,
        PositionEncoding::Utf16,
        "the session offers utf-8 first and this engine names no encoding, \
         so the protocol default stands"
    );
    assert!(
        !record.pull_diagnostics,
        "this engine publishes diagnostics instead of serving pulls: {record:#?}"
    );
    assert_eq!(record.diagnostic_identifier, None);
    assert!(record.references, "the engine advertises references");
    let document = ProjectPath::new("hub.ts").expect("fixture path");
    session
        .open(
            &document,
            "typescript",
            include_str!("fixtures/typescript/hub.ts").to_owned(),
        )
        .await
        .expect("didOpen is sent");
    let locations = session
        .references(&document, lsp_types::Position::new(0, 16))
        .await
        .expect("the function references resolve");
    for (path, line, character) in [("hub.ts", 0, 16), ("caller.ts", 4, 9), ("view.tsx", 3, 16)] {
        assert!(
            locations
                .iter()
                .any(|location| location.uri.path().as_str().ends_with(path)
                    && location.range.start == lsp_types::Position::new(line, character)),
            "the reference in {path} must resolve: {locations:?}"
        );
    }
    let stopped_at = Instant::now();
    let stderr = session.shutdown().await;
    let elapsed = stopped_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "typescript-language-server must exit on shutdown without waiting for the kill: {elapsed:?}"
    );
    eprintln!(
        "stderr: {} bytes, truncated {}",
        stderr.total_bytes, stderr.truncated
    );
}
