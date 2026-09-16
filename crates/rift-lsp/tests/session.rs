//! Integration tests: the session over an in-process transport and over a
//! real minimal process.
//!
//! Exchange-level scenarios - capability negotiation, refusals,
//! correlation, progress, and the empty-answer resend policy - run
//! `EngineSession::start_over_transport` against a scripted engine sharing
//! the other half of a `tokio::io::duplex` pair: the test owns both ends of
//! the byte stream and no process is involved (`exchange.rs`).
//! Process-lifecycle scenarios - a startup that never answers, a shutdown
//! that never ends, a connection that closes mid-request - keep a real `sh`
//! process whose only behavior is answering, or refusing to answer, one
//! fixed `initialize` response (`process_lifecycle.rs`).
//!
//! Byte-level framing misbehavior (garbage bytes, an oversized
//! announcement) is proven directly against `framing::Framing` in that
//! module's own unit tests; the two tests here prove only that the session
//! turns such a framing refusal into the right `EngineFault`, writing the
//! misbehaving bytes straight onto the duplex with no scripted engine
//! behind them.

#![cfg(unix)]

mod exchange;
mod process_lifecycle;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use exchange::{ScriptedEngine, full_capabilities, zero_range};
use lsp_types::{FileChangeType, Position};
use rift_core::{ErrorCode, ErrorName, ProjectPath};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::framing::FramingFault;
use rift_lsp::session::{EngineError, EngineFault, EngineLaunch, EngineReadiness, EngineSession};
use rift_lsp::uri::TreeRoot;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, DuplexStream};

/// One JSON-RPC diagnostic with the given message.
fn diagnostic(message: &str) -> Value {
    json!({"range": zero_range(), "message": message})
}

fn path(value: &str) -> ProjectPath {
    ProjectPath::new(value).expect("fixture path is valid")
}

/// A launch for `start_over_transport`: `program`, `arguments`, and
/// `environment` are never read over a transport that is already
/// connected.
fn transport_launch() -> EngineLaunch {
    EngineLaunch {
        program: String::new(),
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        initialization_options: None,
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        stderr_capture_bytes: 4_096,
    }
}

/// Starts a session over one half of a fresh duplex pair, handing the other
/// half to `script`. Returns the workspace, the start result, and the
/// script's join handle, which the caller awaits after driving its own
/// assertions.
async fn begin<F, Fut>(
    launch: EngineLaunch,
    script: F,
) -> (
    tempfile::TempDir,
    Result<EngineSession, EngineError>,
    tokio::task::JoinHandle<()>,
)
where
    F: FnOnce(ScriptedEngine<DuplexStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (client, engine_side) = tokio::io::duplex(64 * 1024);
    let engine_task = tokio::spawn(script(ScriptedEngine::new(engine_side)));
    let workspace = tempfile::tempdir().expect("tempdir");
    let result =
        EngineSession::start_over_transport(launch, workspace.path(), client, tokio::io::empty())
            .await;
    (workspace, result, engine_task)
}

/// [`begin`] with the shared transport launch, asserting the handshake
/// succeeds.
async fn started<F, Fut>(
    script: F,
) -> (
    tempfile::TempDir,
    EngineSession,
    tokio::task::JoinHandle<()>,
)
where
    F: FnOnce(ScriptedEngine<DuplexStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (workspace, result, engine_task) = begin(transport_launch(), script).await;
    (
        workspace,
        result.expect("the scripted engine completes the handshake"),
        engine_task,
    )
}

/// Joins the script task, propagating a panic inside the script as a panic
/// in the test.
async fn join(engine_task: tokio::task::JoinHandle<()>) {
    engine_task.await.expect("the scripted engine task joins");
}

// ---------------------------------------------------------------------
// Exchange-level: capability negotiation, requests, and refusals.
// ---------------------------------------------------------------------

/// The happy-path script: negotiates the full capability grid, publishes
/// diagnostics for the opened document and one outside the root, answers
/// every operation the flagship test drives, then shuts down cleanly.
async fn happy_engine_script(mut engine: ScriptedEngine<DuplexStream>) {
    engine.handshake(full_capabilities()).await;
    let opened = engine.next_message().await;
    assert_eq!(opened["method"], json!("textDocument/didOpen"));
    let document_uri = opened["params"]["textDocument"]["uri"]
        .as_str()
        .expect("uri")
        .to_owned();
    engine
        .notify(
            "textDocument/publishDiagnostics",
            json!({
                "uri": document_uri,
                "diagnostics": [diagnostic("published diagnostic")],
            }),
        )
        .await;
    engine
        .notify(
            "textDocument/publishDiagnostics",
            json!({"uri": "file:///rift-elsewhere/out.rs", "diagnostics": []}),
        )
        .await;
    let (id, params) = engine.expect_request("textDocument/references").await;
    assert_eq!(
        params["context"]["includeDeclaration"],
        json!(true),
        "the reference request asks the engine to name the declaration itself"
    );
    engine.respond(&id, json!([])).await;
    let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
    engine
        .respond(
            &id,
            json!({"kind": "full", "items": [diagnostic("pulled diagnostic")]}),
        )
        .await;
    let closed = engine.next_message().await;
    assert_eq!(closed["method"], json!("textDocument/didClose"));
    let (id, _params) = engine.expect_request("shutdown").await;
    engine.respond(&id, Value::Null).await;
    let ended = engine.next_message().await;
    assert_eq!(ended["method"], json!("exit"));
}

#[tokio::test]
async fn happy_engine_negotiates_references_and_serves_diagnostics() {
    let (workspace, mut session, engine_task) = started(happy_engine_script).await;

    let record = session.capabilities();
    assert_eq!(record.position_encoding, PositionEncoding::Utf8);
    assert!(record.references);
    assert!(record.pull_diagnostics);
    assert_eq!(record.diagnostic_identifier.as_deref(), Some("scripted"));
    assert_eq!(
        session.root(),
        &TreeRoot::new(workspace.path()).expect("the root converts")
    );

    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    let references = session
        .references(
            &document,
            Position {
                line: 0,
                character: 3,
            },
        )
        .await
        .expect("references answers");
    assert!(
        references.is_empty(),
        "the scripted engine names no location, the declaration's own included"
    );

    assert!(
        session.published_diagnostics(&path("out.rs")).is_none(),
        "a publish outside the root is never retained"
    );

    let published = session
        .published_diagnostics(&document)
        .expect("the didOpen publish was retained");
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].message, "published diagnostic");
    assert_eq!(
        session.published_diagnostics_version(&document),
        None,
        "the scripted publish names no version, so none is retained"
    );

    let pulled = session
        .pull_diagnostics(&document)
        .await
        .expect("diagnostic pull answers");
    assert_eq!(pulled.len(), 1);
    assert_eq!(pulled[0].message, "pulled diagnostic");

    session.close(&document).await.expect("didClose is sent");
    let stderr = session.shutdown().await;
    assert_eq!(stderr.total_bytes, 0);
    join(engine_task).await;
}

/// A publish naming a version is retained under it, and a later publish
/// for the same document overwrites both the diagnostics and the version.
#[tokio::test]
async fn published_diagnostics_version_tracks_the_latest_publish() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let opened = engine.next_message().await;
        assert_eq!(opened["method"], json!("textDocument/didOpen"));
        let uri = opened["params"]["textDocument"]["uri"]
            .as_str()
            .expect("uri")
            .to_owned();
        engine
            .notify(
                "textDocument/publishDiagnostics",
                json!({"uri": uri, "diagnostics": [diagnostic("first")], "version": 3}),
            )
            .await;
        let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
        engine
            .respond(&id, json!({"kind": "full", "items": []}))
            .await;
        engine
            .notify(
                "textDocument/publishDiagnostics",
                json!({"uri": uri, "diagnostics": [diagnostic("second")], "version": 7}),
            )
            .await;
        let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
        engine
            .respond(&id, json!({"kind": "full", "items": []}))
            .await;
    })
    .await;

    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");

    session
        .pull_diagnostics(&document)
        .await
        .expect("the first pull answers, reading the queued publish along the way");
    assert_eq!(session.published_diagnostics_version(&document), Some(3));

    session
        .pull_diagnostics(&document)
        .await
        .expect("the second pull answers, reading the newer publish along the way");
    assert_eq!(
        session.published_diagnostics_version(&document),
        Some(7),
        "a later publish overwrites the version its predecessor named"
    );
    assert_eq!(
        session
            .published_diagnostics(&document)
            .expect("the second publish was retained")[0]
            .message,
        "second"
    );

    join(engine_task).await;
}

#[tokio::test]
async fn engine_without_capabilities_gets_typed_refusals_before_any_request() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(json!({})).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        let exit = engine.next_message().await;
        assert_eq!(exit["method"], json!("exit"));
    })
    .await;
    assert_eq!(
        session.capabilities().position_encoding,
        PositionEncoding::Utf16
    );
    let document = path("src/lib.rs");
    let refusal = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("request was never advertised");
    assert!(matches!(
        refusal.fault(),
        EngineFault::CapabilityAbsent { capability } if capability == "textDocument/references"
    ));
    assert_eq!(
        refusal.name(),
        ErrorName::Wire(ErrorCode::CapabilityUnavailable)
    );
    for absent in [
        session.pull_diagnostics(&document).await.err(),
        session
            .references(
                &document,
                Position {
                    line: 0,
                    character: 0,
                },
            )
            .await
            .err(),
    ] {
        let error = absent.expect("the capability gate refuses");
        assert!(matches!(
            error.fault(),
            EngineFault::CapabilityAbsent { .. }
        ));
    }
    session.shutdown().await;
    join(engine_task).await;
}

#[tokio::test]
async fn unoffered_position_encoding_fails_the_start() {
    let (_workspace, result, engine_task) = begin(transport_launch(), |mut engine| async move {
        let (id, _params) = engine.expect_request("initialize").await;
        engine
            .respond(&id, json!({"capabilities": {"positionEncoding": "utf-32"}}))
            .await;
    })
    .await;
    let error = result.expect_err("utf-32 was never offered");
    assert!(matches!(error.fault(), EngineFault::Negotiation { .. }));
    assert_eq!(
        error.name(),
        ErrorName::Wire(ErrorCode::CapabilityUnavailable)
    );
    join(engine_task).await;
}

#[tokio::test]
async fn garbage_bytes_fail_the_start_with_a_framing_fault() {
    let (client, mut engine_side) = tokio::io::duplex(64 * 1024);
    let engine_task = tokio::spawn(async move {
        engine_side
            .write_all(b"these bytes are not a base-protocol frame\r\n\r\n")
            .await
            .expect("the duplex writes");
    });
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start_over_transport(
        transport_launch(),
        workspace.path(),
        client,
        tokio::io::empty(),
    )
    .await
    .expect_err("garbage is refused");
    assert!(matches!(error.fault(), EngineFault::Framing { .. }));
    join(engine_task).await;
}

#[tokio::test]
async fn oversized_announcement_fails_the_start_as_limit_exceeded() {
    let (client, mut engine_side) = tokio::io::duplex(64 * 1024);
    let announced = rift_lsp::framing::MESSAGE_BYTES_MAX + 1;
    let engine_task = tokio::spawn(async move {
        engine_side
            .write_all(format!("Content-Length: {announced}\r\n\r\n").as_bytes())
            .await
            .expect("the duplex writes");
    });
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start_over_transport(
        transport_launch(),
        workspace.path(),
        client,
        tokio::io::empty(),
    )
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
    join(engine_task).await;
}

#[tokio::test]
async fn result_outside_the_method_shape_is_typed_and_non_fatal() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.respond(&id, json!(42)).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .respond(
                &id,
                json!([{"uri": "file:///lib.rs", "range": zero_range()}]),
            )
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let error = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("a number is not a location list");
    assert!(matches!(error.fault(), EngineFault::ResultInvalid { .. }));
    session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("an invalid result leaves the engine serving");
    session.shutdown().await;
    join(engine_task).await;
}

#[tokio::test]
async fn refused_references_request_is_typed_and_leaves_the_session_serving() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .refuse(&id, -32602, "position is outside the document")
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .respond(
                &id,
                json!([{"uri": "file:///lib.rs", "range": zero_range()}]),
            )
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let refusal = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("the engine refuses the position");
    assert!(matches!(
        refusal.fault(),
        EngineFault::Refused { code: -32602, message, .. } if message == "position is outside the document"
    ));
    assert_eq!(refusal.name(), ErrorName::Wire(ErrorCode::InvalidRequest));
    session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("a refusal leaves the engine serving");
    session.shutdown().await;
    join(engine_task).await;
}

/// A server cancellation is a refusal the caller may resend: the typed
/// fault carries the code, classifies as temporarily unavailable, and
/// leaves the engine serving the next request.
#[tokio::test]
async fn cancelled_references_request_is_a_retryable_refusal() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .refuse(&id, -32802, "server cancelled the request")
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .respond(
                &id,
                json!([{"uri": "file:///lib.rs", "range": zero_range()}]),
            )
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let refusal = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
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
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("a cancellation leaves the engine serving");
    session.shutdown().await;
    join(engine_task).await;
}

#[tokio::test]
async fn payload_without_an_envelope_ends_the_session() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (_id, _params) = engine.expect_request("textDocument/references").await;
        engine.send(&json!({"jsonrpc": "2.0"})).await;
    })
    .await;
    let error = session
        .references(
            &path("src/lib.rs"),
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("the payload fits no envelope");
    assert!(matches!(error.fault(), EngineFault::MessageUnreadable));
    session.shutdown().await;
    join(engine_task).await;
}

/// The engine delays its answer past the caller's own cancellation, and a
/// later request on the same session settles and discards it.
#[tokio::test]
async fn cancelled_request_response_is_discarded_by_the_next_call() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        engine.respond(&id, json!([])).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .respond(
                &id,
                json!([{"uri": "file:///lib.rs", "range": zero_range()}]),
            )
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let cancelled = tokio::time::timeout(
        Duration::from_millis(50),
        session.references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        ),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "the caller cancels before the delayed answer"
    );
    let prepared = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("the stale request response is settled and discarded");
    assert!(!prepared.is_empty());
    session.shutdown().await;
    join(engine_task).await;
}

/// Five server-initiated requests, each answered per the routing policy:
/// configuration, registration, progress creation, diagnostic refresh, and an unserved probe.
/// The request answers only after every one of them settles.
#[tokio::test]
async fn server_initiated_requests_are_routed_before_the_response() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let opened = engine.next_message().await;
        assert_eq!(opened["method"], json!("textDocument/didOpen"));
        let (request_id, params) = engine.expect_request("textDocument/references").await;
        let uri = params["textDocument"]["uri"]
            .as_str()
            .expect("uri")
            .to_owned();

        engine
            .send(&json!({
                "jsonrpc": "2.0", "id": 90, "method": "workspace/configuration",
                "params": {"items": [{}, {}]},
            }))
            .await;
        let configuration = engine.next_raw_message().await;
        assert_eq!(configuration["result"], json!([null, null]));
        engine
            .send(&json!({
                "jsonrpc": "2.0", "id": 91, "method": "client/registerCapability",
                "params": {"registrations": []},
            }))
            .await;
        let registered = engine.next_raw_message().await;
        assert_eq!(registered["result"], Value::Null);
        engine
            .send(&json!({
                "jsonrpc": "2.0", "id": 92, "method": "window/workDoneProgress/create",
                "params": {"token": "probe"},
            }))
            .await;
        let progress = engine.next_raw_message().await;
        assert_eq!(progress["result"], Value::Null);
        engine
            .send(&json!({
                "jsonrpc": "2.0", "id": 93, "method": "workspace/diagnostic/refresh",
                "params": null,
            }))
            .await;
        let refresh = engine.next_raw_message().await;
        assert_eq!(refresh["result"], Value::Null);
        engine
            .send(&json!({"jsonrpc": "2.0", "id": 94, "method": "engine/probe", "params": null}))
            .await;
        let unserved = engine.next_raw_message().await;
        assert_eq!(unserved["error"]["code"], json!(-32601));

        let sibling = format!("{uri}.sibling");
        engine
            .respond(
                &request_id,
                json!([{ "uri": uri, "range": zero_range() }, { "uri": sibling, "range": zero_range() }]),
            )
            .await;
        let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
        engine
            .respond(&id, json!({"kind": "unchanged", "resultId": "1"}))
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    // The scripted engine dies unless configuration, registration,
    // progress, refresh, and the unserved probe are each answered per the routing
    // policy before the request itself answers.
    let locations = session
        .references(
            &document,
            Position {
                line: 0,
                character: 3,
            },
        )
        .await
        .expect("the request answers after the server requests");
    assert_eq!(locations.len(), 2);
    assert_eq!(
        session.diagnostic_refresh_revision(),
        1,
        "diagnostic refresh invalidates earlier pull evidence"
    );
    let unchanged = session
        .pull_diagnostics(&document)
        .await
        .expect("an unchanged report answers");
    assert!(unchanged.is_empty(), "an unchanged report carries no items");
    session.shutdown().await;
    join(engine_task).await;
}

/// The standard-error drain runs concurrently with the exchange: a flood on
/// one stream never stalls a request answered on the other.
#[tokio::test]
async fn stderr_flood_is_drained_bounded_while_the_request_answers() {
    use tokio::io::AsyncReadExt as _;

    let (client, engine_side) = tokio::io::duplex(64 * 1024);
    let engine_task = tokio::spawn(async move {
        let mut engine = ScriptedEngine::new(engine_side);
        engine.handshake(full_capabilities()).await;
        let opened = engine.next_message().await;
        assert_eq!(opened["method"], json!("textDocument/didOpen"));
        let (id, _params) = engine.expect_request("textDocument/references").await;
        let location = json!({"uri": "file:///lib.rs", "range": zero_range()});
        engine.respond(&id, json!([location])).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    });
    let workspace = tempfile::tempdir().expect("tempdir");
    let flood = tokio::io::repeat(b'f').take(1 << 20);
    let mut session =
        EngineSession::start_over_transport(transport_launch(), workspace.path(), client, flood)
            .await
            .expect("the scripted engine completes the handshake");
    let document = path("src/lib.rs");
    session
        .open(&document, "rust", "fn a() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect("the request answers after the flood");
    let stderr = session.shutdown().await;
    assert_eq!(stderr.captured_bytes, 4_096);
    assert!(
        stderr.total_bytes >= 1 << 20,
        "total {}",
        stderr.total_bytes
    );
    assert!(stderr.truncated);
    join(engine_task).await;
}

/// The session reads the engine's `$/progress` traffic and answers whether
/// work is outstanding.
///
/// The engine mints its token and begins work right after the handshake, so
/// nothing is outstanding until a later exchange pumps those messages: the
/// first pull reads the create request, answers it, reads the begin, and
/// then answers with no items - the shape a loading engine produces. The
/// `didOpen` before it adds a report, and the request ends the token, so the
/// session reads as analyzing between them and settled after.
#[tokio::test]
async fn work_done_progress_decides_whether_the_engine_is_analyzing() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine.begin_progress().await;
        let opened = engine.next_message().await;
        assert_eq!(opened["method"], json!("textDocument/didOpen"));
        engine.report_progress().await;
        let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
        engine
            .respond(&id, json!({"kind": "full", "items": []}))
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.end_progress().await;
        let location = json!({"uri": "file:///lib.rs", "range": zero_range()});
        engine.respond(&id, json!([location])).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
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
        .references(
            &document,
            Position {
                line: 0,
                character: 3,
            },
        )
        .await
        .expect("the request answers");
    assert!(
        !session.is_analyzing(),
        "the end the request consumed retires the token"
    );
    session.shutdown().await;
    join(engine_task).await;
}

/// An engine that never ends its progress keeps reading as analyzing, so
/// every answer it gives stays provisional.
#[tokio::test]
async fn progress_that_never_ends_keeps_the_engine_analyzing() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine.begin_progress().await;
        for _probe in 0..3 {
            let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
            engine
                .respond(&id, json!({"kind": "full", "items": []}))
                .await;
        }
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
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
    join(engine_task).await;
}

/// Diagnostic reports use their own settlement record. They do not set
/// the semantic empty-answer state.
#[tokio::test]
async fn an_announced_engine_diagnostic_does_not_set_semantic_empty_answer() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine.begin_progress().await;
        let (id, _params) = engine.expect_request("textDocument/diagnostic").await;
        engine
            .respond(&id, json!({"kind": "full", "items": []}))
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let pulled = session
        .pull_diagnostics(&document)
        .await
        .expect("the loading engine answers its pull");
    assert!(pulled.is_empty(), "a loading engine reports nothing yet");
    assert_eq!(
        session.readiness(),
        EngineReadiness::Analyzing,
        "the begin the pull consumed announces work still outstanding"
    );
    assert!(
        !session.latest_answer_is_empty(),
        "diagnostic settlement owns this empty report"
    );
    session.shutdown().await;
    join(engine_task).await;
}

/// An engine that has announced no work of its own answers `references`
/// with an empty list - the answer of an engine with nothing pointing at
/// the declaration, and of one that has not indexed the file yet.
///
/// Every empty references answer remains unconfirmed until engine
/// announces work. Engine slot retry policy supplies bound.
#[tokio::test]
async fn an_empty_references_answer_from_an_unannounced_engine_stays_unconfirmed() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        for _attempt in 0..2 {
            let (id, _params) = engine.expect_request("textDocument/references").await;
            engine.respond(&id, json!([])).await;
        }
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    assert_eq!(session.readiness(), EngineReadiness::Unconfirmed);

    let first = session
        .references(&document, position)
        .await
        .expect("references answers");
    assert!(first.is_empty(), "the engine names nothing");
    assert!(
        session.latest_answer_is_empty(),
        "empty answer from unconfirmed engine remains provisional"
    );

    let second = session
        .references(&document, position)
        .await
        .expect("references answers again");
    assert!(second.is_empty());
    assert!(
        session.latest_answer_is_empty(),
        "configured retry policy, not session state, bounds resends"
    );
    session.shutdown().await;
    join(engine_task).await;
}

/// Readiness moves through its three states in the order the protocol
/// traffic proves them: unconfirmed before any progress, analyzing while a
/// token is outstanding, ready once every announced token has ended.
#[tokio::test]
async fn readiness_moves_from_unconfirmed_through_analyzing_to_ready() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.begin_progress().await;
        engine.respond(&id, json!([])).await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.end_progress().await;
        engine.respond(&id, json!([])).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    assert_eq!(
        session.readiness(),
        EngineReadiness::Unconfirmed,
        "no progress has been read yet"
    );

    session
        .references(&document, position)
        .await
        .expect("references answers");
    assert_eq!(
        session.readiness(),
        EngineReadiness::Analyzing,
        "the begin this exchange read leaves work outstanding"
    );

    session
        .references(&document, position)
        .await
        .expect("references answers again");
    assert_eq!(
        session.readiness(),
        EngineReadiness::Ready,
        "the end this exchange read retires the only outstanding token"
    );

    session
        .notify_changed_paths(&[(document.clone(), FileChangeType::CHANGED)])
        .await
        .expect("an unregistered path needs no notification");
    assert_eq!(
        session.readiness(),
        EngineReadiness::Unconfirmed,
        "changed workspace bytes invalidate earlier readiness"
    );

    session.shutdown().await;
    join(engine_task).await;
}

/// A provisional answer - given while work is still outstanding - is
/// retried until the engine settles: a caller that discards every answer
/// read while `is_analyzing()` holds and keeps asking converges on the
/// engine's real verdict, not the provisional one.
///
/// `EngineSession` records only whether the engine was analyzing; the
/// resend loop is the caller's, exactly as `EngineSlot::request` runs it.
/// This proves the primitive that loop retries on.
#[tokio::test]
async fn a_provisional_answer_is_retried_until_the_engine_settles() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine.begin_progress().await;
        for _provisional in 0..2 {
            let (id, _params) = engine.expect_request("textDocument/references").await;
            engine
                .respond(
                    &id,
                    json!([{"uri": "file:///workspace/lib.rs", "range": zero_range()}]),
                )
                .await;
        }
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.end_progress().await;
        engine.respond(&id, json!([])).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    let mut settled = None;
    for _attempt in 0..3 {
        let answer = session
            .references(&document, position)
            .await
            .expect("references answers");
        if !session.is_analyzing() {
            settled = Some(answer);
            break;
        }
    }
    let settled = settled.expect("the engine settles inside the attempt bound");
    assert!(
        settled.is_empty(),
        "the settled answer must be the engine's real verdict, not one of the provisional \
         locations it answered while analyzing: {settled:#?}"
    );
    session.shutdown().await;
    join(engine_task).await;
}

/// An engine whose progress never ends stays analyzing forever. A caller
/// that gives up after its own attempt bound reports the budget it spent -
/// the shape `EngineSlot::request` reports through
/// `EngineFault::Analyzing` once its own retry table is exhausted.
#[tokio::test]
async fn an_engine_that_never_settles_lets_a_caller_report_its_spent_budget() {
    const ATTEMPTS_MAX: u64 = 3;
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine.begin_progress().await;
        for _attempt in 0..ATTEMPTS_MAX {
            let (id, _params) = engine.expect_request("textDocument/references").await;
            engine.respond(&id, json!([])).await;
        }
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    for _attempt in 1..=ATTEMPTS_MAX {
        session
            .references(&document, position)
            .await
            .expect("references answers");
        assert!(
            session.is_analyzing(),
            "the engine never ends the progress it began"
        );
    }
    let exhausted = EngineError::new(EngineFault::Analyzing {
        attempts: ATTEMPTS_MAX,
    });
    assert_eq!(
        exhausted.name(),
        ErrorName::Wire(ErrorCode::TemporarilyUnavailable)
    );
    assert!(
        exhausted
            .to_string()
            .contains(&format!("attempts {ATTEMPTS_MAX}")),
        "{exhausted}"
    );
    session.shutdown().await;
    join(engine_task).await;
}

/// Each registered watcher receives its requested create, change, or delete event.
#[tokio::test]
async fn notify_changed_paths_matches_only_the_engines_registered_watchers() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        engine
            .send(&json!({
                "jsonrpc": "2.0",
                "id": "register-watched-files",
                "method": "client/registerCapability",
                "params": {
                    "registrations": [{
                        "id": "watch-rust",
                        "method": "workspace/didChangeWatchedFiles",
                        "registerOptions": {
                            "watchers": [
                                {"globPattern": "**/*.rs", "kind": 1},
                                {"globPattern": "**/*.rs", "kind": 2},
                                {"globPattern": "**/*.rs", "kind": 4}
                            ]
                        }
                    }]
                }
            }))
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.respond(&id, json!([])).await;
        let notified = engine.next_message().await;
        assert_eq!(
            notified["method"],
            json!("workspace/didChangeWatchedFiles"),
            "{notified:#}"
        );
        let changes = notified["params"]["changes"]
            .as_array()
            .expect("changes is an array");
        assert_eq!(changes.len(), 3, "{notified:#}");
        assert_eq!(changes[0]["type"], json!(1), "created: {notified:#}");
        assert_eq!(changes[1]["type"], json!(2), "changed: {notified:#}");
        assert_eq!(changes[2]["type"], json!(3), "deleted: {notified:#}");
        assert!(
            changes[0]["uri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("new.rs")
                && changes[1]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .ends_with("lib.rs")
                && changes[2]["uri"]
                    .as_str()
                    .unwrap_or_default()
                    .ends_with("old.rs"),
            "{notified:#}"
        );
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let position = Position {
        line: 0,
        character: 3,
    };
    session
        .references(&path("src/lib.rs"), position)
        .await
        .expect("references answers");

    let changed = [
        (path("src/new.rs"), lsp_types::FileChangeType::CREATED),
        (path("src/lib.rs"), lsp_types::FileChangeType::CHANGED),
        (path("src/old.rs"), lsp_types::FileChangeType::DELETED),
        (path("README.md"), lsp_types::FileChangeType::CREATED),
    ];
    let matched = session
        .notify_changed_paths(&changed)
        .await
        .expect("the notification sends");
    assert_eq!(
        matched,
        vec![path("src/new.rs"), path("src/lib.rs"), path("src/old.rs")],
        "the engine receives one ordered batch filtered by path and event kind"
    );

    session.shutdown().await;
    join(engine_task).await;
}

/// A `client/registerCapability` call the session cannot read as a
/// registration - malformed top-level params, a registration for a
/// different method, one missing `registerOptions`, or one whose
/// `registerOptions` will not parse as
/// `DidChangeWatchedFilesRegistrationOptions` - contributes no watcher, and
/// the session still answers every one of them: a malformed registration
/// costs the engine no capability it would otherwise have. A later valid
/// registration in the same batch is unaffected, proving each bad entry is
/// skipped rather than aborting the whole registration.
#[tokio::test]
async fn malformed_watched_file_registrations_are_skipped_without_losing_the_valid_one() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        // Top-level params that will not parse as `RegistrationParams` at
        // all: `registrations` must be an array.
        engine
            .send(&json!({
                "jsonrpc": "2.0",
                "id": "unparsable-params",
                "method": "client/registerCapability",
                "params": {"registrations": "not-an-array"},
            }))
            .await;
        // One well-formed batch mixing three bad entries with one valid
        // one, in order: wrong method, missing `registerOptions`,
        // unparsable `registerOptions`, then a real watcher.
        engine
            .send(&json!({
                "jsonrpc": "2.0",
                "id": "mixed-batch",
                "method": "client/registerCapability",
                "params": {
                    "registrations": [
                        {
                            "id": "wrong-method",
                            "method": "workspace/didChangeConfiguration",
                        },
                        {
                            "id": "missing-options",
                            "method": "workspace/didChangeWatchedFiles",
                        },
                        {
                            "id": "bad-options",
                            "method": "workspace/didChangeWatchedFiles",
                            "registerOptions": {"watchers": "not-an-array"},
                        },
                        {
                            "id": "good",
                            "method": "workspace/didChangeWatchedFiles",
                            "registerOptions": {
                                "watchers": [{"globPattern": "**/*.rs"}],
                            },
                        },
                    ],
                },
            }))
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.respond(&id, json!([])).await;
        let notified = engine.next_message().await;
        assert_eq!(
            notified["method"],
            json!("workspace/didChangeWatchedFiles"),
            "{notified:#}"
        );
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    // Pumps the exchange loop that reads both registration calls before
    // the notification is sent below.
    session
        .references(&document, position)
        .await
        .expect("references answers");

    let matched = session
        .notify_changed_paths(&[
            (document.clone(), lsp_types::FileChangeType::CHANGED),
            (path("src/notes.md"), lsp_types::FileChangeType::CHANGED),
        ])
        .await
        .expect("the notification sends");
    assert_eq!(
        matched,
        vec![document],
        "only the path the surviving valid watcher covers is matched"
    );

    session.shutdown().await;
    join(engine_task).await;
}

/// A watched-file registration past the session's retained-watcher bound
/// (64) is dropped: the record already holds enough patterns to match
/// against, so a batch registering 65 watchers keeps only the first 64 and
/// drops the rest, whatever glob they name.
#[tokio::test]
async fn watched_file_watchers_past_the_bound_are_dropped() {
    let watchers_max = 64_usize;
    let (_workspace, mut session, engine_task) = started(move |mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let mut watchers: Vec<Value> = (0..watchers_max)
            .map(|_| json!({"globPattern": "**/*.rs"}))
            .collect();
        watchers.push(json!({"globPattern": "**/*.md"}));
        watchers.push(json!({"globPattern": "**/*.txt"}));
        engine
            .send(&json!({
                "jsonrpc": "2.0",
                "id": "oversized-batch",
                "method": "client/registerCapability",
                "params": {
                    "registrations": [{
                        "id": "watch-many",
                        "method": "workspace/didChangeWatchedFiles",
                        "registerOptions": {"watchers": watchers},
                    }],
                },
            }))
            .await;
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine.respond(&id, json!([])).await;
        let notified = engine.next_message().await;
        assert_eq!(
            notified["method"],
            json!("workspace/didChangeWatchedFiles"),
            "{notified:#}"
        );
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let document = path("src/lib.rs");
    let position = Position {
        line: 0,
        character: 3,
    };
    session
        .references(&document, position)
        .await
        .expect("references answers");

    let matched = session
        .notify_changed_paths(&[
            (document.clone(), lsp_types::FileChangeType::CHANGED),
            (path("src/notes.md"), lsp_types::FileChangeType::CHANGED),
            (path("src/readme.txt"), lsp_types::FileChangeType::CHANGED),
        ])
        .await
        .expect("the notification sends");
    assert_eq!(
        matched,
        vec![document],
        "the 65th and 66th watchers never landed once the bound was reached"
    );

    session.shutdown().await;
    join(engine_task).await;
}

/// The launch's `initialization_options` reach the engine's `initialize`
/// parameters exactly when configured, and are absent when they are not.
#[tokio::test]
async fn initialization_options_ride_the_initialize_params_exactly_when_configured() {
    let mut with_options = transport_launch();
    with_options.initialization_options = Some(json!({ "engine": "scripted" }));
    let (_workspace, result, engine_task) = begin(with_options, |mut engine| async move {
        let (id, params) = engine.expect_request("initialize").await;
        assert_eq!(
            params["initializationOptions"],
            json!({ "engine": "scripted" }),
            "the configured options must ride the request"
        );
        engine.respond(&id, json!({"capabilities": {}})).await;
        engine.read_initialized().await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    result.expect("the handshake completes").shutdown().await;
    join(engine_task).await;

    let (_workspace, result, engine_task) = begin(transport_launch(), |mut engine| async move {
        let (id, params) = engine.expect_request("initialize").await;
        assert!(
            params["initializationOptions"].is_null(),
            "no options were configured: {params:#}"
        );
        engine.respond(&id, json!({"capabilities": {}})).await;
        engine.read_initialized().await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    result.expect("the handshake completes").shutdown().await;
    join(engine_task).await;
}

/// The outside-root engine answers its fixed escape URI verbatim; the
/// session relays it unchanged.
#[tokio::test]
async fn outside_root_engine_answers_its_escape_uri() {
    let (_workspace, mut session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let opened = engine.next_message().await;
        assert_eq!(opened["method"], json!("textDocument/didOpen"));
        let (id, _params) = engine.expect_request("textDocument/references").await;
        engine
            .respond(
                &id,
                json!([{ "uri": "file:///rift-elsewhere/out.rs", "range": zero_range() }]),
            )
            .await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let target = path("lib.rs");
    session
        .open(&target, "rust", "pub fn beacon() {}\n".to_owned())
        .await
        .expect("didOpen is sent");
    let locations = session
        .references(
            &target,
            Position {
                line: 0,
                character: 7,
            },
        )
        .await
        .expect("request answers");
    assert_eq!(locations[0].uri.as_str(), "file:///rift-elsewhere/out.rs");
    session.shutdown().await;
    join(engine_task).await;
}

/// A transport session's `Debug` rendering names no process: `child_pid`
/// stays `None`, because `start_over_transport` never spawns one.
#[tokio::test]
async fn debug_of_a_transport_session_names_no_process() {
    let (_workspace, session, engine_task) = started(|mut engine| async move {
        engine.handshake(full_capabilities()).await;
        let (id, _params) = engine.expect_request("shutdown").await;
        engine.respond(&id, Value::Null).await;
        engine.next_message().await;
    })
    .await;
    let rendered = format!("{session:?}");
    assert!(
        rendered.contains("child_pid: None"),
        "a transport session names no process: {rendered}"
    );
    session.shutdown().await;
    join(engine_task).await;
}

// ---------------------------------------------------------------------
// Process lifecycle: exit, hang, and crash of a real minimal process.
//
// Each fixture in `process_lifecycle.rs` is a plain `sh -c` script; none of
// it speaks a scripted misbehavior grid, only exit, hang, or a fixed
// handshake answer.
// ---------------------------------------------------------------------

/// A launch naming `program`, otherwise identical to [`transport_launch`],
/// for the two tests that must fail before any process exists.
fn launch_naming(program: &str) -> EngineLaunch {
    EngineLaunch {
        program: program.to_owned(),
        arguments: Vec::new(),
        environment: BTreeMap::new(),
        initialization_options: None,
        startup_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        stderr_capture_bytes: 4_096,
    }
}

#[tokio::test]
async fn silent_engine_is_killed_at_the_startup_timeout() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut silent = process_lifecycle::never_responds();
    silent.startup_timeout = Duration::from_millis(300);
    let started_at = Instant::now();
    let error = EngineSession::start(silent, workspace.path())
        .await
        .expect_err("the engine never answers");
    assert!(matches!(error.fault(), EngineFault::TimedOut { .. }));
    let elapsed = started_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the kill must not wait out the engine: {elapsed:?}"
    );
}

#[tokio::test]
async fn lingering_engine_is_killed_at_the_shutdown_timeout() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let session = EngineSession::start(process_lifecycle::answers_then_hangs(), workspace.path())
        .await
        .expect("the engine answers initialize and then hangs");
    let started_at = Instant::now();
    session.shutdown().await;
    let elapsed = started_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "the shutdown kill must not wait out the engine: {elapsed:?}"
    );
}

/// An engine that answers `shutdown` properly, unlike
/// [`lingering_engine_is_killed_at_the_shutdown_timeout`]'s engine, which
/// never does and so ends the session from the shutdown request's own
/// timeout. Answering `shutdown` keeps the session unended past that
/// point, so the wait on the still-running child overstays
/// `EngineSession::shutdown`'s own timeout and kills it there instead.
#[tokio::test]
async fn engine_that_answers_shutdown_but_never_exits_is_killed_after_the_wait() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let session = EngineSession::start(
        process_lifecycle::answers_shutdown_but_never_exits(),
        workspace.path(),
    )
    .await
    .expect("the engine answers initialize");
    let started_at = Instant::now();
    session.shutdown().await;
    let elapsed = started_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "the post-shutdown kill must not wait out the engine: {elapsed:?}"
    );
}

/// A spawned session's `Debug` rendering names its child's pid, and
/// `ended` flips from `false` to `true` once a fault ends the session in
/// place.
#[tokio::test]
async fn debug_of_a_spawned_session_names_its_pid_and_flips_ended_after_a_fault() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut hanging = process_lifecycle::answers_then_hangs();
    hanging.request_timeout = Duration::from_millis(300);
    let mut session = EngineSession::start(hanging, workspace.path())
        .await
        .expect("the engine answers initialize and then hangs");
    let before = format!("{session:?}");
    assert!(
        before.contains("child_pid: Some("),
        "a spawned session names its child's pid: {before}"
    );
    assert!(
        before.contains("ended: false"),
        "a session that has not ended reports so: {before}"
    );
    let document = path("src/lib.rs");
    let error = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("the hanging engine never answers the request");
    assert!(matches!(error.fault(), EngineFault::TimedOut { .. }));
    let after = format!("{session:?}");
    assert!(
        after.contains("ended: true"),
        "the timed-out request ends the session in place: {after}"
    );
    session.shutdown().await;
}

#[tokio::test]
async fn engine_exit_mid_request_ends_the_session() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut session =
        EngineSession::start(process_lifecycle::answers_then_exits(), workspace.path())
            .await
            .expect("the engine answers initialize before it exits");
    let document = path("src/lib.rs");
    let error = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("the engine exits instead of answering");
    assert!(matches!(
        error.fault(),
        EngineFault::ConnectionClosed { .. }
    ));
    let ended = session
        .references(
            &document,
            Position {
                line: 0,
                character: 0,
            },
        )
        .await
        .expect_err("the session refuses after its engine ended");
    assert!(matches!(ended.fault(), EngineFault::Ended));
    session.shutdown().await;
}

#[tokio::test]
async fn engine_death_between_operations_closes_the_connection() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut session =
        EngineSession::start(process_lifecycle::answers_then_exits(), workspace.path())
            .await
            .expect("the engine answers initialize before it exits");
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
async fn missing_program_fails_the_launch_with_a_typed_error() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let error = EngineSession::start(
        launch_naming("rift-engine-that-does-not-exist"),
        workspace.path(),
    )
    .await
    .expect_err("the program cannot be found");
    assert!(matches!(error.fault(), EngineFault::LaunchFailed { .. }));
}

#[tokio::test]
async fn empty_and_absolute_programs_are_refused_before_spawning() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let refused = EngineSession::start(launch_naming(""), workspace.path())
        .await
        .expect_err("an empty program is refused");
    assert!(matches!(refused.fault(), EngineFault::ProgramEmpty));
    let refused = EngineSession::start(launch_naming("/usr/bin/rift-engine"), workspace.path())
        .await
        .expect_err("an absolute executable path is refused");
    assert!(matches!(
        refused.fault(),
        EngineFault::ProgramAbsolute { program } if program == "/usr/bin/rift-engine"
    ));
    assert_eq!(
        refused.name(),
        ErrorName::Wire(ErrorCode::ConfigurationInvalid)
    );
}

#[tokio::test]
async fn notification_write_to_a_non_reading_engine_times_out() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let mut deaf = process_lifecycle::answers_then_hangs();
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
async fn engine_inherits_the_server_environment() {
    let workspace = tempfile::tempdir().expect("tempdir");
    let session = EngineSession::start(process_lifecycle::reports_environment(), workspace.path())
        .await
        .expect("the engine answers initialize");
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
    let mut overlaid = process_lifecycle::reports_environment();
    overlaid
        .environment
        .insert("HOME".to_owned(), "/rift/overlay".to_owned());
    overlaid
        .environment
        .insert("RIFT_ENGINE_PROBE".to_owned(), "42".to_owned());
    let session = EngineSession::start(overlaid, workspace.path())
        .await
        .expect("the engine answers initialize");
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
async fn workspace_root_without_a_name_still_initializes() {
    let session = EngineSession::start(process_lifecycle::answers_then_exits(), Path::new("/"))
        .await
        .expect("the filesystem root serves as a workspace root");
    session.shutdown().await;
}
