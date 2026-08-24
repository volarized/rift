//! Live integration: the session against rust-analyzer.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_rust_analyzer`
//! runs the suite; without the variable every test skips visibly. The
//! engine is spawned as `rust-analyzer`, resolved through the inherited
//! `PATH` where rustup's proxy answers it, so the spawn policy, the
//! framing, and the utf-8 negotiation are proven against a second real
//! engine beside the scripted one. Every asserted shape was observed on a
//! live rust-analyzer answer first, then pinned.
//!
//! The tool-level proof - rename, move, and diagnostics through the real
//! server - lives in rift-mcp's `live_rust_analyzer` suite. This suite
//! keeps the session contract pinned: the capability grid those tools
//! stand on, and a clean shutdown.

#![cfg(unix)]

mod live_engine_gate;
mod rust_engine;

use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use lsp_types::FileOperationPatternKind;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{EngineLaunch, EngineSession};
use rust_engine::{RUST_ANALYZER_PROGRAM, require_rust_analyzer, rust_analyzer_environment};

/// The cargo project fixture: a manifest, a crate root, a module, and the
/// module's cross-file reference.
const MANIFEST: &str = include_str!("fixtures/rust/Cargo.toml");
const CRATE_ROOT: &str = include_str!("fixtures/rust/lib.rs");
const HUB: &str = include_str!("fixtures/rust/hub.rs");
const CALLER: &str = include_str!("fixtures/rust/caller.rs");

/// The live launch. rust-analyzer answers initialize in milliseconds and
/// loads the cargo project afterwards, so a later request can wait on a
/// cold sysroot walk: the bounds are generous, and still bounds.
fn launch() -> EngineLaunch {
    EngineLaunch {
        program: RUST_ANALYZER_PROGRAM.to_owned(),
        arguments: Vec::new(),
        environment: rust_analyzer_environment(),
        initialization_options: None,
        startup_timeout: Duration::from_mins(2),
        request_timeout: Duration::from_mins(2),
        stderr_capture_bytes: 65_536,
    }
}

/// One cargo project on disk, outside the repository's toolchain pin.
fn cargo_project() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("tempdir");
    for (name, source) in [
        ("Cargo.toml", MANIFEST),
        ("lib.rs", CRATE_ROOT),
        ("hub.rs", HUB),
        ("caller.rs", CALLER),
    ] {
        std::fs::write(workspace.path().join(name), source).expect("fixture writes");
    }
    workspace
}

#[tokio::test]
async fn rust_analyzer_negotiates_utf8_and_advertises_the_pinned_capability_grid() {
    if !engine_live() {
        return;
    }
    let workspace = cargo_project();
    require_rust_analyzer(workspace.path());
    let started_at = Instant::now();
    let session = EngineSession::start(launch(), workspace.path())
        .await
        .expect("rust-analyzer starts and negotiates");
    eprintln!("initialize answered in {:?}", started_at.elapsed());
    let record = session.capabilities();
    assert_eq!(
        record.position_encoding,
        PositionEncoding::Utf8,
        "rust-analyzer accepts the preferred utf-8 offer"
    );
    assert!(
        record.rename && record.prepare_rename,
        "the rename tool stands on the prepared rename: {record:#?}"
    );
    assert!(
        record.pull_diagnostics,
        "the diagnostics walk stands on the pull: {record:#?}"
    );
    assert_eq!(
        record.diagnostic_identifier.as_deref(),
        Some("rust-analyzer")
    );
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
            ("file", "**/*.rs", Some(&FileOperationPatternKind::File)),
            ("file", "**", Some(&FileOperationPatternKind::Folder)),
        ],
        "rust-analyzer filters file renames to rust sources: {record:#?}"
    );
    assert!(
        record.will_rename_matches("hub.rs"),
        "the advertised filters cover a module file at the tree root: {record:#?}"
    );
    let stopped_at = Instant::now();
    let stderr = session.shutdown().await;
    let elapsed = stopped_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "rust-analyzer must exit on shutdown without waiting for the kill: {elapsed:?}"
    );
    eprintln!(
        "stderr: {} bytes, truncated {}",
        stderr.total_bytes, stderr.truncated
    );
}
