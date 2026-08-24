//! Live integration: the session against typescript-language-server.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_typescript` runs
//! the suite; without the variable every test skips visibly. The engine is
//! started as `bunx`, resolved through the inherited `PATH`, so the spawn
//! policy, the framing, and the encoding negotiation are proven against a
//! second real engine beside rust-analyzer - and against the opposite arm
//! of every capability gate. Every asserted shape was observed on a live
//! typescript-language-server answer first, then pinned.
//!
//! The tool-level proof - rename across both dialects, the move's import
//! rewrites, and the engine's silence on an applied change - lives in
//! rift-mcp's `live_typescript` suite. This suite keeps the session
//! contract pinned: the capability grid those tools stand on, and a clean
//! shutdown.

#![cfg(unix)]

mod live_engine_gate;
mod typescript_engine;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use lsp_types::FileOperationPatternKind;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{EngineLaunch, EngineSession};
use typescript_engine::{
    BUNX_PROGRAM, LANGUAGE_SERVER_PACKAGE, install_typescript_engine, typescript_package_files,
};

/// The live launch. The engine answers initialize in tens of milliseconds
/// once `bunx` has resolved the package, so the bounds carry a cold
/// resolution - and are still bounds.
///
/// `tsserver.useSyntaxServer = "never"` keeps the engine to one semantic
/// server, the launch rift-mcp's tool suite pins its answers against.
fn launch() -> EngineLaunch {
    EngineLaunch {
        program: BUNX_PROGRAM.to_owned(),
        arguments: vec![LANGUAGE_SERVER_PACKAGE.to_owned(), "--stdio".to_owned()],
        environment: BTreeMap::new(),
        initialization_options: Some(serde_json::json!({
            "tsserver": { "useSyntaxServer": "never" }
        })),
        startup_timeout: Duration::from_mins(2),
        request_timeout: Duration::from_mins(2),
        stderr_capture_bytes: 65_536,
    }
}

/// One bun project on disk with the pinned `typescript` installed.
fn bun_project() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("tempdir");
    for (name, source) in typescript_package_files() {
        std::fs::write(workspace.path().join(name), source).expect("fixture writes");
    }
    install_typescript_engine(workspace.path());
    workspace
}

#[tokio::test]
async fn typescript_language_server_falls_back_to_utf16_and_advertises_the_pinned_capability_grid()
{
    if !engine_live() {
        return;
    }
    let workspace = bun_project();
    let started_at = Instant::now();
    let session = EngineSession::start(launch(), workspace.path())
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
        record.rename && record.prepare_rename,
        "the rename tool stands on the prepared rename: {record:#?}"
    );
    assert!(
        !record.pull_diagnostics,
        "this engine publishes diagnostics instead of serving pulls: {record:#?}"
    );
    assert_eq!(record.diagnostic_identifier, None);
    assert!(
        record.will_rename_files(),
        "the move tool stands on workspace/willRenameFiles: {record:#?}"
    );
    let filters: Vec<(&str, &str, Option<&FileOperationPatternKind>)> = record
        .will_rename_filters
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|filter| {
            (
                filter.scheme.as_deref().unwrap_or_default(),
                filter.pattern.glob.as_str(),
                filter.pattern.matches.as_ref(),
            )
        })
        .collect();
    assert_eq!(
        filters,
        [
            (
                "file",
                "**/*.{ts,js,jsx,tsx,mjs,mts,cjs,cts}",
                Some(&FileOperationPatternKind::File)
            ),
            ("file", "**", Some(&FileOperationPatternKind::Folder)),
        ],
        "one alternation covers every ECMAScript extension the engine serves: {record:#?}"
    );
    assert!(
        record.will_rename_matches("hub.ts") && record.will_rename_matches("view.tsx"),
        "the advertised filters cover both dialects at the tree root: {record:#?}"
    );
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
