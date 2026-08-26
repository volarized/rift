//! Guarded stdio transport: answers what rmcp's own stdio transport leaves
//! silent or misnames.
//!
//! JSON-RPC 2.0 requires `-32700 Parse error` for a frame that fails to
//! parse; rmcp's own codec instead drops it silently, matching other MCP
//! SDKs' defense against an error-reply storm with a peer that echoes
//! malformed input back. A `tools/call` frame whose `arguments` member is
//! not an object reaches rmcp's routing as an unmatched method, answered
//! `-32601 Method not found`, which names nothing an agent can act on.
//! This module inspects each inbound line before rmcp ever sees it and
//! answers both cases itself; every other frame forwards unchanged, so an
//! unrelated unmatched method still gets rmcp's own `-32601`.

use std::io;

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::error as wire;
use rmcp::model::ErrorCode as JsonRpcErrorCode;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite,
    AsyncWriteExt as _,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::failure::WireFailure as _;

/// Bytes one inbound frame may hold before the guard refuses it rather
/// than keep buffering. Large enough for a substantial patch or diff
/// carried as `tools/call` arguments, matching the scale
/// [`rift_protocol::configuration::TEXT_CHUNK_BYTES_MAX`] already uses for
/// "a big but bounded blob" elsewhere in the protocol.
const INBOUND_FRAME_BYTES_MAX: usize = 16 << 20;

/// Bytes relayed per read from the internal duplex feeding rmcp's own
/// output back to the real writer.
const OUTBOUND_RELAY_BYTES: usize = 8 << 10;

/// Capacity of the internal duplex pipe between the guard and rmcp.
const GUARD_DUPLEX_BUFFER_BYTES: usize = 64 << 10;

/// Depth of the queue carrying the guard's own answers to the outbound
/// writer. A full queue makes the inbound task's `send` await: reading
/// further stdin frames pauses until the writer drains one, so a burst of
/// malformed frames applies backpressure to the peer rather than growing
/// memory without bound.
const GUARD_ANSWER_QUEUE_DEPTH: usize = 16;

/// Serves stdio behind the guard, returning the half `rmcp` should be
/// served over. The guard's two background tasks - one reading real
/// stdin, one writing real stdout - run until stdin closes and rmcp's own
/// output stream closes with it; a write failure on the real output ends
/// the read task with it, instead of leaving it forwarding stdin into the
/// bounded guard duplex forever.
pub(crate) fn guarded_stdio() -> tokio::io::DuplexStream {
    let (transport, _inbound, _outbound) = guarded(tokio::io::stdin(), tokio::io::stdout());
    transport
}

/// Wraps `input`/`output` behind the guard, returning the half `rmcp`
/// should be served over, plus the guard's own two background task
/// handles - kept so a caller (a test, here) can observe them end.
///
/// Split from [`guarded_stdio`] so the guard is testable in-process, over
/// an in-memory pipe, with no real stdio.
fn guarded(
    input: impl AsyncRead + Send + Unpin + 'static,
    output: impl AsyncWrite + Send + Unpin + 'static,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
) {
    let (rmcp_side, guard_side) = tokio::io::duplex(GUARD_DUPLEX_BUFFER_BYTES);
    let (guard_reader, guard_writer) = tokio::io::split(guard_side);
    let (answer_sender, answer_receiver) = mpsc::channel(GUARD_ANSWER_QUEUE_DEPTH);
    let writer_gone = CancellationToken::new();
    let inbound = tokio::spawn(read_inbound(
        input,
        guard_writer,
        answer_sender,
        writer_gone.clone(),
    ));
    let outbound = tokio::spawn(write_outbound(
        output,
        answer_receiver,
        guard_reader,
        writer_gone,
    ));
    (rmcp_side, inbound, outbound)
}

/// Reads newline-delimited frames from `input`, forwarding every frame
/// into `forward` for rmcp to read, except a frame that fails to parse or
/// a `tools/call` frame whose `arguments` is not an object - those two
/// answer through `answers` instead and never reach rmcp.
///
/// `writer_gone` is cancelled by [`write_outbound`] once it ends, for any
/// reason: waiting on more stdin, or forwarding a line already read, would
/// otherwise block forever once nothing drains the guard duplex on the
/// other side.
///
/// # Cancel safety
///
/// Dropping this future mid-read leaves any bytes already written to
/// `forward` visible to rmcp; a frame that was midway through being read
/// from `input` is lost, matching an ordinary closed stdin.
async fn read_inbound(
    input: impl AsyncRead + Unpin,
    mut forward: impl AsyncWrite + Unpin,
    answers: mpsc::Sender<Vec<u8>>,
    writer_gone: CancellationToken,
) {
    let mut reader = tokio::io::BufReader::new(input);
    loop {
        let Some(frame) = next_inbound_frame(&mut reader, &writer_gone).await else {
            break;
        };
        match frame {
            InboundFrame::Oversized => {
                if answers.send(parse_error_response()).await.is_err() {
                    break;
                }
            }
            InboundFrame::Line(line) if line.is_empty() => {}
            InboundFrame::Line(line) => match classify_inbound_line(&line) {
                InboundDecision::Answer(response) => {
                    if answers.send(response).await.is_err() {
                        break;
                    }
                }
                InboundDecision::Forward => {
                    let mut framed = line;
                    framed.push(b'\n');
                    if !forward_line(&mut forward, &framed, &writer_gone).await {
                        break;
                    }
                }
            },
        }
    }
    // Closing the forwarding half is what hands rmcp its end of stream.
    // Dropping it alone would not: the writer and the reader of the
    // guard's duplex are two halves of one split stream, and closing here
    // unconditionally - a `writer_gone` cancellation included - is what
    // lets rmcp notice the input side end even when the outbound task
    // broke first.
    let _ = forward.shutdown().await;
}

/// The next inbound frame, or `None` when the stream ended or
/// [`write_outbound`] ended first: waiting on more stdin once nothing
/// drains the guard duplex on the other side would otherwise block
/// forever.
async fn next_inbound_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    writer_gone: &CancellationToken,
) -> Option<InboundFrame> {
    tokio::select! {
        biased;
        () = writer_gone.cancelled() => None,
        frame = read_inbound_frame(reader) => frame.ok().flatten(),
    }
}

/// Forwards `framed` to rmcp, stopping early when [`write_outbound`] ended
/// first instead of waiting forever on a guard duplex nobody drains
/// anymore.
async fn forward_line(
    forward: &mut (impl AsyncWrite + Unpin),
    framed: &[u8],
    writer_gone: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = writer_gone.cancelled() => false,
        result = forward.write_all(framed) => result.is_ok(),
    }
}

/// Owns `output`: every byte written to it comes from this one task, so
/// the guard's own answers and rmcp's answers can never interleave a
/// frame. Drains `answers` and `upstream` until both end - `answers`
/// closes once [`read_inbound`] returns, and `upstream` closes once rmcp's
/// own connection ends.
///
/// Cancels `writer_gone` on every exit, including a real output write
/// failure: without that signal, [`read_inbound`] has no way to learn its
/// sibling died and keeps forwarding stdin into the bounded guard duplex
/// until it fills and blocks forever.
///
/// # Cancel safety
///
/// Dropping this future may leave one frame partially written to
/// `output`; the real-stdout case only drops with the whole process
/// exiting.
async fn write_outbound(
    mut output: impl AsyncWrite + Unpin,
    mut answers: mpsc::Receiver<Vec<u8>>,
    mut upstream: impl AsyncRead + Unpin,
    writer_gone: CancellationToken,
) {
    let mut buffer = [0_u8; OUTBOUND_RELAY_BYTES];
    let mut answers_open = true;
    loop {
        if !answers_open {
            match upstream.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read_bytes) => {
                    if write_and_flush(&mut output, &buffer[..read_bytes])
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            continue;
        }
        tokio::select! {
            biased;
            answer = answers.recv() => match answer {
                Some(bytes) => {
                    if write_and_flush(&mut output, &bytes).await.is_err() {
                        break;
                    }
                }
                None => answers_open = false,
            },
            read = upstream.read(&mut buffer) => match read {
                Ok(0) | Err(_) => break,
                Ok(read_bytes) => {
                    if write_and_flush(&mut output, &buffer[..read_bytes]).await.is_err() {
                        break;
                    }
                }
            },
        }
    }
    writer_gone.cancel();
}

/// Writes `bytes` then flushes, so a peer reading line by line sees the
/// frame promptly instead of waiting on internal buffering.
async fn write_and_flush(output: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> io::Result<()> {
    output.write_all(bytes).await?;
    output.flush().await
}

/// One frame [`read_inbound_frame`] produced.
enum InboundFrame {
    /// A complete line, without its trailing newline.
    Line(Vec<u8>),
    /// A line ran past [`INBOUND_FRAME_BYTES_MAX`] before its newline
    /// arrived; the remainder up to the newline was read and discarded.
    Oversized,
}

/// Reads one newline-delimited frame from `reader`, retaining at most
/// [`INBOUND_FRAME_BYTES_MAX`] bytes regardless of how long the line
/// runs. `Ok(None)` marks end of stream.
///
/// Bytes past the bound are read and dropped rather than kept, so an
/// unbounded peer cannot grow this reader's memory; the discard phase
/// still reads every byte up to the next newline before answering, the
/// same way rmcp's own decoder discards an over-length line - a peer that
/// never sends one is not otherwise bounded here.
async fn read_inbound_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
) -> io::Result<Option<InboundFrame>> {
    let mut line: Vec<u8> = Vec::new();
    let mut oversized = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if line.is_empty() && !oversized {
                None
            } else if oversized {
                Some(InboundFrame::Oversized)
            } else {
                Some(InboundFrame::Line(line))
            });
        }
        let newline_at = available.iter().position(|byte| *byte == b'\n');
        let body_end = newline_at.unwrap_or(available.len());
        let body = &available[..body_end];
        if !oversized {
            if line.len() + body.len() > INBOUND_FRAME_BYTES_MAX {
                oversized = true;
            } else {
                line.extend_from_slice(body);
            }
        }
        let consumed = newline_at.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline_at.is_some() {
            return Ok(Some(if oversized {
                InboundFrame::Oversized
            } else {
                InboundFrame::Line(line)
            }));
        }
    }
}

/// What one parsed inbound line resolves to.
enum InboundDecision {
    /// Forward unchanged to rmcp.
    Forward,
    /// Answer directly; the frame never reaches rmcp.
    Answer(Vec<u8>),
}

/// Classifies one inbound line: invalid JSON answers a parse error; a
/// `tools/call` frame whose `arguments` is not an object answers naming
/// the field; everything else forwards.
fn classify_inbound_line(line: &[u8]) -> InboundDecision {
    let Ok(frame) = serde_json::from_slice::<Value>(line) else {
        return InboundDecision::Answer(parse_error_response());
    };
    match tools_call_arguments_violation(&frame) {
        Some(id) => InboundDecision::Answer(arguments_not_object_response(&id)),
        None => InboundDecision::Forward,
    }
}

/// The frame's `id` when it is a `tools/call` request whose `arguments`
/// member is present and not a JSON object. `None` for every other frame,
/// including a `tools/call` with no `arguments` at all - rmcp's own
/// schema validation answers that absence.
fn tools_call_arguments_violation(frame: &Value) -> Option<Value> {
    if frame.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let arguments = frame.get("params")?.get("arguments")?;
    if arguments.is_object() {
        return None;
    }
    Some(frame.get("id").cloned().unwrap_or(Value::Null))
}

/// One condition the guard refuses before rmcp ever sees the frame.
#[derive(Debug)]
enum GuardFault {
    /// A `tools/call` frame's `arguments` member is present and not a
    /// JSON object.
    ArgumentsNotObject,
}

impl Fault for GuardFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InvalidRequest)
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::ArgumentsNotObject => vec![ErrorContext::new("field", "arguments")],
        }
    }
}

/// The `-32700` reply for a frame that failed to parse. JSON-RPC 2.0
/// requires `id` null here, because no id could be read from the input;
/// the connection stays open for the next frame.
fn parse_error_response() -> Vec<u8> {
    frame_bytes(&json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": {
            "code": JsonRpcErrorCode::PARSE_ERROR.0,
            "message": "Parse error",
            "data": Value::Null,
        }
    }))
}

/// The reply for a `tools/call` frame whose `arguments` is not an object:
/// the same wire shape every other Rift refusal carries, naming the field
/// that failed, carrying the frame's own `id`.
fn arguments_not_object_response(id: &Value) -> Vec<u8> {
    let refusal = Error::new(GuardFault::ArgumentsNotObject).tool_error(wire::ErrorPhase::Read);
    frame_bytes(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": refusal.code.0,
            "message": refusal.message,
            "data": refusal.data,
        }
    }))
}

/// A JSON-RPC line the guard writes on its own, without going through
/// rmcp: the value's bytes followed by a newline. Falls back to a bare
/// parse-error line on a serialization failure this guard's own
/// well-formed values cannot actually produce, so a caller never receives
/// an empty write.
fn frame_bytes(message: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(message).unwrap_or_else(|_| PARSE_ERROR_FALLBACK.to_vec());
    bytes.push(b'\n');
    bytes
}

/// The line [`frame_bytes`] falls back to when serializing this guard's
/// own value somehow fails.
const PARSE_ERROR_FALLBACK: &[u8] =
    br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#;

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{
        AsyncBufReadExt as _, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{InboundFrame, guarded, read_inbound, read_inbound_frame, write_outbound};

    /// A writer whose every write fails, so a caller that depends on the
    /// real output staying writable exercises its failure arm without a
    /// real broken pipe.
    struct AlwaysErrWriter;

    impl AsyncWrite for AlwaysErrWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("forced write failure")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A handler with no overrides at all: every method not registered by
    /// name answers through rmcp's own default routing, which is exactly
    /// what these tests need to observe - the guard's behavior, not any
    /// tool logic.
    struct DefaultHandler;
    impl rmcp::ServerHandler for DefaultHandler {}

    /// One end-to-end guarded session: a controllable fake stdin writer
    /// and a line reader over the guard's real output. The initialize
    /// handshake - the first message any rmcp server session requires -
    /// is already complete when this returns.
    struct GuardedSession {
        stdin: tokio::io::DuplexStream,
        stdout: BufReader<tokio::io::DuplexStream>,
        server: tokio::task::JoinHandle<()>,
        inbound: tokio::task::JoinHandle<()>,
        outbound: tokio::task::JoinHandle<()>,
    }

    impl GuardedSession {
        async fn send(&mut self, raw: &str) {
            self.stdin
                .write_all(raw.as_bytes())
                .await
                .expect("test stdin write must succeed");
            self.stdin
                .write_all(b"\n")
                .await
                .expect("test stdin write must succeed");
        }

        async fn send_bytes(&mut self, raw: &[u8]) {
            self.stdin
                .write_all(raw)
                .await
                .expect("test stdin write must succeed");
            self.stdin
                .write_all(b"\n")
                .await
                .expect("test stdin write must succeed");
        }

        /// Reads one response line, bounded so a guard defect that stops
        /// answering fails the test instead of hanging it.
        async fn read_response(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), self.stdout.read_line(&mut line))
                .await
                .expect("a response line must arrive within the test bound")
                .expect("test stdout read must succeed");
            serde_json::from_str(line.trim_end()).expect("test response must be valid JSON")
        }
    }

    async fn start_guarded_session() -> GuardedSession {
        let (stdin_writer, stdin_reader) = tokio::io::duplex(8 << 10);
        let (stdout_writer, stdout_reader) = tokio::io::duplex(8 << 10);
        let (transport, inbound, outbound) = guarded(stdin_reader, stdout_writer);
        let server = tokio::spawn(async move {
            use rmcp::ServiceExt as _;
            let running = DefaultHandler
                .serve(transport)
                .await
                .expect("test session must initialize");
            let _ = running.waiting().await;
        });
        let mut session = GuardedSession {
            stdin: stdin_writer,
            stdout: BufReader::new(stdout_reader),
            server,
            inbound,
            outbound,
        };
        session
            .send(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"transport-test-client","version":"0.0.0"}}}"#,
            )
            .await;
        let response = session.read_response().await;
        assert_eq!(
            response["id"],
            json!(1),
            "the initialize response must answer request 1: {response}"
        );
        assert!(
            response.get("error").is_none(),
            "initialize must not fail: {response}"
        );
        session
    }

    #[tokio::test]
    async fn test_invalid_json_is_answered_parse_error_and_the_connection_stays_open() {
        let mut session = start_guarded_session().await;
        session.send("not json at all").await;
        let response = session.read_response().await;
        assert_eq!(response["error"]["code"], json!(-32700));
        assert_eq!(response["id"], Value::Null);

        session
            .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await;
        let follow_up = session.read_response().await;
        assert_eq!(follow_up["id"], json!(2));
        assert!(follow_up.get("error").is_none(), "{follow_up}");
        session.server.abort();
    }

    #[tokio::test]
    async fn test_non_object_tool_call_arguments_answer_invalid_request_naming_arguments() {
        let mut session = start_guarded_session().await;
        session
            .send(
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":"not an object"}}"#,
            )
            .await;
        let response = session.read_response().await;
        assert_eq!(response["id"], json!(5));
        assert_eq!(response["error"]["data"]["code"], json!("invalid_request"));
        assert!(
            response["error"]["message"]
                .as_str()
                .expect("message must be a string")
                .contains("arguments"),
            "{response}"
        );

        session
            .send(r#"{"jsonrpc":"2.0","id":6,"method":"tools/list"}"#)
            .await;
        let follow_up = session.read_response().await;
        assert_eq!(follow_up["id"], json!(6));
        assert!(follow_up.get("error").is_none(), "{follow_up}");
        session.server.abort();
    }

    #[tokio::test]
    async fn test_unknown_method_still_answers_method_not_found() {
        let mut session = start_guarded_session().await;
        session
            .send(r#"{"jsonrpc":"2.0","id":7,"method":"definitely/not/a/method"}"#)
            .await;
        let response = session.read_response().await;
        assert_eq!(response["id"], json!(7));
        assert_eq!(response["error"]["code"], json!(-32601));

        session
            .send(r#"{"jsonrpc":"2.0","id":8,"method":"tools/list"}"#)
            .await;
        let follow_up = session.read_response().await;
        assert_eq!(follow_up["id"], json!(8));
        assert!(follow_up.get("error").is_none(), "{follow_up}");
        session.server.abort();
    }

    #[tokio::test]
    async fn test_frame_longer_than_the_byte_bound_refuses_without_growing_unbounded() {
        let mut session = start_guarded_session().await;
        let oversized = vec![b'x'; super::INBOUND_FRAME_BYTES_MAX + 1];
        session.send_bytes(&oversized).await;
        let response = session.read_response().await;
        assert_eq!(response["error"]["code"], json!(-32700));

        session
            .send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#)
            .await;
        let follow_up = session.read_response().await;
        assert_eq!(follow_up["id"], json!(9));
        assert!(follow_up.get("error").is_none(), "{follow_up}");
        session.server.abort();
    }

    /// Proves the module documentation's own claim - the guard's two
    /// background tasks run until stdin closes and rmcp's own output
    /// stream closes with it - by closing stdin and joining every task
    /// directly, with no forced abort standing in for a graceful end.
    #[tokio::test]
    async fn test_closing_stdin_ends_both_guard_tasks_and_the_session_on_their_own() {
        let mut session = start_guarded_session().await;
        session
            .stdin
            .shutdown()
            .await
            .expect("closing the test stdin must succeed");
        let inbound = tokio::time::timeout(Duration::from_secs(5), session.inbound)
            .await
            .expect("the inbound task must end once stdin closes, not hang");
        assert!(
            inbound.is_ok(),
            "the inbound task must end on its own, not be aborted: {inbound:?}"
        );
        let outbound = tokio::time::timeout(Duration::from_secs(5), session.outbound)
            .await
            .expect("the outbound task must end once rmcp's own output closes with it, not hang");
        assert!(
            outbound.is_ok(),
            "the outbound task must end on its own, not be aborted: {outbound:?}"
        );
        let server = tokio::time::timeout(Duration::from_secs(5), session.server)
            .await
            .expect("the rmcp session must end once stdin closes, not hang");
        assert!(
            server.is_ok(),
            "the session task must end without panicking: {server:?}"
        );
    }

    /// Before the fix, a broken real output silently ended only the
    /// outbound task; the inbound task kept forwarding stdin into the
    /// bounded guard duplex with nothing left to drain it, and the session
    /// never ended. This drops the peer of the real output side mid
    /// session, then asserts the session ends instead of hanging.
    #[tokio::test]
    async fn test_breaking_real_output_mid_session_ends_the_session_instead_of_hanging() {
        let mut session = start_guarded_session().await;
        // Dropping the test's own read half of the "real stdout" duplex
        // makes the next write into its write half fail, the same way a
        // closed real stdout would.
        drop(session.stdout);
        session
            .stdin
            .write_all(b"not json at all\n")
            .await
            .expect("test stdin write must succeed");
        let server = tokio::time::timeout(Duration::from_secs(5), session.server)
            .await
            .expect("a broken real output must end the session instead of hanging the process");
        assert!(
            server.is_ok(),
            "the session task must end without panicking: {server:?}"
        );
    }

    #[tokio::test]
    async fn read_inbound_frame_returns_the_partial_line_when_stdin_closes_without_a_trailing_newline()
     {
        let mut reader = BufReader::new(std::io::Cursor::new(b"no trailing newline".to_vec()));
        let frame = read_inbound_frame(&mut reader)
            .await
            .expect("a plain EOF must not be a read error");
        assert!(
            matches!(&frame, Some(InboundFrame::Line(line)) if line == b"no trailing newline"),
            "a partial line at EOF must still answer as one line"
        );
    }

    #[tokio::test]
    async fn read_inbound_frame_reports_oversized_when_stdin_closes_mid_frame_without_a_newline() {
        let body = vec![b'x'; super::INBOUND_FRAME_BYTES_MAX + 1];
        let mut reader = BufReader::new(std::io::Cursor::new(body));
        let frame = read_inbound_frame(&mut reader)
            .await
            .expect("a plain EOF must not be a read error");
        assert!(
            matches!(frame, Some(InboundFrame::Oversized)),
            "an over-length frame that never reaches a newline before EOF must still refuse as oversized"
        );
    }

    /// Before the fix, `read_inbound` kept reading stdin even once nothing
    /// could carry its answer anywhere: with the answer channel already
    /// gone, an oversized frame's own answer has nowhere to go, so the
    /// task must stop instead of trying the next frame in the input.
    #[tokio::test]
    async fn read_inbound_stops_after_an_oversized_frame_once_the_answer_channel_is_gone() {
        let (answers, receiver) = mpsc::channel(1);
        drop(receiver);
        let (forward_writer, mut forward_reader) = tokio::io::duplex(1 << 10);
        let mut input = vec![b'x'; super::INBOUND_FRAME_BYTES_MAX + 1];
        input.push(b'\n');
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
        input.push(b'\n');
        let cursor = std::io::Cursor::new(input);
        tokio::time::timeout(
            Duration::from_secs(5),
            read_inbound(cursor, forward_writer, answers, CancellationToken::new()),
        )
        .await
        .expect("read_inbound must return once the answer channel is gone, not hang");

        let mut collected = Vec::new();
        forward_reader
            .read_to_end(&mut collected)
            .await
            .expect("the forwarding half must close once read_inbound returns");
        assert!(
            collected.is_empty(),
            "the well-formed frame after the oversized one must never be forwarded: {collected:?}"
        );
    }

    #[tokio::test]
    async fn read_inbound_stops_after_a_malformed_frame_once_the_answer_channel_is_gone() {
        let (answers, receiver) = mpsc::channel(1);
        drop(receiver);
        let (forward_writer, mut forward_reader) = tokio::io::duplex(1 << 10);
        let input =
            b"not json at all\n{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/list\"}\n".to_vec();
        let cursor = std::io::Cursor::new(input);
        tokio::time::timeout(
            Duration::from_secs(5),
            read_inbound(cursor, forward_writer, answers, CancellationToken::new()),
        )
        .await
        .expect("read_inbound must return once the answer channel is gone, not hang");

        let mut collected = Vec::new();
        forward_reader
            .read_to_end(&mut collected)
            .await
            .expect("the forwarding half must close once read_inbound returns");
        assert!(
            collected.is_empty(),
            "the well-formed frame after the malformed one must never be forwarded: {collected:?}"
        );
    }

    /// Before the fix, a forwardable frame that could not reach rmcp - the
    /// guard duplex refuses the write - was retried on the next line
    /// instead of ending the task.
    #[tokio::test]
    async fn read_inbound_stops_forwarding_once_the_guard_duplex_cannot_be_written() {
        let (answers, mut receiver) = mpsc::channel(4);
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n\
                       {\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n"
            .to_vec();
        let cursor = std::io::Cursor::new(input);
        tokio::time::timeout(
            Duration::from_secs(5),
            read_inbound(cursor, AlwaysErrWriter, answers, CancellationToken::new()),
        )
        .await
        .expect("read_inbound must return once the forward write fails, not hang");
        assert_eq!(
            receiver.recv().await,
            None,
            "no answer must have been queued for a frame that only needed forwarding"
        );
    }

    /// `write_outbound` keeps relaying rmcp's own answers after
    /// `read_inbound` has already ended: the answer channel closing does
    /// not by itself end the session, since rmcp may still have more to
    /// say.
    #[tokio::test]
    async fn write_outbound_relays_upstream_bytes_once_the_answer_channel_is_gone() {
        let (answers, receiver) = mpsc::channel::<Vec<u8>>(1);
        drop(answers);
        let (mut upstream_writer, upstream_reader) = tokio::io::duplex(1 << 10);
        let (output_writer, mut output_reader) = tokio::io::duplex(1 << 10);
        let writer_gone = CancellationToken::new();
        let task = tokio::spawn(write_outbound(
            output_writer,
            receiver,
            upstream_reader,
            writer_gone.clone(),
        ));
        upstream_writer
            .write_all(b"relayed bytes")
            .await
            .expect("test write must succeed");
        let mut buffer = [0_u8; 32];
        let read_bytes =
            tokio::time::timeout(Duration::from_secs(5), output_reader.read(&mut buffer))
                .await
                .expect("write_outbound must relay once the answer channel is gone, not hang")
                .expect("test read must succeed");
        assert_eq!(&buffer[..read_bytes], b"relayed bytes");

        drop(upstream_writer);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("write_outbound must end once upstream closes")
            .expect("the task must not panic");
        assert!(writer_gone.is_cancelled());
    }

    #[tokio::test]
    async fn write_outbound_stops_once_the_real_output_cannot_be_written_after_the_answer_channel_is_gone()
     {
        let (answers, receiver) = mpsc::channel::<Vec<u8>>(1);
        drop(answers);
        let (mut upstream_writer, upstream_reader) = tokio::io::duplex(1 << 10);
        let writer_gone = CancellationToken::new();
        let task = tokio::spawn(write_outbound(
            AlwaysErrWriter,
            receiver,
            upstream_reader,
            writer_gone.clone(),
        ));
        upstream_writer
            .write_all(b"x")
            .await
            .expect("test write must succeed");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("write_outbound must end once the real output write fails, not hang")
            .expect("the task must not panic");
        assert!(writer_gone.is_cancelled());
    }

    /// `write_outbound` still watches `upstream` for its own end while the
    /// answer channel stays open: a session that never sends a guard
    /// answer must not be able to hang the outbound task past rmcp's own
    /// end of stream.
    #[tokio::test]
    async fn write_outbound_ends_when_upstream_closes_while_still_forwarding_answers() {
        let (answers, receiver) = mpsc::channel::<Vec<u8>>(1);
        let (upstream_writer, upstream_reader) = tokio::io::duplex(1 << 10);
        drop(upstream_writer);
        let (output_writer, _output_reader) = tokio::io::duplex(1 << 10);
        let writer_gone = CancellationToken::new();
        let task = tokio::spawn(write_outbound(
            output_writer,
            receiver,
            upstream_reader,
            writer_gone.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("write_outbound must end once upstream closes while still forwarding answers")
            .expect("the task must not panic");
        assert!(writer_gone.is_cancelled());
        let _answers = answers;
    }

    #[tokio::test]
    async fn write_outbound_ends_when_the_real_output_cannot_be_written_while_forwarding_upstream()
    {
        let (answers, receiver) = mpsc::channel::<Vec<u8>>(1);
        let (mut upstream_writer, upstream_reader) = tokio::io::duplex(1 << 10);
        let writer_gone = CancellationToken::new();
        let task = tokio::spawn(write_outbound(
            AlwaysErrWriter,
            receiver,
            upstream_reader,
            writer_gone.clone(),
        ));
        upstream_writer
            .write_all(b"x")
            .await
            .expect("test write must succeed");
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("write_outbound must end once the real output write fails while still forwarding upstream")
            .expect("the task must not panic");
        assert!(writer_gone.is_cancelled());
        let _answers = answers;
    }
}
