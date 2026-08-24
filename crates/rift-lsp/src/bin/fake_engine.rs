//! Scripted fake language engine for rift-lsp integration tests.
//!
//! The binary speaks real base-protocol framing over stdio. Its first
//! argument selects a scripted behavior; each behavior misbehaves in
//! exactly one way so a test can prove one session policy at a time. The
//! process is test scaffolding: it never ships and may end abruptly by
//! script.

use std::collections::VecDeque;
use std::io::{Read, StdinLock, Write};

use lsp_types::error_codes::SERVER_CANCELLED;
use rift_lsp::framing::{Framing, MESSAGE_BYTES_MAX};
use serde_json::{Value, json};

/// Bytes read from stdin per read call.
const READ_BYTES: usize = 8 << 10;

/// Bytes of standard error the flooding behavior writes.
const FLOOD_BYTES: usize = 1 << 20;

/// The work-done progress token the progress behaviors mint.
const PROGRESS_TOKEN: &str = "fake/analysis";

/// Behaviors that begin work-done progress as soon as they initialize.
///
/// Each mints [`PROGRESS_TOKEN`] through `window/workDoneProgress/create`
/// and begins it, exactly as a language server loading a project does.
/// `reports-progress` ends the token before it answers a rename,
/// `analyzes-then-serves` and `refuses-while-analyzing` before the second
/// rename each is asked for, `announces-then-answers-nothing` before the
/// will-rename it answers with `null`, and `never-ends-progress` never
/// ends it at all.
const PROGRESS_BEHAVIORS: &[&str] = &[
    "reports-progress",
    "analyzes-then-serves",
    "refuses-while-analyzing",
    "announces-then-answers-nothing",
    "never-ends-progress",
];

fn main() {
    let behavior = std::env::args().nth(1).unwrap_or_default();
    match behavior.as_str() {
        "garbage" => {
            print_raw(b"these bytes are not a base-protocol frame\r\n\r\n");
            park();
        }
        "oversized" => {
            let announced = MESSAGE_BYTES_MAX + 1;
            print_raw(format!("Content-Length: {announced}\r\n\r\n").as_bytes());
            park();
        }
        "never-responds" => park(),
        _ => serve(&behavior),
    }
}

/// The one stdin reader: framing state plus decoded messages not yet read.
///
/// Stdin's lock is not reentrant, so exactly one of these exists and every
/// read - the serve loop's and a scripted mid-request read - goes through
/// it.
struct EngineInput {
    framing: Framing,
    decoded: VecDeque<Value>,
    stdin: StdinLock<'static>,
}

impl EngineInput {
    fn new() -> Self {
        Self {
            framing: Framing::new(),
            decoded: VecDeque::new(),
            stdin: std::io::stdin().lock(),
        }
    }

    /// The next client message, blocking; `None` once stdin closes.
    fn next_message(&mut self) -> Option<Value> {
        let mut chunk = [0_u8; READ_BYTES];
        loop {
            if let Some(message) = self.decoded.pop_front() {
                return Some(message);
            }
            let read = self
                .stdin
                .read(&mut chunk)
                .expect("fake engine reads stdin");
            if read == 0 {
                return None;
            }
            let messages = self
                .framing
                .feed(&chunk[..read])
                .expect("client frames are valid");
            for message in messages {
                let value: Value = serde_json::from_slice(&message).expect("client sends JSON");
                self.decoded.push_back(value);
            }
        }
    }
}

/// Reads frames and dispatches until stdin closes or the script exits.
fn serve(behavior: &str) {
    if behavior == "environment" {
        report_environment();
    }
    wait_for_start_gate();
    let mut input = EngineInput::new();
    while let Some(message) = input.next_message() {
        dispatch(behavior, &message, &mut input);
    }
}

/// Handles one client message under the selected behavior.
fn dispatch(behavior: &str, message: &Value, input: &mut EngineInput) {
    let method = message["method"].as_str().unwrap_or_default();
    let id = &message["id"];
    match method {
        "initialize" => {
            record_lifecycle("initialize");
            if behavior == "initialization-options" {
                verify_initialization_options(message);
            }
            respond(id, &initialize_answer(behavior));
            if behavior == "happy" {
                send_unretained_notifications();
            }
            if PROGRESS_BEHAVIORS.contains(&behavior) {
                begin_progress();
            }
        }
        "initialized" => match behavior {
            "deaf" => park(),
            "exits-after-start" => std::process::exit(0),
            _ => {}
        },
        "shutdown" => respond(id, &Value::Null),
        "exit" => {
            record_lifecycle("exit");
            if behavior != "lingering" {
                std::process::exit(0);
            }
        }
        "textDocument/didOpen" => {
            publish_open_diagnostics(behavior, message);
            if PROGRESS_BEHAVIORS.contains(&behavior) {
                report_progress();
            }
        }
        "textDocument/rename" => answer_rename(behavior, message, input),
        "textDocument/prepareRename" => respond(
            id,
            &json!({
                "range": zero_range(),
                "placeholder": "renamed",
            }),
        ),
        "workspace/willRenameFiles" => answer_will_rename(behavior, message),
        "textDocument/diagnostic" => answer_diagnostic(behavior, id),
        _ => {}
    }
}

/// Answers one will-rename request under the selected behavior.
///
/// Every request is recorded in the lifecycle log first, so a test counts
/// how many times the engine was asked. The default answer is an edit set
/// holding no edit, which says exactly what a `null` answer says;
/// `announces-then-answers-nothing` ends the work it announced before
/// giving the same nothing, so its silence is its own.
fn answer_will_rename(behavior: &str, message: &Value) {
    record_lifecycle("will-rename");
    let id = &message["id"];
    if behavior == "announces-then-answers-nothing" {
        end_progress();
        respond(id, &Value::Null);
        return;
    }
    let new_uri = message["params"]["files"][0]["newUri"].clone();
    respond(
        id,
        &json!({"changes": {(new_uri.as_str().unwrap_or_default()): []}}),
    );
}

/// Answers one diagnostic pull under the selected behavior.
///
/// Every pull is recorded in the lifecycle log first, so a test counts how
/// many times the engine was asked, and `pulls-empty-then-reports` reads
/// that count back: it answers the first pull the way an engine that has
/// not analyzed the document does - cleanly and with nothing to say - and
/// carries its finding on the pull after it.
fn answer_diagnostic(behavior: &str, id: &Value) {
    record_lifecycle("diagnostic");
    match behavior {
        "server-requests" => respond(id, &json!({"kind": "unchanged", "resultId": "1"})),
        "pulls-empty-then-reports" if recorded_lifecycle("diagnostic") == 1 => {
            respond(id, &json!({"kind": "full", "items": []}));
        }
        _ if PROGRESS_BEHAVIORS.contains(&behavior) => {
            // An engine still loading answers cleanly and says nothing.
            respond(id, &json!({"kind": "full", "items": []}));
        }
        _ => respond(
            id,
            &json!({"kind": "full", "items": [diagnostic("pulled diagnostic")]}),
        ),
    }
}

/// The initialize result each behavior advertises.
fn initialize_answer(behavior: &str) -> Value {
    match behavior {
        "no-rename-capability" => json!({"capabilities": {}}),
        "bad-encoding" => json!({"capabilities": {"positionEncoding": "utf-32"}}),
        _ => json!({
            "capabilities": {
                "positionEncoding": "utf-8",
                "renameProvider": {"prepareProvider": true},
                "workspace": {
                    "fileOperations": {
                        "willRename": {
                            "filters": [{"pattern": {"glob": "**/*"}}],
                        },
                    },
                },
                "diagnosticProvider": {
                    "identifier": "fake",
                    "interFileDependencies": false,
                    "workspaceDiagnostics": false,
                },
            },
        }),
    }
}

/// Answers a rename, first exercising the scripted misbehavior, if any.
///
/// Every rename is recorded in the lifecycle log before the behavior runs,
/// so a test counts how many times the engine was asked - and the
/// behaviors that act once and then serve read that count back, which an
/// engine that dies and restarts could not keep in memory.
fn answer_rename(behavior: &str, message: &Value, input: &mut EngineInput) {
    record_lifecycle("rename");
    match behavior {
        "exit-mid-request" => std::process::exit(0),
        "dies-on-command" => {
            if message["params"]["newName"] == json!("die") {
                std::process::exit(0);
            }
        }
        "dies-once-on-rename" => {
            if recorded_lifecycle("rename") == 1 {
                std::process::exit(0);
            }
        }
        "analyzes-then-serves" => {
            if recorded_lifecycle("rename") > 1 {
                end_progress();
            }
        }
        "reports-progress" => end_progress(),
        "stderr-flood" => {
            let flood = vec![b'f'; FLOOD_BYTES];
            std::io::stderr()
                .write_all(&flood)
                .expect("fake engine floods stderr");
        }
        "server-requests" => demand_client_answers(input),
        "refuses-rename" => {
            refuse_rename(&message["id"], -32602, "new name is not an identifier");
            return;
        }
        "refuses-while-analyzing" => {
            // A verdict answered mid-analysis is not a verdict: the engine
            // reaches its real one only once its work has ended.
            if recorded_lifecycle("rename") > 1 {
                end_progress();
            }
            refuse_rename(&message["id"], -32602, "new name is not an identifier");
            return;
        }
        "cancels-first-rename" => {
            if recorded_lifecycle("rename") == 1 {
                refuse_rename(
                    &message["id"],
                    SERVER_CANCELLED,
                    "cancelled the first rename",
                );
                return;
            }
        }
        "cancels-rename" => {
            refuse_rename(
                &message["id"],
                SERVER_CANCELLED,
                "server cancelled the request",
            );
            return;
        }
        "unreadable" => {
            print_message(&json!({"jsonrpc": "2.0"}));
            return;
        }
        "delayed-rename" => std::thread::sleep(std::time::Duration::from_millis(300)),
        "wrong-result" => {
            respond(&message["id"], &json!(42));
            return;
        }
        _ => {}
    }
    let uri = message["params"]["textDocument"]["uri"]
        .as_str()
        .unwrap_or_default();
    let sibling = format!("{uri}.sibling");
    let edit = json!({"range": zero_range(), "newText": "renamed"});
    respond(
        &message["id"],
        &json!({"changes": {(uri): [edit.clone()], (sibling): [edit]}}),
    );
}

/// Sends four server-initiated requests and verifies every answer.
///
/// Configuration, registration, progress, then an unserved probe; the
/// process dies unless the client answers each per its routing policy.
fn demand_client_answers(input: &mut EngineInput) {
    let requests = [
        (90, "workspace/configuration", json!({"items": [{}, {}]})),
        (
            91,
            "client/registerCapability",
            json!({"registrations": []}),
        ),
        (
            92,
            "window/workDoneProgress/create",
            json!({"token": "probe"}),
        ),
        (93, "engine/probe", Value::Null),
    ];
    for (id, method, params) in requests {
        let answer = request(input, id, method, &params).expect("client answers server requests");
        verify_client_answer(&answer);
    }
}

/// One server-initiated request; the returned value is the client's answer.
fn request(input: &mut EngineInput, id: u64, method: &str, params: &Value) -> Option<Value> {
    print_message(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    }));
    input.next_message()
}

/// Dies unless the client's answer matches the routing policy.
fn verify_client_answer(answer: &Value) {
    let verdict = match answer["id"].as_u64() {
        Some(90) => answer["result"] == json!([null, null]),
        Some(91 | 92) => answer["result"] == Value::Null,
        Some(93) => answer["error"]["code"] == json!(-32601),
        _ => false,
    };
    assert!(verdict, "client answer broke the routing policy: {answer}");
}

/// Refuses one rename with a JSON-RPC error.
fn refuse_rename(id: &Value, code: i64, message: &str) {
    print_message(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    }));
}

/// Mints the progress token and begins work on it.
///
/// The create request is sent without waiting for its answer: the client
/// is not reading at this point in the handshake, and it answers the
/// request during its next exchange, exactly as a real client does.
fn begin_progress() {
    print_message(&json!({
        "jsonrpc": "2.0",
        "id": 80,
        "method": "window/workDoneProgress/create",
        "params": {"token": PROGRESS_TOKEN},
    }));
    print_progress(&json!({"kind": "begin", "title": "loading the project"}));
}

/// Reports continued work on the progress token.
fn report_progress() {
    print_progress(&json!({"kind": "report", "message": "indexing"}));
}

/// Ends the progress token: the work the engine announced is done.
fn end_progress() {
    print_progress(&json!({"kind": "end", "message": "project loaded"}));
}

/// Sends one `$/progress` notification for the minted token.
fn print_progress(value: &Value) {
    print_message(&json!({
        "jsonrpc": "2.0",
        "method": "$/progress",
        "params": {"token": PROGRESS_TOKEN, "value": value},
    }));
}

/// Publishes one diagnostic for the opened document, happy path only.
fn publish_open_diagnostics(behavior: &str, message: &Value) {
    if behavior != "happy" && behavior != "server-requests" {
        return;
    }
    let uri = message["params"]["textDocument"]["uri"].clone();
    print_message(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "diagnostics": [diagnostic("published diagnostic")],
        },
    }));
}

/// Sends the notifications the client consumes without retaining.
///
/// A log message, a publish with unreadable parameters, and a publish for
/// a document outside the workspace root.
fn send_unretained_notifications() {
    print_message(&json!({
        "jsonrpc": "2.0",
        "method": "window/logMessage",
        "params": {"type": 3, "message": "engine says hi"},
    }));
    print_message(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": null,
    }));
    print_message(&json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {"uri": "file:///rift-elsewhere/out.rs", "diagnostics": []},
    }));
}

/// Parks until the start gate opens, when the gate variable names one.
///
/// The gate is a FIFO: reading it blocks until the test's writer opens
/// and closes it, so a test holds every spawned engine at startup and
/// releases them all at one deterministic moment.
fn wait_for_start_gate() {
    let Ok(gate) = std::env::var("RIFT_FAKE_ENGINE_START_GATE") else {
        return;
    };
    let _released = std::fs::read(gate).expect("fake engine reads its start gate");
}

/// Appends one lifecycle line when the log environment variable is set.
///
/// A test that sets `RIFT_FAKE_ENGINE_LIFECYCLE_LOG` counts the lines to
/// prove how many engine processes initialized, how many renames they
/// were asked for, and how many were asked to exit.
fn record_lifecycle(event: &str) {
    let Ok(path) = std::env::var("RIFT_FAKE_ENGINE_LIFECYCLE_LOG") else {
        return;
    };
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("fake engine opens its lifecycle log");
    writeln!(log, "{event}").expect("fake engine appends its lifecycle log");
}

/// Lines of one lifecycle event the log already holds.
///
/// The behaviors that act once and then serve read their count back from
/// the log: a scripted death ends the process, and any count it kept in
/// memory with it. Such a behavior requires the log variable and dies
/// without it, so a test that forgot to wire it fails with these words
/// rather than watching the behavior never fire.
fn recorded_lifecycle(event: &str) -> usize {
    let path = std::env::var("RIFT_FAKE_ENGINE_LIFECYCLE_LOG")
        .expect("this behavior counts its requests in the lifecycle log");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| *line == event)
        .count()
}

/// Dies unless the expected initialization options rode the request.
fn verify_initialization_options(message: &Value) {
    let options = &message["params"]["initializationOptions"];
    assert_eq!(
        options,
        &json!({ "engine": "fake" }),
        "initialization options did not arrive"
    );
}

/// Writes the inherited environment to stderr for the policy tests.
fn report_environment() {
    let home = std::env::var("HOME").unwrap_or_default();
    let probe = std::env::var("RIFT_ENGINE_PROBE").unwrap_or_default();
    eprintln!("HOME={home}");
    eprintln!("RIFT_ENGINE_PROBE={probe}");
}

/// One diagnostic value with the given message.
fn diagnostic(message: &str) -> Value {
    json!({"range": zero_range(), "message": message})
}

/// A zero-width range at the document start.
fn zero_range() -> Value {
    json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": 0, "character": 0},
    })
}

/// Responds to one client request.
fn respond(id: &Value, result: &Value) {
    print_message(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

/// Frames and writes one message to stdout.
fn print_message(message: &Value) {
    let payload = serde_json::to_vec(message).expect("scripted messages serialize");
    print_raw(&Framing::frame(&payload));
}

/// Writes raw bytes to stdout and flushes them.
fn print_raw(bytes: &[u8]) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).expect("fake engine writes stdout");
    stdout.flush().expect("fake engine flushes stdout");
}

/// Sleeps forever; the client's timeout is expected to kill the process.
fn park() {
    loop {
        std::thread::sleep(std::time::Duration::from_mins(1));
    }
}
