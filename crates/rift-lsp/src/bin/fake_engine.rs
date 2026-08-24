//! Scripted fake language engine for rift-lsp integration tests.
//!
//! The binary speaks real base-protocol framing over stdio. Its first
//! argument selects a scripted behavior; each behavior misbehaves in
//! exactly one way so a test can prove one session policy at a time. The
//! process is test scaffolding: it never ships and may end abruptly by
//! script.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, StdinLock, Write};
use std::path::PathBuf;

use lsp_types::error_codes::SERVER_CANCELLED;
use rift_lsp::framing::{Framing, MESSAGE_BYTES_MAX};
use serde_json::{Value, json};

/// Bytes read from stdin per read call.
const READ_BYTES: usize = 8 << 10;

/// Bytes of standard error the flooding behavior writes.
const FLOOD_BYTES: usize = 1 << 20;

/// Most `.rs` files the word-renaming behaviors scan under the root.
const SCANNED_FILES_MAX: usize = 256;

/// Deepest directory level the word-renaming behaviors walk.
const SCANNED_DEPTH_MAX: usize = 3;

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

/// Conversation state the word-renaming behaviors read back.
#[derive(Default)]
struct EngineState {
    /// The workspace root path, decoded from the initialize `rootUri`.
    root: String,
    /// Each opened document's text, by URI.
    opened: HashMap<String, String>,
}

/// Reads frames and dispatches until stdin closes or the script exits.
fn serve(behavior: &str) {
    if behavior == "environment" {
        report_environment();
    }
    wait_for_start_gate();
    let mut input = EngineInput::new();
    let mut state = EngineState::default();
    while let Some(message) = input.next_message() {
        dispatch(behavior, &message, &mut input, &mut state);
    }
}

/// Handles one client message under the selected behavior.
fn dispatch(behavior: &str, message: &Value, input: &mut EngineInput, state: &mut EngineState) {
    let method = message["method"].as_str().unwrap_or_default();
    let id = &message["id"];
    match method {
        "initialize" => {
            record_lifecycle("initialize");
            if behavior == "initialization-options" {
                verify_initialization_options(message);
            }
            message["params"]["rootUri"]
                .as_str()
                .unwrap_or_default()
                .strip_prefix("file://")
                .unwrap_or_default()
                .clone_into(&mut state.root);
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
            record_lifecycle("exit");
            if behavior != "lingering" {
                std::process::exit(0);
            }
        }
        "textDocument/didOpen" => {
            let uri = message["params"]["textDocument"]["uri"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let text = message["params"]["textDocument"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            state.opened.insert(uri, text);
            publish_open_diagnostics(behavior, message);
        }
        "textDocument/rename" => answer_rename(behavior, message, input, state),
        "textDocument/prepareRename" => match behavior {
            "declines-prepare" => respond(id, &Value::Null),
            "parks-on-prepare" => park(),
            "refuses-prepare" => print_message(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32602, "message": "cannot rename here"},
            })),
            _ => respond(
                id,
                &json!({
                    "range": zero_range(),
                    "placeholder": "renamed",
                }),
            ),
        },
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
fn answer_rename(behavior: &str, message: &Value, input: &mut EngineInput, state: &EngineState) {
    match behavior {
        "exit-mid-request" => std::process::exit(0),
        "parks-on-rename" => park(),
        "renames-outside-root" => {
            let edit = json!({"range": zero_range(), "newText": "renamed"});
            respond(
                &message["id"],
                &json!({"changes": {"file:///rift-elsewhere/out.rs": [edit]}}),
            );
            return;
        }
        "renames-word" => {
            respond(&message["id"], &word_rename_answer(message, state));
            return;
        }
        "mutates-then-renames" => {
            let target = message["params"]["textDocument"]["uri"]
                .as_str()
                .unwrap_or_default()
                .strip_prefix("file://")
                .unwrap_or_default();
            let mutated = format!(
                "{}// the engine drifted this file\n",
                std::fs::read_to_string(target).expect("fake engine reads its target")
            );
            std::fs::write(target, mutated).expect("fake engine mutates its target");
            respond(&message["id"], &word_rename_answer(message, state));
            return;
        }
        "dies-on-command" => {
            if message["params"]["newName"] == json!("die") {
                std::process::exit(0);
            }
        }
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

/// The word-rename proposal: every word-boundary occurrence of the word at
/// the requested position, replaced across the root's `.rs` files.
///
/// The opened target contributes edits computed from its `didOpen` text;
/// every other file is read from disk. Positions are UTF-8, matching the
/// encoding the default capabilities advertise, and the fixtures are ASCII.
fn word_rename_answer(message: &Value, state: &EngineState) -> Value {
    let uri = message["params"]["textDocument"]["uri"]
        .as_str()
        .unwrap_or_default();
    let new_name = message["params"]["newName"].as_str().unwrap_or_default();
    let line = usize::try_from(
        message["params"]["position"]["line"]
            .as_u64()
            .unwrap_or_default(),
    )
    .expect("fixture lines fit in usize");
    let character = usize::try_from(
        message["params"]["position"]["character"]
            .as_u64()
            .unwrap_or_default(),
    )
    .expect("fixture characters fit in usize");
    let target_text = state
        .opened
        .get(uri)
        .expect("the rename target was opened")
        .clone();
    let offset = line_start(&target_text, line) + character;
    let old_name = word_at(&target_text, offset);
    let mut changes = serde_json::Map::new();
    for path in rust_files(&state.root) {
        let path_uri = format!("file://{}", path.display());
        let text = if path_uri == uri {
            target_text.clone()
        } else {
            std::fs::read_to_string(&path).expect("fake engine reads sources")
        };
        let edits = word_edits(&text, &old_name, new_name);
        if !edits.is_empty() {
            changes.insert(path_uri, Value::Array(edits));
        }
    }
    json!({ "changes": changes })
}

/// Byte offset where line `line` starts.
fn line_start(text: &str, line: usize) -> usize {
    text.split_inclusive('\n').take(line).map(str::len).sum()
}

/// The ASCII word starting at `offset`.
fn word_at(text: &str, offset: usize) -> String {
    let bytes = text.as_bytes();
    let mut end = offset;
    while end < bytes.len() && is_ascii_word(bytes[end]) {
        end += 1;
    }
    text[offset..end].to_owned()
}

/// Whether one byte continues an ASCII word.
fn is_ascii_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Word-boundary replacement edits with UTF-8 line and character positions.
fn word_edits(text: &str, old_name: &str, new_name: &str) -> Vec<Value> {
    let bytes = text.as_bytes();
    let mut edits = Vec::new();
    for (offset, matched) in text.match_indices(old_name) {
        let clear_before = offset == 0 || !is_ascii_word(bytes[offset - 1]);
        let clear_after = bytes
            .get(offset + matched.len())
            .is_none_or(|byte| !is_ascii_word(*byte));
        if !(clear_before && clear_after) {
            continue;
        }
        let (line, character) = line_character(text, offset);
        edits.push(json!({
            "range": {
                "start": {"line": line, "character": character},
                "end": {"line": line, "character": character + old_name.len()},
            },
            "newText": new_name,
        }));
    }
    edits
}

/// Zero-based line and UTF-8 character of one byte offset.
fn line_character(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |position| position + 1);
    (line, offset - start)
}

/// The root's `.rs` files in path order, walked to a bounded depth and
/// count, hidden directories skipped.
fn rust_files(root: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_rust_files(std::path::Path::new(root), 0, &mut files);
    files.sort();
    files
}

/// Recursive walk behind [`rust_files`], bounded by depth and file count.
fn collect_rust_files(directory: &std::path::Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > SCANNED_DEPTH_MAX || files.len() >= SCANNED_FILES_MAX {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let rust_source = std::path::Path::new(&name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"));
        if path.is_dir() {
            collect_rust_files(&path, depth + 1, files);
        } else if rust_source && files.len() < SCANNED_FILES_MAX {
            files.push(path);
        }
    }
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
/// prove how many engine processes initialized and how many were asked to
/// exit.
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
