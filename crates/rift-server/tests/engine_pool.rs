//! Integration tests: the engine pool against the scripted fake engine.
//!
//! Every test resolves rift-lsp's `fake_engine` binary through an overlaid
//! `PATH`, exactly as an operator's `[engines.<name>]` table would resolve
//! a real engine. The fake engine's lifecycle log counts how many engine
//! processes initialized, how many renames each was asked for, and how
//! many exited, so spawn-once, reuse, restart, absorption, and shutdown
//! are each proven by counting, never by timing.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_types::Position;
use rift_core::{ErrorCode, ErrorName, ProjectPath};
use rift_lsp::session::{EngineFault, EngineSession};
use rift_protocol::configuration::{ByteSize, Duration, EngineConfiguration};
use rift_protocol::read::Language;
use rift_protocol::retry::{RestartPolicy, RetryPolicy};
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
        retry: RetryPolicy::default(),
        restart: RestartPolicy::default(),
    }
}

/// The same table with its restart budget narrowed to `attempts`.
fn restarting(mut table: EngineConfiguration, attempts: u64) -> EngineConfiguration {
    table.restart = RestartPolicy {
        attempts,
        ..RestartPolicy::default()
    };
    table
}

/// The same table with its retry budget narrowed to `attempts`.
///
/// The waits are held at a millisecond so the suite spends no time on
/// them; the shape of the growing wait is proven by the policy's own unit
/// tests.
fn retrying(mut table: EngineConfiguration, attempts: u64) -> EngineConfiguration {
    table.retry = RetryPolicy {
        attempts,
        delay: Duration::from_millis(1),
        delay_limit: Duration::from_millis(1),
    };
    table
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

/// One rename conversation through the pool for `name`, discarding the
/// edit.
///
/// The document is opened first, as the server's own rename does, so an
/// absorbed condition sends the whole conversation again and every
/// scripted behavior sees the target it renames.
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
                .open(&target, "rust", "fn beacon() {}\n".to_owned())
                .await?;
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
async fn dead_engine_is_restarted_within_the_budget_and_a_death_past_it_surfaces() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![(
            "fake",
            restarting(engine_table("dies-on-command", &["rust"], &log), 1),
        )],
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
        "the request started one engine and restarted it once"
    );
    pool.shutdown().await;
}

/// A budget spent inside its window refuses the next request's start.
///
/// The one-restart budget is spent by the first request's crash loop, so
/// the second request finds no engine and no budget to start one: it
/// surfaces `temporarily_unavailable` without adding a lifecycle line.
#[tokio::test]
async fn restart_budget_spent_inside_the_window_refuses_the_next_start() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let pool = pool_of(
        workspace.path(),
        vec![(
            "fake",
            restarting(engine_table("dies-on-command", &["rust"], &log), 1),
        )],
    );
    rename_through(&pool, "rust", "die")
        .await
        .expect_err("the engine dies on both attempts");
    assert_eq!(recorded(&log, "initialize"), 2);
    let refused = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the spent budget refuses another start");
    assert!(
        matches!(refused.fault(), EngineFault::Ended),
        "unexpected fault {:?}",
        refused.fault()
    );
    assert_eq!(
        recorded(&log, "initialize"),
        2,
        "a refused claim starts no engine"
    );
    pool.shutdown().await;
}

/// An engine that stopped answering is restarted, not merely reported.
///
/// The `deaf` behavior completes its handshake and then reads nothing, so
/// the rename overstays the one-second request bound and the session ends
/// itself. The one-restart budget carries exactly one replacement, which
/// goes deaf the same way, and the timeout surfaces.
#[tokio::test]
async fn engine_that_stopped_answering_is_restarted_within_the_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let mut deaf = restarting(engine_table("deaf", &["rust"], &log), 1);
    deaf.request_timeout = Duration::from_millis(1_000);
    let pool = pool_of(workspace.path(), vec![("fake", deaf)]);
    let error = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("neither engine answers");
    assert!(
        matches!(error.fault(), EngineFault::TimedOut { .. }),
        "unexpected fault {:?}",
        error.fault()
    );
    assert_eq!(
        recorded(&log, "initialize"),
        2,
        "the timed-out engine was replaced once"
    );
    pool.shutdown().await;
}

/// A start that never succeeds spends the budget like any other restart.
///
/// The program resolves through no `PATH` entry, so every start fails
/// before a process exists. The first request spends its one restart on a
/// second failed start; the second request has none left and surfaces
/// `temporarily_unavailable` instead of trying again.
#[tokio::test]
async fn failed_starts_spend_the_restart_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let mut absent = restarting(engine_table("happy", &["rust"], &log), 1);
    absent.program = "rift_absent_engine".to_owned();
    let pool = pool_of(workspace.path(), vec![("fake", absent)]);
    let error = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the program cannot be started");
    assert!(
        matches!(error.fault(), EngineFault::LaunchFailed { .. }),
        "unexpected fault {:?}",
        error.fault()
    );
    let refused = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the spent budget refuses another start");
    assert!(
        matches!(refused.fault(), EngineFault::Ended),
        "unexpected fault {:?}",
        refused.fault()
    );
}

/// A restart that left the window stops counting against the budget.
///
/// The clock is paused, so the window passes without the test waiting for
/// it. This engine's program resolves nowhere, so no child process is ever
/// created and the paused clock drives the whole test: a runtime with its
/// time frozen auto-advances whenever it is idle, and an idle wait on a
/// real engine's pipes would jump straight onto that engine's own request
/// bound. The answer is what proves the window: a spent budget refuses
/// with `Ended`, and once the earlier restart has left the window the same
/// request reaches the start again and reports the start's own failure.
#[tokio::test(start_paused = true)]
async fn restart_budget_frees_once_its_window_passes() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let mut absent = restarting(engine_table("happy", &["rust"], &log), 1);
    absent.program = "rift_absent_engine".to_owned();
    absent.restart.window = Duration::from_millis(1_000);
    let pool = pool_of(workspace.path(), vec![("fake", absent)]);
    let failed = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the program cannot be started");
    assert!(matches!(failed.fault(), EngineFault::LaunchFailed { .. }));
    let refused = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the spent budget refuses another start");
    assert!(
        matches!(refused.fault(), EngineFault::Ended),
        "unexpected fault {:?}",
        refused.fault()
    );
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let freed = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the program still cannot be started");
    assert!(
        matches!(freed.fault(), EngineFault::LaunchFailed { .. }),
        "the freed budget must reach the start again, not refuse: {:?}",
        freed.fault()
    );
}

/// A configuration fault surfaces at once instead of restarting.
///
/// An absolute program is refused before any process exists and answers
/// the same way every time, so the pool never restarts past it. The
/// one-restart budget makes that countable: a request that did restart
/// would spend the whole budget on its own, leaving the second request
/// nothing and turning its answer into `temporarily_unavailable`.
#[tokio::test]
async fn a_configuration_fault_surfaces_without_restarting() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let mut absolute = restarting(engine_table("happy", &["rust"], &log), 1);
    absolute.program = "/usr/bin/rift_absent_engine".to_owned();
    let pool = pool_of(workspace.path(), vec![("fake", absolute)]);
    for _ in 0..2 {
        let error = rename_through(&pool, "rust", "renamed")
            .await
            .expect_err("the program is refused");
        assert!(
            matches!(error.fault(), EngineFault::ProgramAbsolute { .. }),
            "unexpected fault {:?}",
            error.fault()
        );
    }
}

/// One slot's held lock never stalls another engine.
///
/// Each slot owns its own lock over its own session and restart budget, so
/// a request that waits inside one slot - on a spawn, on a restart, on a
/// wait an operation takes between its own attempts - leaves every other
/// slot free. The gated engine parks inside its start, holding its slot's
/// lock for the whole test; the second engine serves anyway, and the first
/// completes once its gate opens.
#[tokio::test]
async fn a_request_held_in_one_slot_leaves_other_engines_serving() {
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
    let pool = Arc::new(pool_of(
        workspace.path(),
        vec![
            ("gated", gated),
            ("free", engine_table("happy", &["python"], &log)),
        ],
    ));
    let (issued_sender, mut issued) = tokio::sync::mpsc::channel::<()>(1);
    let held = tokio::spawn({
        let pool = Arc::clone(&pool);
        async move {
            issued_sender.send(()).await.expect("the test listens");
            rename_through(&pool, "rust", "renamed").await
        }
    });
    issued.recv().await.expect("the held request was issued");
    rename_through(&pool, "python", "renamed")
        .await
        .expect("the second engine serves while the first slot is held");
    tokio::task::spawn_blocking(move || std::fs::write(&gate, b"go"))
        .await
        .expect("the gate writer joins")
        .expect("the gate opens");
    held.await
        .expect("the held task joins")
        .expect("the released request serves");
    pool.shutdown().await;
}

#[tokio::test]
async fn refusal_leaves_the_engine_serving_without_a_restart() {
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
        "a refusal never restarts the engine"
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

/// The attempt bound the absorption tests state for themselves, small
/// enough to count off the lifecycle log.
const ABSORPTION_ATTEMPTS: u64 = 3;

/// An answer the engine gave while it was still analyzing is provisional,
/// so the slot asks again until the engine ends its work.
///
/// The scripted engine begins work-done progress at initialize and ends it
/// before the second rename it is asked for. The lifecycle log counts both
/// renames, and the caller sees one settled answer.
#[tokio::test]
async fn a_provisional_answer_is_asked_again_until_the_engine_settles() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("analyzes-then-serves", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the settled answer comes back as the operation's own");
    assert_eq!(
        recorded(&log, "rename"),
        2,
        "the provisional answer was discarded and the operation ran again"
    );
    assert_eq!(recorded(&log, "initialize"), 1, "nothing was restarted");
    pool.shutdown().await;
}

/// An engine that never ends its work spends the whole attempt bound and
/// then says so, instead of handing the caller a provisional answer.
#[tokio::test]
async fn an_engine_that_never_settles_spends_the_budget_and_reports_it() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("never-ends-progress", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let error = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("every attempt was answered mid-analysis");
    assert!(
        matches!(
            error.fault(),
            EngineFault::Analyzing { attempts } if *attempts == ABSORPTION_ATTEMPTS
        ),
        "unexpected fault {:?}",
        error.fault()
    );
    assert_eq!(
        error.name(),
        ErrorName::Wire(ErrorCode::TemporarilyUnavailable),
        "the caller is told to send the request again"
    );
    assert_eq!(
        recorded(&log, "rename"),
        usize::try_from(ABSORPTION_ATTEMPTS).expect("the bound fits in usize"),
        "the engine was asked exactly as often as the table allows"
    );
    pool.shutdown().await;
}

/// A refusal the engine invites again is absorbed: the resend answers, and
/// the caller never learns the first attempt happened.
#[tokio::test]
async fn a_retryable_refusal_is_absorbed_and_the_resend_answers() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("cancels-first-rename", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the resend answers");
    assert_eq!(recorded(&log, "rename"), 2);
    assert_eq!(recorded(&log, "initialize"), 1, "a refusal never restarts");
    pool.shutdown().await;
}

/// A refusal that is the engine's verdict on the request surfaces at once:
/// resending it would change nothing and cost the caller its latency.
#[tokio::test]
async fn a_verdict_refusal_surfaces_without_a_resend() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("refuses-rename", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let error = rename_through(&pool, "rust", "1nvalid")
        .await
        .expect_err("the engine refuses the name");
    assert!(
        matches!(error.fault(), EngineFault::Refused { code: -32602, .. }),
        "unexpected fault {:?}",
        error.fault()
    );
    assert_eq!(
        recorded(&log, "rename"),
        1,
        "a verdict is never sent a second time"
    );
    pool.shutdown().await;
}

/// An engine that dies mid-request is replaced inside its restart budget
/// and the operation runs on the replacement, so the caller sees the
/// answer and never the death.
#[tokio::test]
async fn an_engine_that_dies_mid_request_is_replaced_before_the_caller_sees_it() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = restarting(engine_table("dies-once-on-rename", &["rust"], &log), 1);
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the replacement answers the same operation");
    assert_eq!(
        recorded(&log, "initialize"),
        2,
        "the dead engine was replaced once"
    );
    assert_eq!(recorded(&log, "rename"), 2);
    pool.shutdown().await;
}

/// A refusal the engine answered while it was still analyzing is not its
/// verdict on the request: rust-analyzer refuses a rename with `No
/// references found at position` for a declaration it has not indexed yet.
/// The slot sends the operation again, and the verdict the settled engine
/// reaches is the one that surfaces.
#[tokio::test]
async fn a_refusal_answered_mid_analysis_is_absorbed_until_the_engine_settles() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("refuses-while-analyzing", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let error = rename_through(&pool, "rust", "1nvalid")
        .await
        .expect_err("the settled engine still refuses the name");
    assert!(
        matches!(error.fault(), EngineFault::Refused { code: -32602, .. }),
        "the settled engine's own verdict surfaces: {:?}",
        error.fault()
    );
    assert_eq!(
        recorded(&log, "rename"),
        2,
        "the refusal answered mid-analysis was asked again"
    );
    pool.shutdown().await;
}

/// One will-rename conversation through the pool for `name`.
///
/// The request names the two URIs alone, so nothing is opened first and
/// the answer is the engine's whole contribution.
async fn will_rename_through(
    pool: &EnginePool,
    name: &str,
) -> Result<Option<lsp_types::WorkspaceEdit>, rift_lsp::session::EngineError> {
    let slot = pool
        .engine_for(&language(name))
        .expect("the language is served");
    // The boxed future may only borrow the session, so each attempt gets
    // its own owned copy of the request paths.
    let request_from = document();
    let request_to = ProjectPath::new("src/moved.rs").expect("fixture path is valid");
    slot.request(move |session: &mut EngineSession| {
        let from = request_from.clone();
        let to = request_to.clone();
        Box::pin(async move { session.will_rename_files(&from, &to).await })
    })
    .await
}

/// One diagnostic pull through the pool for `name`.
async fn pull_through(
    pool: &EnginePool,
    name: &str,
) -> Result<Vec<lsp_types::Diagnostic>, rift_lsp::session::EngineError> {
    let slot = pool
        .engine_for(&language(name))
        .expect("the language is served");
    let request_target = document();
    slot.request(move |session: &mut EngineSession| {
        let target = request_target.clone();
        Box::pin(async move {
            session
                .open(&target, "rust", "fn beacon() {}\n".to_owned())
                .await?;
            session.pull_diagnostics(&target).await
        })
    })
    .await
}

/// An engine that has never announced work answers nothing where
/// something was expected, and nothing is what a settled engine answers
/// for a document that is clean. The slot sends the pull again once, and
/// the answer the second pull carries is the one the caller gets.
#[tokio::test]
async fn an_empty_answer_from_an_unannounced_engine_is_pulled_again() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("pulls-empty-then-reports", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let pulled = pull_through(&pool, "rust")
        .await
        .expect("the engine answers both pulls");
    assert_eq!(
        pulled.len(),
        1,
        "the finding the second pull carried is the answer: {pulled:#?}"
    );
    assert_eq!(
        recorded(&log, "diagnostic"),
        2,
        "the empty answer was discarded and the document pulled again"
    );
    assert_eq!(recorded(&log, "initialize"), 1, "nothing was restarted");
    pool.shutdown().await;
}

/// The resend is claimed once per operation per session. An engine that
/// answers nothing twice has said what it has to say, and the second
/// answer comes back as the operation's own.
#[tokio::test]
async fn a_second_empty_answer_is_the_engines_own_and_is_never_asked_again() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(engine_table("happy", &["rust"], &log), ABSORPTION_ATTEMPTS);
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let proposal = will_rename_through(&pool, "rust")
        .await
        .expect("the engine answers both requests");
    assert!(
        rift_lsp::session::proposes_no_edit(proposal.as_ref()),
        "the engine proposed nothing twice: {proposal:#?}"
    );
    assert_eq!(
        recorded(&log, "will-rename"),
        2,
        "one resend, and only one, for an engine that announces nothing"
    );

    will_rename_through(&pool, "rust")
        .await
        .expect("the engine answers the later request");
    assert_eq!(
        recorded(&log, "will-rename"),
        3,
        "a later request on the same session pays no resend at all"
    );
    pool.shutdown().await;
}

/// An engine that has announced work keeps its empty answer: it reports
/// what it is doing, so its silence about a request is its own verdict
/// and the operation is never sent again.
#[tokio::test]
async fn an_announced_engine_keeps_its_empty_answer() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(
        engine_table("announces-then-answers-nothing", &["rust"], &log),
        ABSORPTION_ATTEMPTS,
    );
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let proposal = will_rename_through(&pool, "rust")
        .await
        .expect("the engine answers");
    assert!(
        proposal.is_none(),
        "the engine answered null: {proposal:#?}"
    );
    assert_eq!(
        recorded(&log, "will-rename"),
        1,
        "an engine that announced its work is asked once"
    );
    pool.shutdown().await;
}

/// A `retry` table with no second attempt in it carries no resend, and
/// the empty answer is then the only answer there is.
#[tokio::test]
async fn a_table_without_a_resend_hands_back_the_empty_answer() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let log = workspace.path().join("lifecycle.log");
    let table = retrying(engine_table("happy", &["rust"], &log), 1);
    let pool = pool_of(workspace.path(), vec![("fake", table)]);
    let proposal = will_rename_through(&pool, "rust")
        .await
        .expect("the engine's own answer comes back");
    assert!(
        rift_lsp::session::proposes_no_edit(proposal.as_ref()),
        "the engine proposed nothing: {proposal:#?}"
    );
    assert_eq!(
        recorded(&log, "will-rename"),
        1,
        "a table that allows one attempt asks once"
    );
    pool.shutdown().await;
}
