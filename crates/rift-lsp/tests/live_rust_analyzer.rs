//! Live integration: the session against rust-analyzer.
//!
//! `RIFT_ENGINE_LIVE=1 cargo test -p rift-lsp --test live_rust_analyzer`
//! runs the suite; without the variable every test skips visibly. The
//! engine is spawned through `rustup run 1.98 rust-analyzer`, so the spawn
//! policy, framing, and utf-8 negotiation are proven against a second real
//! engine beside the scripted one. Every asserted shape was observed on a
//! live rust-analyzer answer first, then pinned.
//!
//! The suite checks capabilities, project-load progress, cross-file references,
//! call hierarchy, and clean shutdown.

#![cfg(unix)]

mod engine_fixture;
mod live_engine_gate;
mod rust_engine;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use live_engine_gate::engine_live;
use rift_core::ProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{EngineLaunch, EngineReadiness, EngineSession};
use rust_engine::require_rust_analyzer;

/// The cargo project fixture: a manifest, a crate root, a module, the
/// module's cross-file reference, and a function whose callees reach into
/// the project and `std` beside a struct.
const MANIFEST: &str = include_str!("fixtures/rust/Cargo.toml");
const CRATE_ROOT: &str = include_str!("fixtures/rust/lib.rs");
const HUB: &str = include_str!("fixtures/rust/hub.rs");
const CALLER: &str = include_str!("fixtures/rust/caller.rs");
const WALK: &str = include_str!("fixtures/rust/walk.rs");

/// The live launch, built from the shared fixture's rust-analyzer data.
fn launch() -> EngineLaunch {
    rust_engine::fixture().launch()
}

/// One cargo project on disk, outside the repository's toolchain pin.
fn cargo_project() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().expect("tempdir");
    for (name, source) in [
        ("Cargo.toml", MANIFEST),
        ("lib.rs", CRATE_ROOT),
        ("hub.rs", HUB),
        ("caller.rs", CALLER),
        ("walk.rs", WALK),
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
        record.pull_diagnostics,
        "the diagnostics walk stands on the pull: {record:#?}"
    );
    assert_eq!(
        record.diagnostic_identifier.as_deref(),
        Some("rust-analyzer")
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

/// Most probes one readiness assertion makes, and the pause between them:
/// at most 30s of waiting, then the test fails instead of hanging.
const READINESS_PROBES_MAX: usize = 150;
const READINESS_PAUSE: Duration = Duration::from_millis(200);

/// rust-analyzer announces the project load it does after initialize, and
/// the session reads that announcement as the engine still analyzing.
///
/// The engine mints its token through `window/workDoneProgress/create` and
/// begins it, which the session reads during the first request after the
/// handshake. Every answer it gives until the token ends is provisional -
/// the pull comes back with no items, which is exactly what a clean file
/// answers - and the token ends once the load is done. Both transitions
/// are asserted, so an engine that stopped reporting progress fails here
/// instead of silently making every answer read as settled.
#[tokio::test]
async fn work_done_progress_marks_the_project_load() {
    if !engine_live() {
        return;
    }
    let workspace = cargo_project();
    require_rust_analyzer(workspace.path());
    let mut session = EngineSession::start(launch(), workspace.path())
        .await
        .expect("rust-analyzer starts");
    let document = ProjectPath::new("caller.rs").expect("fixture path is valid");
    session
        .open(&document, "rust", CALLER.to_owned())
        .await
        .expect("didOpen is sent");

    let started = Instant::now();
    let mut announced = false;
    let mut settled = None;
    for _probe in 0..READINESS_PROBES_MAX {
        // The pull is what makes the session read the engine's traffic;
        // its answer is not what this test reads, the progress record is,
        // and a loading engine cancels some of these pulls outright.
        let _answer = session.pull_diagnostics(&document).await;
        if session.is_analyzing() {
            announced = true;
        } else if announced {
            settled = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(READINESS_PAUSE).await;
    }
    assert!(
        announced,
        "rust-analyzer must announce its project load over $/progress"
    );
    let settled = settled.expect("the announced load must end inside the probe bound");
    eprintln!("rust-analyzer ended its load progress after {settled:?}");
    let declaration = ProjectPath::new("hub.rs").expect("fixture declaration path");
    session
        .open(&declaration, "rust", HUB.to_owned())
        .await
        .expect("declaration opens");
    let locations = session
        .references(&declaration, lsp_types::Position::new(0, 7))
        .await
        .expect("the loaded function references resolve");
    for (path, line, character) in [("hub.rs", 0, 7), ("caller.rs", 3, 4)] {
        assert!(
            locations
                .iter()
                .any(|location| location.uri.path().as_str().ends_with(path)
                    && location.range.start == lsp_types::Position::new(line, character)),
            "the reference in {path} must resolve: {locations:?}"
        );
    }
    session.shutdown().await;
}

/// Most time a walk waits for rust-analyzer to read ready: past it the test
/// fails instead of hanging.
const WALK_READY_LIMIT: Duration = Duration::from_mins(1);

/// A rust-analyzer session over `workspace` that reads ready for a walk:
/// every load token but the `cargo check` flycheck ones ended, then
/// `settle_delay` of quiet, read through [`EngineSession::read_output`] as a
/// walk's wait reads it. Before that, rust-analyzer answers an empty prepare.
async fn walk_ready_session(workspace: &Path) -> EngineSession {
    let mut session = EngineSession::start(launch(), workspace)
        .await
        .expect("rust-analyzer starts");
    let started = Instant::now();
    session
        .read_output(tokio::time::Instant::now() + WALK_READY_LIMIT, |session| {
            session.walk_readiness() == EngineReadiness::Ready
        })
        .await
        .expect("the engine's output reads");
    assert_eq!(
        session.walk_readiness(),
        EngineReadiness::Ready,
        "rust-analyzer must read ready for a walk inside {WALK_READY_LIMIT:?}"
    );
    eprintln!(
        "rust-analyzer read ready for a walk after {:?}, session readiness {:?}",
        started.elapsed(),
        session.readiness()
    );
    session
}

/// Once rust-analyzer reads ready for a walk, prepare at a function answers
/// one item and outgoing calls name its callees: one in the project and two
/// in `std`. `vec!` is a macro, so it is no callee.
#[tokio::test]
async fn rust_analyzer_names_the_callees_of_a_prepared_function() {
    if !engine_live() {
        return;
    }
    let workspace = cargo_project();
    require_rust_analyzer(workspace.path());
    let mut session = walk_ready_session(workspace.path()).await;
    let document = ProjectPath::new("walk.rs").expect("fixture path is valid");
    session
        .open(&document, "rust", WALK.to_owned())
        .await
        .expect("didOpen is sent");
    let asked = Instant::now();
    let items = session
        .prepare_call_hierarchy(&document, lsp_types::Position::new(6, 7))
        .await
        .expect("prepare answers");
    let [item] = items.as_slice() else {
        panic!("one item at `larger`: {items:?}");
    };
    assert_eq!(item.name, "larger");
    let calls = session
        .outgoing_calls(item.clone())
        .await
        .expect("outgoing calls answer");
    eprintln!(
        "prepare and outgoing calls answered in {:?}",
        asked.elapsed()
    );
    let callees: BTreeSet<&str> = calls.iter().map(|call| call.to.name.as_str()).collect();
    assert_eq!(
        callees,
        BTreeSet::from(["beacon", "len", "max"]),
        "{calls:#?}"
    );
    assert!(
        calls
            .iter()
            .any(|call| call.to.name == "beacon" && call.to.uri.path().as_str().ends_with("hub.rs")),
        "the project callee answers its own file: {calls:#?}"
    );
    session.shutdown().await;
}

/// Once rust-analyzer reads ready for a walk, prepare at a struct answers
/// no item, the empty prepare that refuses the seed; prepare at a function
/// in the same file still answers one.
#[tokio::test]
async fn rust_analyzer_prepares_no_call_hierarchy_item_at_a_struct() {
    if !engine_live() {
        return;
    }
    let workspace = cargo_project();
    require_rust_analyzer(workspace.path());
    let mut session = walk_ready_session(workspace.path()).await;
    let document = ProjectPath::new("walk.rs").expect("fixture path is valid");
    session
        .open(&document, "rust", WALK.to_owned())
        .await
        .expect("didOpen is sent");
    let at_struct = session
        .prepare_call_hierarchy(&document, lsp_types::Position::new(2, 11))
        .await
        .expect("prepare answers");
    assert!(at_struct.is_empty(), "no item at `Beacon`: {at_struct:?}");
    let at_function = session
        .prepare_call_hierarchy(&document, lsp_types::Position::new(6, 7))
        .await
        .expect("prepare answers");
    assert_eq!(
        at_function.len(),
        1,
        "the engine prepares a function in the same file: {at_function:?}"
    );
    session.shutdown().await;
}

#[test]
fn rust_engine_fixture_pins_1_98() {
    let fixture = rust_engine::fixture();
    assert_eq!(fixture.program, "rustup");
    assert_eq!(fixture.arguments, ["run", "1.98", "rust-analyzer"]);
}
