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
            respond(id, &initialize_answer(behavior));
            if behavior == "happy" {
                send_unretained_notifications();
            }
        }
        "initialized" => match behavior {
            "deaf" => park(),
            "exits-after-start" => std::process::exit(0),
            _ => {}
        },
        "shutdown" => respond(id, &Value::Null),
        "exit" => {
            if behavior != "lingering" {
                std::process::exit(0);
            }
        }
        "textDocument/didOpen" => publish_open_diagnostics(behavior, message),
        "textDocument/rename" => answer_rename(behavior, message, input),
        "textDocument/prepareRename" => respond(
            id,
            &json!({
                "range": zero_range(),
                "placeholder": "renamed",
            }),
        ),
        "workspace/willRenameFiles" => {
            let new_uri = message["params"]["files"][0]["newUri"].clone();
            respond(
                id,
                &json!({"changes": {(new_uri.as_str().unwrap_or_default()): []}}),
            );
        }
        "textDocument/diagnostic" => {
            if behavior == "server-requests" {
                respond(id, &json!({"kind": "unchanged", "resultId": "1"}));
            } else {
                respond(
                    id,
                    &json!({"kind": "full", "items": [diagnostic("pulled diagnostic")]}),
                );
            }
        }
        _ => {}
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
fn answer_rename(behavior: &str, message: &Value, input: &mut EngineInput) {
    match behavior {
        "exit-mid-request" => std::process::exit(0),
        "stderr-flood" => {
            let flood = vec![b'f'; FLOOD_BYTES];
            std::io::stderr()
                .write_all(&flood)
                .expect("fake engine floods stderr");
        }
        "server-requests" => demand_client_answers(input),
        "refuses-rename" => {
            print_message(&json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {"code": -32602, "message": "new name is not an identifier"},
            }));
            return;
        }
        "cancels-rename" => {
            print_message(&json!({
                "jsonrpc": "2.0",
                "id": message["id"],
                "error": {
                    "code": SERVER_CANCELLED,
                    "message": "server cancelled the request",
                },
            }));
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
