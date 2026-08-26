//! Integration tests: the engine pool against real minimal processes.
//!
//! Every engine table in `process_lifecycle.rs` resolves a plain `sh -c`
//! script: spawn-once, reuse, restart, and shutdown are each proven by a
//! canned sequence of framed answers, never by a scripted engine binary
//! that speaks a misbehavior grid. A scenario needing an engine to decide,
//! from its own protocol state, whether an answer is settled - the
//! provisional-answer absorption and the empty-answer resend policy - has
//! no such fixture here: it needs an engine that can be asked an unbounded
//! number of times and answer differently once it has "settled," which a
//! fixed canned sequence cannot express, and no real language engine can
//! be told to behave that way on command. That coverage is not this
//! suite's to add; it lands wherever the absorption policy itself is
//! reworked next.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;

use lsp_types::Position;
use process_lifecycle::{
    absent_program, absolute_program, answers, answers_initialize_then_exits,
    answers_initialize_then_hangs, dies_once_then_answers, gated_then_answers, null_response,
    ok_response, refused_response, retrying,
};
use rift_core::ProjectPath;
use rift_lsp::session::{EngineFault, EngineSession};
use rift_protocol::configuration::{Duration, EngineConfiguration};
use rift_protocol::read::Language;
use rift_protocol::retry::RestartPolicy;
use rift_server::EnginePool;

mod process_lifecycle;

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

/// One rename conversation through the pool for `name`, discarding the
/// edit.
///
/// The document is opened first, as the server's own rename does, so a
/// restarted engine sees the target it renames.
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
    let responses = vec![ok_response(1), ok_response(2)];
    let pool = pool_of(
        workspace.path(),
        vec![("fake", answers(&responses, &["rust", "python"]))],
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the first request serves");
    rename_through(&pool, "python", "renamed")
        .await
        .expect("a second language served by the same engine reuses it");
    pool.shutdown().await;
}

#[tokio::test]
async fn dead_engine_is_restarted_within_the_budget_and_a_death_past_it_surfaces() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut dies = answers_initialize_then_exits(&["rust"]);
    dies.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
    let pool = pool_of(workspace.path(), vec![("fake", dies)]);
    let error = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the engine dies on both attempts");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. }
    ));
    pool.shutdown().await;
}

/// A budget spent inside its window refuses the next request's start.
#[tokio::test]
async fn restart_budget_spent_inside_the_window_refuses_the_next_start() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut dies = answers_initialize_then_exits(&["rust"]);
    dies.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
    let pool = pool_of(workspace.path(), vec![("fake", dies)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the engine dies on both attempts");
    let refused = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("the spent budget refuses another start");
    assert!(
        matches!(refused.fault(), EngineFault::Ended),
        "unexpected fault {:?}",
        refused.fault()
    );
    pool.shutdown().await;
}

/// An engine that stopped answering is restarted, not merely reported.
#[tokio::test]
async fn engine_that_stopped_answering_is_restarted_within_the_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut deaf = answers_initialize_then_hangs(&["rust"]);
    deaf.request_timeout = Duration::from_millis(1_000);
    deaf.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
    let pool = pool_of(workspace.path(), vec![("fake", deaf)]);
    let error = rename_through(&pool, "rust", "renamed")
        .await
        .expect_err("neither engine answers");
    assert!(
        matches!(error.fault(), EngineFault::TimedOut { .. }),
        "unexpected fault {:?}",
        error.fault()
    );
    pool.shutdown().await;
}

/// A start that never succeeds spends the budget like any other restart.
#[tokio::test]
async fn failed_starts_spend_the_restart_budget() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut absent = absent_program(&["rust"]);
    absent.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
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
#[tokio::test(start_paused = true)]
async fn restart_budget_frees_once_its_window_passes() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut absent = absent_program(&["rust"]);
    absent.restart = RestartPolicy {
        attempts: 1,
        window: Duration::from_millis(1_000),
    };
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
#[tokio::test]
async fn a_configuration_fault_surfaces_without_restarting() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut absolute = absolute_program(&["rust"]);
    absolute.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
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
/// The gated engine parks inside its start, holding its slot's lock for
/// the whole test; the second engine serves anyway, and the first
/// completes once its gate opens.
#[tokio::test]
async fn a_request_held_in_one_slot_leaves_other_engines_serving() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let gate = workspace.path().join("start.gate");
    let made = std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo must create the gate");
    let gated = gated_then_answers(&gate, &[ok_response(1)], &["rust"]);
    let free = answers(&[ok_response(1)], &["python"]);
    let pool = Arc::new(pool_of(
        workspace.path(),
        vec![("gated", gated), ("free", free)],
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
    tokio::task::spawn_blocking(move || std::fs::write(&gate, b"go\n"))
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
    let responses = vec![
        refused_response(1, -32602, "new name is not an identifier"),
        null_response(2),
    ];
    let pool = pool_of(
        workspace.path(),
        vec![("fake", answers(&responses, &["rust"]))],
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
    pool.shutdown().await;
}

#[tokio::test]
async fn shutdown_walks_every_running_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let pool = pool_of(
        workspace.path(),
        vec![
            ("a", answers(&[ok_response(1)], &["rust"])),
            ("b", answers(&[ok_response(1)], &["python"])),
        ],
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the first engine serves");
    rename_through(&pool, "python", "renamed")
        .await
        .expect("the second engine serves");
    let started_at = std::time::Instant::now();
    pool.shutdown().await;
    assert!(
        started_at.elapsed() < std::time::Duration::from_secs(10),
        "shutdown must walk every running engine without stalling on any one"
    );
}

#[tokio::test]
async fn unserved_language_answers_no_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let pool = pool_of(
        workspace.path(),
        vec![("fake", answers(&[ok_response(1)], &["rust"]))],
    );
    assert!(pool.engine_for(&language("go")).is_none());
    pool.shutdown().await;
}

/// Two requests race on an empty slot; the spawn is gated by a FIFO the
/// test controls, so both requests are provably in flight before any
/// engine finishes initializing. Both racers target the same language, so
/// the one spawned engine answers both requests it is asked for in order.
#[tokio::test]
async fn concurrent_requests_on_an_empty_slot_spawn_one_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let gate = workspace.path().join("start.gate");
    let made = std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo runs");
    assert!(made.success(), "mkfifo must create the gate");
    let gated = gated_then_answers(&gate, &[ok_response(1), ok_response(2)], &["rust"]);
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
    // Both requests are in flight and the spawned engine is parked on the
    // gate; opening and closing the write end releases it.
    tokio::task::spawn_blocking(move || std::fs::write(&gate, b"go\n"))
        .await
        .expect("the gate writer joins")
        .expect("the gate opens");
    for racer in racers {
        racer
            .await
            .expect("the request task joins")
            .expect("the request serves");
    }
    pool.shutdown().await;
}

/// A refusal the engine invites again is absorbed: the resend answers, and
/// the caller never learns the first attempt happened.
#[tokio::test]
async fn a_retryable_refusal_is_absorbed_and_the_resend_answers() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let responses = vec![
        refused_response(1, -32802, "server cancelled the request"),
        ok_response(2),
    ];
    let pool = pool_of(
        workspace.path(),
        vec![("fake", retrying(answers(&responses, &["rust"]), 3))],
    );
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the resend answers");
    pool.shutdown().await;
}

/// A refusal that is the engine's verdict on the request surfaces at once:
/// resending it would change nothing and cost the caller its latency.
#[tokio::test]
async fn a_verdict_refusal_surfaces_without_a_resend() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let responses = vec![refused_response(1, -32602, "new name is not an identifier")];
    let pool = pool_of(
        workspace.path(),
        vec![("fake", answers(&responses, &["rust"]))],
    );
    let error = rename_through(&pool, "rust", "1nvalid")
        .await
        .expect_err("the engine refuses the name");
    assert!(
        matches!(error.fault(), EngineFault::Refused { code: -32602, .. }),
        "unexpected fault {:?}",
        error.fault()
    );
    pool.shutdown().await;
}

/// An engine that dies mid-request is replaced inside its restart budget
/// and the operation runs on the replacement, so the caller sees the
/// answer and never the death.
#[tokio::test]
async fn an_engine_that_dies_mid_request_is_replaced_before_the_caller_sees_it() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let marker = workspace.path().join("restarted.marker");
    let mut dies_once = dies_once_then_answers(&marker, &[ok_response(1)], &["rust"]);
    dies_once.restart = RestartPolicy {
        attempts: 1,
        ..RestartPolicy::default()
    };
    let pool = pool_of(workspace.path(), vec![("fake", dies_once)]);
    rename_through(&pool, "rust", "renamed")
        .await
        .expect("the replacement answers the same operation");
    pool.shutdown().await;
}
