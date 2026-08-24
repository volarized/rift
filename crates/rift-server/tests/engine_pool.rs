//! Integration tests: the engine pool against the scripted fake engine.
//!
//! Every test resolves rift-lsp's `fake_engine` binary through an overlaid
//! `PATH`, exactly as an operator's `[engines.<name>]` table would resolve
//! a real engine. The fake engine's lifecycle log counts how many engine
//! processes initialized and exited, so spawn-once, reuse, respawn, and
//! shutdown are each proven by counting, never by timing.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::Position;
use rift_core::ProjectPath;
use rift_lsp::session::{EngineFault, EngineSession};
use rift_protocol::configuration::{ByteSize, Duration, EngineConfiguration};
use rift_protocol::read::Language;
use rift_server::EnginePool;

/// The directory holding the compiled `fake_engine` binary.
///
/// This test binary runs from `target/<profile>/deps`, and Cargo places
/// another crate's binary one level up. Running the suite with `rift-lsp`
/// in the invocation - the workspace suite does - builds the binary before
/// any test runs.
fn fake_engine_directory() -> PathBuf {
    let mut directory = std::env::current_exe().expect("the test binary has a path");
    directory.pop();
    if directory.ends_with("deps") {
        directory.pop();
    }
    assert!(
        directory.join("fake_engine").exists(),
        "fake_engine is missing from {}: build it first with `cargo test -p rift-lsp`",
        directory.display(),
    );
    directory
}

/// One engine table resolving `fake_engine` through an overlaid `PATH`,
/// with the lifecycle log riding the engine's environment.
fn engine_table(behavior: &str, languages: &[&str], log: &Path) -> EngineConfiguration {
    let inherited = std::env::var("PATH").unwrap_or_default();
    let mut environment = BTreeMap::new();
    environment.insert(
        "PATH".to_owned(),
        format!("{}:{inherited}", fake_engine_directory().display()),
    );
    environment.insert(
        "RIFT_FAKE_ENGINE_LIFECYCLE_LOG".to_owned(),
        log.display().to_string(),
    );
    EngineConfiguration {
        program: "fake_engine".to_owned(),
        arguments: vec![behavior.to_owned()],
        environment,
        languages: languages
            .iter()
            .map(|&language| language.to_owned())
            .collect(),
        initialization_options: None,
        startup_timeout: Duration::from_millis(10_000),
        request_timeout: Duration::from_millis(10_000),
        output_limit: ByteSize::from_bytes(4_096),
    }
}

fn pool_of(workspace: &Path, entries: Vec<(&str, EngineConfiguration)>) -> EnginePool {
    let engines = entries
        .into_iter()
        .map(|(name, engine)| (name.to_owned(), engine))
        .collect();
    EnginePool::new(workspace, engines)
}

fn language(name: &str) -> Language {
    Language {
        name: name.to_owned(),
        dialect: None,
    }
}

fn document() -> ProjectPath {
    ProjectPath::new("src/lib.rs").expect("fixture path is valid")
}

fn start_position() -> Position {
    Position {
        line: 0,
        character: 3,
    }
}

/// Lines of one lifecycle event recorded so far.
fn recorded(log: &Path, event: &str) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == event)
        .count()
}

/// One rename request through the pool for `name`, discarding the edit.
async fn rename_through(
    pool: &EnginePool,
    name: &str,
    new_name: &str,
) -> Result<(), rift_lsp::session::EngineError> {
    let slot = pool
        .engine_for(&language(name))
        .expect("the language is served");
    let target = document();
    let renamed = new_name.to_owned();
    slot.request(move |session: &mut EngineSession| {
        let target = target.clone();
        let renamed = renamed.clone();
        Box::pin(async move {
            session
                .rename(&target, start_position(), &renamed)
                .await
                .map(|_edit| ())
        })
    })
    .await
}

#[tokio::test]
async fn first_request_spawns_and_later_requests_reuse_one_session() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![("fake", engine_table("happy", &["rust", "python"], &log))],
    );
    assert_eq!(
        recorded(&log, "initialize"),
        0,
        "building the pool spawns nothing"
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the first request serves");
    rename_through(&pool, "python", "renamed")
        .await
        .expect("a second language served by the same engine reuses it");
    assert_eq!(
        recorded(&log, "initialize"),
        1,
        "one engine serves every request and every language it claims"
    );
    pool.shutdown().await;
    assert_eq!(
        recorded(&log, "exit"),
        1,
        "shutdown asks the engine to exit"
    );
}

#[tokio::test]
async fn dead_engine_is_respawned_once_and_a_second_death_surfaces() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![("fake", engine_table("dies-on-command", &["rust"], &log))],
    );
    let error = rename_through(&pool, "rust", "die")
        .await
        .expect_err("the engine dies on both attempts");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. }
    ));
    assert_eq!(
        recorded(&log, "initialize"),
        2,
        "the request spawned once and respawned exactly once"
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the next request replaces the dead engine and serves");
    assert_eq!(
        recorded(&log, "initialize"),
        3,
        "the later request respawned the dead engine once"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn refusal_leaves_the_engine_serving_without_a_respawn() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![("fake", engine_table("refuses-rename", &["rust"], &log))],
    );
    let refusal = rename_through(&pool, "rust", "1nvalid")
        .await
        .expect_err("the engine refuses the name");
    assert!(matches!(refusal.fault(), EngineFault::Refused { .. }));
    let slot = pool
        .engine_for(&language("rust"))
        .expect("the language is served");
    let target = document();
    slot.request(move |session: &mut EngineSession| {
        let target = target.clone();
        Box::pin(async move {
            session
                .prepare_rename(&target, start_position())
                .await
                .map(|_verdict| ())
        })
    })
    .await
    .expect("the refusal left the engine serving");
    assert_eq!(
        recorded(&log, "initialize"),
        1,
        "a refusal never respawns the engine"
    );
    pool.shutdown().await;
}

#[tokio::test]
async fn initialization_options_reach_the_engine_through_the_pool() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let mut asserting = engine_table("initialization-options", &["rust"], &log);
    asserting.initialization_options = Some(serde_json::json!({ "engine": "fake" }));
    let pool = pool_of(workspace.path(), vec![("fake", asserting)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the engine verified its initialization options and serves");
    pool.shutdown().await;
}

#[tokio::test]
async fn shutdown_walks_every_running_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![
            ("a", engine_table("happy", &["rust"], &log)),
            ("b", engine_table("happy", &["python"], &log)),
        ],
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the first engine serves");
    rename_through(&pool, "python", "renamed")
        .await
        .expect("the second engine serves");
    assert_eq!(recorded(&log, "initialize"), 2);
    pool.shutdown().await;
    assert_eq!(
        recorded(&log, "exit"),
        2,
        "shutdown asks every running engine to exit"
    );
}

#[tokio::test]
async fn unserved_language_answers_no_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![("fake", engine_table("happy", &["rust"], &log))],
    );
    assert!(pool.engine_for(&language("go")).is_none());
    assert_eq!(
        recorded(&log, "initialize"),
        0,
        "an absent engine spawns nothing"
    );
}

/// Two requests race on an empty slot; the spawn is gated by a FIFO the
/// test controls, so both requests are provably in flight before any
/// engine finishes initializing. One initialize line proves the slot lock
/// serialized the spawn instead of starting two engines.
#[tokio::test]
async fn concurrent_requests_on_an_empty_slot_spawn_one_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let gate = workspace.path().join("start.gate");
    let made = std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo must create the gate");
    let mut gated = engine_table("happy", &["rust"], &log);
    gated.environment.insert(
        "RIFT_FAKE_ENGINE_START_GATE".to_owned(),
        gate.display().to_string(),
    );
    let pool = Arc::new(pool_of(workspace.path(), vec![("fake", gated)]));
    let (issued_sender, mut issued) = tokio::sync::mpsc::channel::<()>(2);
    let mut racers = Vec::new();
    for _ in 0..2 {
        let pool = Arc::clone(&pool);
        let issued_sender = issued_sender.clone();
        racers.push(tokio::spawn(async move {
            issued_sender.send(()).await.expect("the test listens");
            rename_through(&pool, "rust", "renamed").await
        }));
    }
    for _ in 0..2 {
        issued.recv().await.expect("both requests were issued");
    }
    // Both requests are in flight and every spawned engine is parked on
    // the gate; opening and closing the write end releases them all.
    tokio::task::spawn_blocking(move || std::fs::write(&gate, b"go"))
        .await
        .expect("the gate writer joins")
        .expect("the gate opens");
    for racer in racers {
        racer
            .await
            .expect("the request task joins")
            .expect("the request serves");
    }
    assert_eq!(
        recorded(&log, "initialize"),
        1,
        "the racing requests must share one spawned engine"
    );
    pool.shutdown().await;
}
