//! Integration tests: the session against the scripted fake engine.
//!
//! Every test spawns the real `fake_engine` binary over real pipes, so the
//! spawn policy, the framing, the routing, and the kill-and-reap paths are
//! proven live with no external language server.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use lsp_types::Position;
use rift_core::{ErrorCode, ErrorName, ProjectPath};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::framing::FramingFault;
use rift_lsp::session::{EngineFault, EngineLaunch, EngineSession};
use rift_lsp::uri::TreeRoot;

/// The compiled fake engine, resolved by Cargo for this test binary.
const FAKE_ENGINE: &str = env!("CARGO_BIN_EXE_fake_engine");

/// A launch resolving `fake_engine` through an overlaid `PATH`, proving
/// the overlay reaches program resolution.
fn launch(behavior: &str) -> EngineLaunch {
    let directory = Path::new(FAKE_ENGINE)
        .parent()
        .expect("the binary has a directory")
        .to_str()
        .expect("the target directory is Unicode");
    let inherited = std::env::var("PATH").unwrap_or_default();
    let mut environment = BTreeMap::new();
    environment.insert("PATH".to_owned(), format!("{directory}:{inherited}"));
    EngineLaunch {
        program: "fake_engine".to_owned(),
        arguments: vec![behavior.to_owned()],
        environment,
        initialization_options: None,
        startup_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(10),
        stderr_capture_bytes: 4_096,
    }
}

fn path(value: &str) -> ProjectPath {
    ProjectPath::new(value).expect("fixture path is valid")
}

async fn started(behavior: &str) -> (tempfile::TempDir, EngineSession) {
    let workspace = tempfile::tempdir().expect("tempdir");
    let session = EngineSession::start(launch(behavior), workspace.path())
        .await
        .expect("the fake engine starts");
    (workspace, session)
}

#[tokio::test]
async fn happy_engine_negotiates_renames_and_serves_diagnostics() {
    let (workspace, mut session) = started("happy").await;
    let record = session.capabilities();
    assert_eq!(record.position_encoding, PositionEncoding::Utf8);
    assert!(record.rename && record.prepare_rename);
    assert!(record.will_rename_files() && record.pull_diagnostics);
    assert_eq!(record.diagnostic_identifier.as_deref(), Some("fake"));
    assert_eq!(
        session.root(),
        &TreeRoot::new(workspace.path()).expect("the root converts")
    );

    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    let edit = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 3,
            },
            "renamed",
        )
        .await
        .expect("rename answers");
    assert!(
        session.published_diagnostics(&path("out.rs")).is_none(),
        "a publish outside the root is never retained"
    );
    assert_eq!(edit.changes.expect("changes come back").len(), 2);

    let published = session
        .published_diagnostics(&document)
        .expect("the didOpen publish was retained");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].message, "published diagnostic");

    let prepared = session
        .prepare_rename(
            &document,
            Position {
                line: 0,
                character: 3,
            },
        )
        .await
        .expect("prepareRename answers");
    assert!(prepared.is_some());

    let moved = session
        .will_rename_files(&document, &path("src/moved.rs"))
        .await
        .expect("willRenameFiles answers");
    assert!(moved.is_some());

    let pulled = session
        .pull_diagnostics(&document)
        .await
        .expect("diagnostic pull answers");
    assert_eq!(pulled.len(), 1);
    assert_eq!(pulled[0].message, "pulled diagnostic");

    session.close(&document).await.expect("didClose is sent");
    let stderr = session.shutdown().await;
    assert_eq!(stderr.total_bytes, 0);
}

#[tokio::test]
async fn engine_without_capabilities_gets_typed_refusals_before_any_request() {
    let (_workspace, mut session) = started("no-rename-capability").await;
    assert_eq!(
        session.capabilities().position_encoding,
        PositionEncoding::Utf16
    );
    let document = path("src/lib.rs");
    let refusal = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("rename was never advertised");
    assert!(matches!(
        refusal.fault(),
        EngineFault::CapabilityAbsent { capability } if capability == "textDocument/rename"
    ));
    assert_eq!(
        refusal.name(),
        ErrorName::Wire(ErrorCode::CapabilityUnavailable)
    );
    for absent in [
        session
            .prepare_rename(
                &document,
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await
            .err(),
        session
            .will_rename_files(&document, &path("b.rs"))
            .await
            .err(),
        session.pull_diagnostics(&document).await.err(),
    ] {
        let error = absent.expect("the capability gate refuses");
        assert!(matches!(
            error.fault(),
            EngineFault::CapabilityAbsent { .. }
        ));
    }
    session.shutdown().await;
}

#[tokio::test]
async fn unoffered_position_encoding_fails_the_start() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start(launch("bad-encoding"), workspace.path())
        .await
        .expect_err("utf-32 was never offered");
    assert!(matches!(error.fault(), EngineFault::Negotiation { .. }));
    assert_eq!(
        error.name(),
        ErrorName::Wire(ErrorCode::CapabilityUnavailable)
    );
}

#[tokio::test]
async fn silent_engine_is_killed_at_the_startup_timeout() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut silent = launch("never-responds");
    silent.startup_timeout = Duration::from_millis(300);
    let started = Instant::now();
    let error = EngineSession::start(silent, workspace.path())
        .await
        .expect_err("the engine never answers");
    assert!(matches!(error.fault(), EngineFault::TimedOut { .. }));
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the kill must not wait out the engine: {elapsed:?}"
    );
}

#[tokio::test]
async fn garbage_bytes_fail_the_start_with_a_framing_fault() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start(launch("garbage"), workspace.path())
        .await
        .expect_err("garbage is refused");
    assert!(matches!(error.fault(), EngineFault::Framing { .. }));
}

#[tokio::test]
async fn oversized_announcement_fails_the_start_as_limit_exceeded() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start(launch("oversized"), workspace.path())
        .await
        .expect_err("the announcement crosses the bound");
    match error.fault() {
        EngineFault::Framing { source } => {
            assert!(matches!(
                source.fault(),
                FramingFault::MessageTooLong { .. }
            ));
        }
        other => panic!("expected a framing fault, got {other:?}"),
    }
    assert_eq!(error.name(), ErrorName::Wire(ErrorCode::LimitExceeded));
}

#[tokio::test]
async fn engine_exit_mid_request_ends_the_session() {
    let (_workspace, mut session) = started("exit-mid-request").await;
    let document = path("src/lib.rs");
    let error = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("the engine exits instead of answering");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. }
    ));
    let ended = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("the session refuses after its engine ended");
    assert!(matches!(ended.fault(), EngineFault::Ended));
    session.shutdown().await;
}

#[tokio::test]
async fn stderr_flood_is_drained_bounded_while_the_request_answers() {
    let (_workspace, mut session) = started("stderr-flood").await;
    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect("the rename answers after the flood");
    let stderr = session.shutdown().await;
    assert_eq!(stderr.captured_bytes, 4_096);
    assert!(
        stderr.total_bytes >= 1 << 20,
        "total {}",
        stderr.total_bytes
    );
    assert!(stderr.truncated);
}

#[tokio::test]
async fn server_initiated_requests_are_routed_before_the_response() {
    let (_workspace, mut session) = started("server-requests").await;
    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    // The fake engine dies unless configuration, registration, progress,
    // and the unserved probe are each answered per the routing policy.
    let edit = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 3,
            },
            "renamed",
        )
        .await
        .expect("the rename answers after the server requests");
    assert_eq!(edit.changes.expect("changes come back").len(), 2);
    let unchanged = session
        .pull_diagnostics(&document)
        .await
        .expect("an unchanged report answers");
    assert!(unchanged.is_empty(), "an unchanged report carries no items");
    session.shutdown().await;
}

#[tokio::test]
async fn result_outside_the_method_shape_is_typed_and_non_fatal() {
    let (_workspace, mut session) = started("wrong-result").await;
    let document = path("src/lib.rs");
    let error = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("a number is not a workspace edit");
    assert!(matches!(error.fault(), EngineFault::ResultInvalid { .. }));
    session
        .prepare_rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("an invalid result leaves the engine serving");
    session.shutdown().await;
}

#[tokio::test]
async fn refused_rename_is_typed_and_leaves_the_session_serving() {
    let (_workspace, mut session) = started("refuses-rename").await;
    let document = path("src/lib.rs");
    let refusal = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "1nvalid",
        )
        .await
        .expect_err("the engine refuses the name");
    assert!(matches!(
        refusal.fault(),
        EngineFault::Refused { code: -32602, message, .. } if message == "new name is not an identifier"
    ));
    assert_eq!(refusal.name(), ErrorName::Wire(ErrorCode::InvalidRequest));
    session
        .prepare_rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("a refusal leaves the engine serving");
    session.shutdown().await;
}

/// A server cancellation is a refusal the caller may resend: the typed
/// fault carries the code, classifies as temporarily unavailable, and
/// leaves the engine serving the next request.
#[tokio::test]
async fn cancelled_rename_is_a_retryable_refusal() {
    let (_workspace, mut session) = started("cancels-rename").await;
    let document = path("src/lib.rs");
    let refusal = session
        .rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("the engine cancels the request");
    assert!(matches!(
        refusal.fault(),
        EngineFault::Refused { code: -32802, message, .. }
            if message == "server cancelled the request"
    ));
    assert!(refusal.fault().is_retryable_refusal());
    assert_eq!(
        refusal.name(),
        ErrorName::Wire(ErrorCode::TemporarilyUnavailable)
    );
    session
        .prepare_rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("a cancellation leaves the engine serving");
    session.shutdown().await;
}

#[tokio::test]
async fn payload_without_an_envelope_ends_the_session() {
    let (_workspace, mut session) = started("unreadable").await;
    let error = session
        .rename(
            &path("src/lib.rs"),
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        )
        .await
        .expect_err("the payload fits no envelope");
    assert!(matches!(error.fault(), EngineFault::MessageUnreadable));
    session.shutdown().await;
}

#[tokio::test]
async fn cancelled_request_response_is_discarded_by_the_next_call() {
    let (_workspace, mut session) = started("delayed-rename").await;
    let document = path("src/lib.rs");
    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        session.rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
            "renamed",
        ),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the caller cancels before the delayed answer"
    );
    let prepared = session
        .prepare_rename(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("the stale rename response is settled and discarded");
    assert!(prepared.is_some());
    session.shutdown().await;
}

#[tokio::test]
async fn notification_write_to_a_non_reading_engine_times_out() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut deaf = launch("deaf");
    deaf.request_timeout = Duration::from_millis(300);
    let mut session = EngineSession::start(deaf, workspace.path())
        .await
        .expect("the handshake completes before the engine goes deaf");
    let error = session
        .open(&path("src/lib.rs"), "rust", "x".repeat(1 << 20))
        .await
        .expect_err("the full pipe must not stall the session");
    assert!(matches!(error.fault(), EngineFault::TimedOut { .. }));
    session.shutdown().await;
}

#[tokio::test]
async fn engine_death_between_operations_closes_the_connection() {
    let (_workspace, mut session) = started("exits-after-start").await;
    let document = path("src/lib.rs");
    let mut refusal = None;
    for _ in 0..1_000 {
        match session.open(&document, "rust", "probe".to_owned()).await {
            Ok(()) => {}
            Err(error) => {
                refusal = Some(error);
                break;
            }
        }
    }
    let error = refusal.expect("the dead engine's pipe refuses within the bound");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. }
    ));
    session.shutdown().await;
}

#[tokio::test]
async fn lingering_engine_is_killed_at_the_shutdown_timeout() {
    let (_workspace, session) = started("lingering").await;
    let started_at = Instant::now();
    session.shutdown().await;
    let elapsed = started_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "the shutdown kill must not wait out the engine: {elapsed:?}"
    );
}

#[tokio::test]
async fn workspace_root_without_a_name_still_initializes() {
    let session = EngineSession::start(launch("happy"), Path::new("/"))
        .await
        .expect("the filesystem root serves as a workspace root");
    session.shutdown().await;
}

#[tokio::test]
async fn engine_inherits_the_server_environment() {
    let (_workspace, session) = started("environment").await;
    let stderr = session.shutdown().await;
    let home = std::env::var("HOME").expect("HOME is set in the test environment");
    assert!(
        stderr.text.contains(&format!("HOME={home}")),
        "the engine must see the inherited HOME: {}",
        stderr.text
    );
}

#[tokio::test]
async fn overlay_entries_win_over_the_inherited_environment() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut overlaid = launch("environment");
    overlaid
        .environment
        .insert("HOME".to_owned(), "/rift/overlay".to_owned());
    overlaid
        .environment
        .insert("RIFT_ENGINE_PROBE".to_owned(), "42".to_owned());
    let session = EngineSession::start(overlaid, workspace.path())
        .await
        .expect("the fake engine starts");
    let stderr = session.shutdown().await;
    assert!(
        stderr.text.contains("HOME=/rift/overlay"),
        "{}",
        stderr.text
    );
    assert!(
        stderr.text.contains("RIFT_ENGINE_PROBE=42"),
        "{}",
        stderr.text
    );
}

#[tokio::test]
async fn empty_and_absolute_programs_are_refused_before_spawning() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut empty = launch("happy");
    empty.program = String::new();
    let refused = EngineSession::start(empty, workspace.path())
        .await
        .expect_err("an empty program is refused");
    assert!(matches!(refused.fault(), EngineFault::ProgramEmpty));
    let mut absolute = launch("happy");
    absolute.program = FAKE_ENGINE.to_owned();
    let refused = EngineSession::start(absolute, workspace.path())
        .await
        .expect_err("an absolute executable path is refused");
    assert!(matches!(
        refused.fault(),
        EngineFault::ProgramAbsolute { program } if program == FAKE_ENGINE
    ));
    assert_eq!(
        refused.name(),
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    );
}

#[tokio::test]
async fn initialization_options_reach_the_engine_at_initialize() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut with_options = launch("initialization-options");
    with_options.initialization_options = Some(serde_json::json!({ "engine": "fake" }));
    let mut session = EngineSession::start(with_options, workspace.path())
        .await
        .expect("the engine accepts its initialization options");
    let document = path("src/lib.rs");
    session
        .rename(
            &document,
            Position {
                line: 0,
                character: 3,
            },
            "renamed",
        )
        .await
        .expect("the engine serves after verifying its options");
    session.shutdown().await;
}

#[tokio::test]
async fn absent_initialization_options_fail_the_asserting_engine() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start(launch("initialization-options"), workspace.path())
        .await
        .expect_err("the engine dies when its options are missing");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. } | EngineFault::Framing { .. }
    ));
}

#[tokio::test]
async fn missing_program_fails_the_launch_with_a_typed_error() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut missing = launch("happy");
    missing.program = "rift-engine-that-does-not-exist".to_owned();
    let error = EngineSession::start(missing, workspace.path())
        .await
        .expect_err("the program cannot be found");
    assert!(matches!(error.fault(), EngineFault::LaunchFailed { .. }));
}

/// The word-renaming behavior proposes word-boundary edits across the
/// root's `.rs` files: the opened target from its `didOpen` bytes, every
/// other file from disk. The graceful shutdown lets the engine flush.
#[tokio::test]
async fn word_renaming_engine_proposes_edits_across_files() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let library = "pub fn beacon() {}\n";
    // `beacons` fails the word boundary, so this file gains exactly one edit.
    let caller = "pub fn caller() { beacon(); } // beacons\n";
    std::fs::write(workspace.path().join("lib.rs"), library).expect("fixture writes");
    std::fs::write(workspace.path().join("main.rs"), caller).expect("fixture writes");
    std::fs::write(workspace.path().join("notes.md"), "beacon\n").expect("fixture writes");
    // The scan skips hidden directories, recurses to its depth bound, and
    // survives an unreadable directory.
    std::fs::create_dir(workspace.path().join(".hidden")).expect("fixture directory creates");
    std::fs::write(workspace.path().join(".hidden/skipped.rs"), "beacon\n")
        .expect("fixture writes");
    let deep = workspace.path().join("a/b/c/d");
    std::fs::create_dir_all(&deep).expect("fixture directory creates");
    std::fs::write(deep.join("too_deep.rs"), "beacon\n").expect("fixture writes");
    let sealed = workspace.path().join("sealed");
    std::fs::create_dir(&sealed).expect("fixture directory creates");
    std::fs::set_permissions(&sealed, std::os::unix::fs::PermissionsExt::from_mode(0o000))
        .expect("fixture permissions set");
    let mut session = EngineSession::start(launch("renames-word"), workspace.path())
        .await
        .expect("the fake engine starts");
    let target = path("lib.rs");
    session
        .open(&target, "rust", library.to_owned())
        .await
        .expect("didOpen is sent");
    let edit = session
        .rename(
            &target,
            Position {
                line: 0,
                character: 7,
            },
            "flare",
        )
        .await
        .expect("rename answers");
    let proposal = serde_json::to_value(&edit).expect("the edit serializes");
    let library_uri = session
        .root()
        .document_uri(&target)
        .expect("the target uri composes")
        .to_string();
    let caller_uri = session
        .root()
        .document_uri(&path("main.rs"))
        .expect("the caller uri composes")
        .to_string();
    assert_eq!(
        proposal["changes"][&library_uri][0]["newText"],
        serde_json::json!("flare")
    );
    assert_eq!(
        proposal["changes"][&caller_uri][0]["range"]["start"]["character"],
        serde_json::json!(18)
    );
    assert_eq!(
        proposal["changes"]
            .as_object()
            .expect("changes is a map")
            .len(),
        2,
        "the markdown file, the hidden directory, and the too-deep file are never \
         proposed: {proposal:#}"
    );
    assert_eq!(
        proposal["changes"][&caller_uri]
            .as_array()
            .expect("the caller's edits are a list")
            .len(),
        1,
        "`beacons` fails the word boundary: {proposal:#}"
    );
    session.shutdown().await;
    std::fs::set_permissions(&sealed, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("fixture permissions restore");
}

/// The mutating behavior drifts the target on disk before answering, and
/// its answer still derives from the opened bytes.
#[tokio::test]
async fn mutating_engine_drifts_the_target_and_answers_from_opened_bytes() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let library = "pub fn beacon() {}\n";
    std::fs::write(workspace.path().join("lib.rs"), library).expect("fixture writes");
    let mut session = EngineSession::start(launch("mutates-then-renames"), workspace.path())
        .await
        .expect("the fake engine starts");
    let target = path("lib.rs");
    session
        .open(&target, "rust", library.to_owned())
        .await
        .expect("didOpen is sent");
    let edit = session
        .rename(
            &target,
            Position {
                line: 0,
                character: 7,
            },
            "flare",
        )
        .await
        .expect("rename answers");
    let drifted =
        std::fs::read_to_string(workspace.path().join("lib.rs")).expect("the target reads");
    assert!(
        drifted.contains("the engine drifted this file"),
        "the engine mutated its target: {drifted}"
    );
    let proposal = serde_json::to_value(&edit).expect("the edit serializes");
    let library_uri = session
        .root()
        .document_uri(&target)
        .expect("the target uri composes")
        .to_string();
    assert_eq!(
        proposal["changes"][&library_uri][0]["range"]["start"]["character"],
        serde_json::json!(7),
        "the edits derive from the opened bytes, not the drifted disk"
    );
    session.shutdown().await;
}

/// The outside-root behavior answers its fixed escape URI verbatim.
#[tokio::test]
async fn outside_root_engine_answers_its_escape_uri() {
    let (_workspace, mut session) = started("renames-outside-root").await;
    let target = path("lib.rs");
    session
        .open(&target, "rust", "pub fn beacon() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    let edit = session
        .rename(
            &target,
            Position {
                line: 0,
                character: 7,
            },
            "flare",
        )
        .await
        .expect("rename answers");
    let proposal = serde_json::to_value(&edit).expect("the edit serializes");
    assert!(
        proposal["changes"]["file:///rift-elsewhere/out.rs"].is_array(),
        "the escape URI rides the answer: {proposal:#}"
    );
    session.shutdown().await;
}

/// The session reads the engine's `$/progress` traffic and answers whether
/// work is outstanding.
///
/// The engine mints its token and begins work right after initialize, so
/// nothing is outstanding until a later exchange pumps those messages: the
/// first pull reads the create request, answers it, reads the begin, and
/// then answers with no items - the shape a loading engine produces. The
/// `didOpen` before it adds a report, and the rename ends the token, so
/// the session reads as analyzing between them and settled after.
#[tokio::test]
async fn work_done_progress_decides_whether_the_engine_is_analyzing() {
    let (_workspace, mut session) = started("reports-progress").await;
    let document = path("src/lib.rs");
    assert!(
        !session.is_analyzing(),
        "a session that has read no progress is never analyzing"
    );

    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    let pulled = session
        .pull_diagnostics(&document)
        .await
        .expect("the loading engine answers its pull");
    assert!(pulled.is_empty(), "a loading engine reports nothing yet");
    assert!(
        session.is_analyzing(),
        "the begin and report the pull consumed leave the token outstanding"
    );

    session
        .rename(
            &document,
            Position {
                line: 0,
                character: 3,
            },
            "renamed",
        )
        .await
        .expect("the rename answers");
    assert!(
        !session.is_analyzing(),
        "the end the rename consumed retires the token"
    );
    session.shutdown().await;
}

/// The prepare behaviors answer a typed decline and a typed refusal, and
/// both leave the engine serving.
#[tokio::test]
async fn prepare_behaviors_decline_and_refuse_with_typed_answers() {
    let (_workspace, mut session) = started("declines-prepare").await;
    let target = path("lib.rs");
    let position = Position {
        line: 0,
        character: 0,
    };
    let declined = session
        .prepare_rename(&target, position)
        .await
        .expect("prepare answers");
    assert!(declined.is_none(), "a null prepare answer declines");
    session.shutdown().await;

    let (_workspace, mut session) = started("refuses-prepare").await;
    let refused = session
        .prepare_rename(&target, position)
        .await
        .expect_err("the engine refuses the prepare");
    assert!(matches!(
        refused.fault(),
        EngineFault::Refused { message, .. } if message == "cannot rename here"
    ));
    session.shutdown().await;
}

/// An engine that never ends its progress keeps reading as analyzing, so
/// every answer it gives stays provisional.
#[tokio::test]
async fn progress_that_never_ends_keeps_the_engine_analyzing() {
    let (_workspace, mut session) = started("never-ends-progress").await;
    let document = path("src/lib.rs");
    for _pull in 0..3 {
        let pulled = session
            .pull_diagnostics(&document)
            .await
            .expect("the loading engine answers its pull");
        assert!(pulled.is_empty());
        assert!(session.is_analyzing());
    }
    session.shutdown().await;
}
