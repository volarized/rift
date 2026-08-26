//! The engine side of one exchange-level test.
//!
//! `EngineSession::start_over_transport` gives the test the client side of a
//! `tokio::io::duplex` pair; this module reads and writes framed JSON-RPC on
//! the other side, so a scenario is scripted directly in Rust with no
//! process involved. Byte-level framing misbehavior is proven directly
//! against `rift_lsp::framing::Framing` in its own unit tests; this module
//! proves the session's handling of a well-framed but scripted conversation.

use std::collections::VecDeque;

use rift_lsp::framing::Framing;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bytes read from the client stream per read call.
const READ_BYTES: usize = 8 << 10;

/// The scripted engine's own side of the duplex: reads client frames and
/// writes framed answers, blocking one exchange at a time.
pub(crate) struct ScriptedEngine<T> {
    transport: T,
    framing: Framing,
    queue: VecDeque<Value>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> ScriptedEngine<T> {
    pub(crate) fn new(transport: T) -> Self {
        Self {
            transport,
            framing: Framing::new(),
            queue: VecDeque::new(),
        }
    }

    /// The next client request or notification, blocking until one
    /// arrives.
    ///
    /// A bare response - an `id` with no `method` - answers a
    /// server-initiated request the script sent
    /// (`window/workDoneProgress/create`, ...): the client answers it
    /// inline during whichever exchange next reads it, at a point no
    /// script can predict relative to the client's own later requests, so
    /// this silently discards it and keeps reading. Every script cares
    /// only about what the client itself asks or announces.
    pub(crate) async fn next_message(&mut self) -> Value {
        loop {
            let message = self.next_raw_message().await;
            if message.get("method").is_some() {
                return message;
            }
        }
    }

    /// The next raw message from the client - request, notification, or a
    /// bare response - blocking until one arrives.
    ///
    /// Only a script that must inspect the client's own answer to a
    /// server-initiated request it sent calls this directly; every other
    /// script reads through [`ScriptedEngine::next_message`], which
    /// filters bare responses out. Each iteration either returns a queued
    /// message or reads at least one byte, so the wait is bounded by what
    /// the client sends.
    pub(crate) async fn next_raw_message(&mut self) -> Value {
        loop {
            if let Some(message) = self.queue.pop_front() {
                return message;
            }
            let mut chunk = [0_u8; READ_BYTES];
            let read = self
                .transport
                .read(&mut chunk)
                .await
                .expect("the duplex reads");
            assert!(read > 0, "the client closed its side of the duplex");
            let messages = self
                .framing
                .feed(&chunk[..read])
                .expect("the client sends valid frames");
            for message in messages {
                let value: Value = serde_json::from_slice(&message).expect("the client sends JSON");
                self.queue.push_back(value);
            }
        }
    }

    /// Writes one message, framed.
    pub(crate) async fn send(&mut self, message: &Value) {
        let payload = serde_json::to_vec(message).expect("scripted messages serialize");
        self.transport
            .write_all(&Framing::frame(&payload))
            .await
            .expect("the duplex writes");
        self.transport.flush().await.expect("the duplex flushes");
    }

    /// Answers one request's `id` with a success `result`.
    pub(crate) async fn respond(&mut self, id: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    /// Refuses one request's `id` with a JSON-RPC error.
    pub(crate) async fn refuse(&mut self, id: &Value, code: i64, message: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .await;
    }

    /// Sends one notification, which carries no `id`.
    pub(crate) async fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// Reads the next message and asserts it names `method`, returning its
    /// `id` and `params`.
    pub(crate) async fn expect_request(&mut self, method: &str) -> (Value, Value) {
        let message = self.next_message().await;
        assert_eq!(
            message["method"],
            json!(method),
            "expected a {method} request, got {message:#}"
        );
        (message["id"].clone(), message["params"].clone())
    }

    /// Reads and answers one `initialize` request with `capabilities`,
    /// asserting nothing about its parameters.
    pub(crate) async fn answer_initialize(&mut self, capabilities: Value) {
        let (id, _params) = self.expect_request("initialize").await;
        self.respond(&id, json!({ "capabilities": capabilities }))
            .await;
    }

    /// Reads the `initialized` notification that always follows a
    /// successful `initialize` answer.
    pub(crate) async fn read_initialized(&mut self) {
        let message = self.next_message().await;
        assert_eq!(message["method"], json!("initialized"), "{message:#}");
    }

    /// Completes the standard handshake: `initialize` answered with
    /// `capabilities`, then the `initialized` notification drained.
    pub(crate) async fn handshake(&mut self, capabilities: Value) {
        self.answer_initialize(capabilities).await;
        self.read_initialized().await;
    }

    /// Mints [`PROGRESS_TOKEN`] and begins work on it, without waiting for
    /// the client's answer to the create request: the client answers it
    /// inline during its next exchange, exactly as a real client does.
    ///
    /// Callers send this only after a message they read proves the client
    /// has already reached the point the progress traffic describes -
    /// never right after answering `initialize`, before the client's own
    /// `initialized` notification is read back here. A duplex write does
    /// not wait for its reader, so writing unprompted risks the client's
    /// own read racing ahead of where the test's assertions expect it.
    pub(crate) async fn begin_progress(&mut self) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 80,
            "method": "window/workDoneProgress/create",
            "params": {"token": PROGRESS_TOKEN},
        }))
        .await;
        self.notify(
            "$/progress",
            json!({"token": PROGRESS_TOKEN, "value": {"kind": "begin", "title": "loading"}}),
        )
        .await;
    }

    /// Reports continued work on [`PROGRESS_TOKEN`].
    pub(crate) async fn report_progress(&mut self) {
        self.notify(
            "$/progress",
            json!({"token": PROGRESS_TOKEN, "value": {"kind": "report", "message": "indexing"}}),
        )
        .await;
    }

    /// Ends [`PROGRESS_TOKEN`]: the work begun on it is done.
    pub(crate) async fn end_progress(&mut self) {
        self.notify(
            "$/progress",
            json!({"token": PROGRESS_TOKEN, "value": {"kind": "end", "message": "loaded"}}),
        )
        .await;
    }
}

/// The work-done progress token every progress script mints.
pub(crate) const PROGRESS_TOKEN: &str = "scripted/analysis";

/// The capability grid most exchange-level tests need: rename, prepared
/// rename, references, will-rename over every file, and pull diagnostics
/// identified as `"scripted"`.
pub(crate) fn full_capabilities() -> Value {
    json!({
        "positionEncoding": "utf-8",
        "renameProvider": {"prepareProvider": true},
        "referencesProvider": true,
        "workspace": {
            "fileOperations": {
                "willRename": {"filters": [{"pattern": {"glob": "**/*"}}]},
            },
        },
        "diagnosticProvider": {
            "identifier": "scripted",
            "interFileDependencies": false,
            "workspaceDiagnostics": false,
        },
    })
}

/// A zero-width range at the document start.
pub(crate) fn zero_range() -> Value {
    json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": 0, "character": 0},
    })
}
