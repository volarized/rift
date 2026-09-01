//! The embedded ty engine: Python semantics served in process.
//!
//! [`started_session`] opens a standard [`EngineSession`] over an in-memory
//! duplex transport whose far end a spawned task serves, speaking the LSP
//! base protocol backed by the linked-in ty crates. Every consumption site
//! keeps its one engine contract: capability negotiation, settlement, and
//! position encoding run exactly as they do over a spawned process, so
//! rename, references, and diagnostics need no embedded-specific path.
//!
//! The served surface is the subset the server itself asks engines for:
//! `initialize`, document open and close, `textDocument/prepareRename`,
//! `textDocument/rename`, `textDocument/references`, and the
//! `textDocument/diagnostic` pull. `workspace/willRenameFiles` is not
//! advertised, so a moved Python file lands with the references-not-updated
//! warning the capability's absence carries. Ranges cross the wire in
//! UTF-8 positions, the encoding the answer advertises.
//!
//! ty analyzes the tree on disk: the database is rooted at the workspace,
//! discovery stays inside it (`discover_without_uv` only when the tree
//! carries a `pyproject.toml` or `ty.toml` marker), and a document open
//! feeds the database the file's current on-disk state. The session hands
//! indexed source that the server has already witnessed against disk, so
//! the two views agree by the time an exchange runs.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rift_lsp::{EngineError, EngineLaunch, EngineSession, Framing, PositionEncoding};
use ruff_db::Db as _;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ty_project::watch::{ChangeEvent, ChangedKind, CreatedKind, DeletedKind};
use ty_project::{ProjectDatabase, ProjectMetadata, SemanticDb as _};

/// Bytes the in-memory transport buffers per direction before a writer waits.
const DUPLEX_BYTES: usize = 256 * 1024;

/// Bytes one framed payload from the session may hold; a change tool's
/// document open carries the whole file, so the bound tracks the largest
/// source the Python provider accepts.
const PAYLOAD_BYTES_MAX: usize = 8 * 1024 * 1024;

/// JSON-RPC error code for a method this engine does not serve.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code for a request this engine could not complete.
const INTERNAL_ERROR: i64 = -32603;

/// Starts one embedded ty session for `workspace_root`.
///
/// The session side is a standard [`EngineSession`]; the far end is a task
/// serving ty over the duplex transport. Dropping the session's transport
/// ends the task the way a spawned engine's exit does.
///
/// # Errors
///
/// Returns [`EngineError`] when the handshake refuses, exactly as a
/// spawned engine's start does.
///
/// # Cancel safety
///
/// Dropping the returned future closes the transport, and the serving task
/// ends on the closed pipe.
pub(crate) async fn started_session(
    launch: EngineLaunch,
    workspace_root: &Path,
) -> Result<EngineSession, EngineError> {
    let (client, server) = tokio::io::duplex(DUPLEX_BYTES);
    let root = workspace_root.to_path_buf();
    tokio::spawn(serve(server, root));
    EngineSession::start_over_transport(launch, workspace_root, client, tokio::io::empty()).await
}

/// Serves the LSP loop over one transport until the peer closes it or
/// sends `exit`.
async fn serve(transport: tokio::io::DuplexStream, root: PathBuf) {
    let (mut reader, mut writer) = tokio::io::split(transport);
    let mut framing = Framing::new();
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut collected: usize = 0;
    loop {
        let payload = loop {
            match framing.frame() {
                Ok(Some(payload)) => break Some(payload),
                Ok(None) => {}
                Err(_) => break None,
            }
            let read = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break None,
                Ok(read) => read,
            };
            collected = collected.saturating_add(read);
            if collected > PAYLOAD_BYTES_MAX.saturating_mul(4) {
                break None;
            }
            if framing.feed(&buffer[..read]).is_err() {
                break None;
            }
        };
        let Some(payload) = payload else {
            return;
        };
        collected = 0;
        let Ok(message) = serde_json::from_slice::<Value>(&payload) else {
            return;
        };
        let root = root.clone();
        let handled =
            tokio::task::spawn_blocking(move || handle_message(&message, &root)).await;
        let outcome = match handled {
            Ok(outcome) => outcome,
            Err(_) => return,
        };
        match outcome {
            Handled::Reply(reply) => {
                let Ok(body) = serde_json::to_vec(&reply) else {
                    return;
                };
                let frame = format!("Content-Length: {}\r\n\r\n", body.len());
                if writer.write_all(frame.as_bytes()).await.is_err()
                    || writer.write_all(&body).await.is_err()
                {
                    return;
                }
            }
            Handled::Silent => {}
            Handled::Exit => return,
        }
    }
}

/// What one handled message asks the loop to do.
enum Handled {
    /// Write this JSON-RPC reply.
    Reply(Value),
    /// A notification: nothing to write.
    Silent,
    /// The peer said `exit`: end the loop.
    Exit,
}

/// Answers one JSON-RPC message from the session.
fn handle_message(message: &Value, root: &Path) -> Handled {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        ("exit", _) => Handled::Exit,
        (_, None) => {
            if method == "textDocument/didOpen" {
                opened_document(root, &params);
            }
            Handled::Silent
        }
        ("initialize", Some(id)) => Handled::Reply(reply(id, initialize_result())),
        ("shutdown", Some(id)) => Handled::Reply(reply(id, Value::Null)),
        ("textDocument/prepareRename", Some(id)) => answered(id, root, &params, prepare_rename),
        ("textDocument/rename", Some(id)) => answered(id, root, &params, rename),
        ("textDocument/references", Some(id)) => answered(id, root, &params, references),
        ("textDocument/diagnostic", Some(id)) => answered(id, root, &params, pulled_diagnostics),
        (_, Some(id)) => Handled::Reply(error_reply(
            id,
            METHOD_NOT_FOUND,
            format!("the embedded ty engine does not serve {method}"),
        )),
    }
}

/// Runs one answer against the tree's database and wraps it as a reply.
fn answered(
    id: Value,
    root: &Path,
    params: &Value,
    answer: fn(&ProjectDatabase, &Path, &Value) -> Result<Value, String>,
) -> Handled {
    let outcome = with_database(root, |db, root| answer(db, root, params));
    Handled::Reply(match outcome {
        Ok(result) => reply(id, result),
        Err(detail) => error_reply(id, INTERNAL_ERROR, detail),
    })
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The capabilities this engine advertises: UTF-8 positions, prepare and
/// rename, references, and the diagnostic pull. No file-operation
/// capability is declared, so moves warn instead of asking.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "positionEncoding": "utf-8",
            "renameProvider": { "prepareProvider": true },
            "referencesProvider": true,
            "diagnosticProvider": {
                "interFileDependencies": true,
                "workspaceDiagnostics": false,
            },
        },
        "serverInfo": { "name": "ty (embedded)" },
    })
}

/// The process-wide database cache, keyed by canonical tree root: one
/// workspace server serves one tree, and a replaced session reuses the
/// database its predecessor built.
fn databases() -> &'static Mutex<HashMap<PathBuf, ProjectDatabase>> {
    static DATABASES: OnceLock<Mutex<HashMap<PathBuf, ProjectDatabase>>> = OnceLock::new();
    DATABASES.get_or_init(Mutex::default)
}

/// Runs `answer` against the tree's database, building it on first use.
/// The lock is held for the whole call, which serializes semantic work per
/// process; every answer extracts owned data before returning.
fn with_database<T>(
    tree_root: &Path,
    answer: impl FnOnce(&ProjectDatabase, &Path) -> Result<T, String>,
) -> Result<T, String> {
    let root = tree_root
        .canonicalize()
        .map_err(|error| format!("tree root {}: {error}", tree_root.display()))?;
    let mut databases = databases()
        .lock()
        .map_err(|_| "the embedded ty database cache is poisoned".to_owned())?;
    let database = match databases.entry(root.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => entry.insert(built_database(&root)?),
    };
    answer(database, &root)
}

/// Builds a project database rooted exactly at `root`.
///
/// Discovery is hermetic: `discover_without_uv` runs only when the tree
/// carries its own project marker, so semantic answers never leak past the
/// served directory; a markerless tree gets default metadata pinned at its
/// root.
fn built_database(root: &Path) -> Result<ProjectDatabase, String> {
    let has_marker = root.join("pyproject.toml").is_file() || root.join("ty.toml").is_file();
    let system_root = SystemPathBuf::from_path_buf(root.to_path_buf())
        .map_err(|path| format!("tree root is not UTF-8: {}", path.display()))?;
    let system = OsSystem::new(&system_root);
    let metadata = if has_marker {
        ProjectMetadata::discover_without_uv(&system_root, &system)
            .map_err(|error| format!("ty project discovery: {error}"))?
    } else {
        ProjectMetadata::new(
            system_root.file_name().unwrap_or("tree"),
            system_root.clone(),
        )
    };
    ProjectDatabase::fallible(metadata, system).map_err(|error| format!("ty database: {error}"))
}

/// Feeds the database one opened document's on-disk state, so an answer
/// reads the bytes the server just witnessed.
fn opened_document(root: &Path, params: &Value) {
    let Some(path) = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .and_then(|uri| uri_to_path(uri))
    else {
        return;
    };
    let Ok(root) = root.canonicalize() else {
        return;
    };
    let Ok(system_path) = SystemPathBuf::from_path_buf(path.clone()) else {
        return;
    };
    let mut databases = match databases().lock() {
        Ok(databases) => databases,
        Err(_) => return,
    };
    let Some(database) = databases.get_mut(&root) else {
        return;
    };
    let event = if !path.exists() {
        ChangeEvent::Deleted {
            path: system_path,
            kind: DeletedKind::Any,
        }
    } else if database.files().try_system(database, &system_path).is_some() {
        ChangeEvent::Changed {
            path: system_path,
            kind: ChangedKind::FileContent,
        }
    } else {
        ChangeEvent::Created {
            path: system_path,
            kind: CreatedKind::File,
        }
    };
    database.apply_changes(&[event]);
}

/// The ty file behind one `file://` URI, refused as text when it cannot
/// resolve.
fn file_at(database: &ProjectDatabase, uri: &Value) -> Result<File, String> {
    let path = uri
        .as_str()
        .and_then(uri_to_path)
        .ok_or_else(|| format!("unreadable document uri: {uri}"))?;
    let system_path = SystemPathBuf::from_path_buf(path)
        .map_err(|path| format!("document path is not UTF-8: {}", path.display()))?;
    system_path_to_file(database, &system_path)
        .map_err(|error| format!("document {}: {error:?}", system_path))
}

/// The filesystem path one `file://` URI spells.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded: String = percent_encoding::percent_decode_str(rest)
        .decode_utf8()
        .ok()?
        .into_owned();
    Some(PathBuf::from(decoded))
}

/// The `file://` URI one path spells, percent-escaping what RFC 3986 keeps
/// out of a path segment.
fn path_to_uri(path: &Path) -> String {
    const ESCAPED: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`');
    format!(
        "file://{}",
        percent_encoding::utf8_percent_encode(&path.to_string_lossy(), ESCAPED)
    )
}

/// The byte offset one LSP position addresses in `text`.
fn offset_at(text: &str, position: &Value) -> Result<TextSize, String> {
    let position: lsp_types::Position = serde_json::from_value(position.clone())
        .map_err(|error| format!("unreadable position: {error}"))?;
    let index = rift_lsp::LineIndex::new(text);
    let offset = index
        .byte_offset(PositionEncoding::Utf8, position)
        .map_err(|error| format!("position outside the document: {error}"))?;
    TextSize::try_from(offset).map_err(|error| format!("offset width: {error}"))
}

/// The LSP range one byte range spells in `text`, in UTF-8 positions.
fn range_at(text: &str, range: TextRange) -> Result<Value, String> {
    let index = rift_lsp::LineIndex::new(text);
    let start = index
        .position(PositionEncoding::Utf8, usize::from(range.start()))
        .map_err(|error| format!("range start outside the document: {error}"))?;
    let end = index
        .position(PositionEncoding::Utf8, usize::from(range.end()))
        .map_err(|error| format!("range end outside the document: {error}"))?;
    Ok(json!({ "start": start, "end": end }))
}

/// Answers `textDocument/prepareRename`: the renameable range, or `null`.
fn prepare_rename(database: &ProjectDatabase, _root: &Path, params: &Value) -> Result<Value, String> {
    let file = file_at(database, params.pointer("/textDocument/uri").unwrap_or(&Value::Null))?;
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let offset = offset_at(text.as_str(), params.pointer("/position").unwrap_or(&Value::Null))?;
    match ty_ide::can_rename(database, program_file, offset) {
        Some(range) => range_at(text.as_str(), range),
        None => Ok(Value::Null),
    }
}

/// Answers `textDocument/rename`: the workspace edit renaming every
/// reference, or `null` when nothing renameable is declared at the position.
fn rename(database: &ProjectDatabase, _root: &Path, params: &Value) -> Result<Value, String> {
    let file = file_at(database, params.pointer("/textDocument/uri").unwrap_or(&Value::Null))?;
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let offset = offset_at(text.as_str(), params.pointer("/position").unwrap_or(&Value::Null))?;
    let new_name = params
        .pointer("/newName")
        .and_then(Value::as_str)
        .ok_or_else(|| "rename carries no newName".to_owned())?;
    if ty_ide::can_rename(database, program_file, offset).is_none() {
        return Ok(Value::Null);
    }
    let Some(targets) = ty_ide::rename(database, program_file, offset, new_name) else {
        return Ok(Value::Null);
    };
    let mut changes: serde_json::Map<String, Value> = serde_json::Map::new();
    for target in &targets {
        let Some((uri, edit_range)) = target_location(database, target.file(), target.range())?
        else {
            continue;
        };
        let edit = json!({ "range": edit_range, "newText": new_name });
        match changes.get_mut(&uri) {
            Some(Value::Array(edits)) => edits.push(edit),
            _ => {
                changes.insert(uri, Value::Array(vec![edit]));
            }
        }
    }
    Ok(json!({ "changes": changes }))
}

/// Answers `textDocument/references`.
fn references(database: &ProjectDatabase, _root: &Path, params: &Value) -> Result<Value, String> {
    let file = file_at(database, params.pointer("/textDocument/uri").unwrap_or(&Value::Null))?;
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let offset = offset_at(text.as_str(), params.pointer("/position").unwrap_or(&Value::Null))?;
    let include_declaration = params
        .pointer("/context/includeDeclaration")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(targets) =
        ty_ide::find_references(database, program_file, offset, include_declaration)
    else {
        return Ok(json!([]));
    };
    let mut locations = Vec::new();
    for target in &targets {
        if let Some((uri, range)) = target_location(database, target.file(), target.range())? {
            locations.push(json!({ "uri": uri, "range": range }));
        }
    }
    Ok(Value::Array(locations))
}

/// One reference target as a `file://` URI and UTF-8 range; `None` for a
/// target outside the filesystem, such as a vendored stub.
fn target_location(
    database: &ProjectDatabase,
    file: File,
    range: TextRange,
) -> Result<Option<(String, Value)>, String> {
    let Some(system_path) = file.path(database).as_system_path() else {
        return Ok(None);
    };
    let uri = path_to_uri(system_path.as_std_path());
    let text = source_text(database, file);
    let range = range_at(text.as_str(), range)?;
    Ok(Some((uri, range)))
}

/// Answers the `textDocument/diagnostic` pull with one full report.
fn pulled_diagnostics(
    database: &ProjectDatabase,
    _root: &Path,
    params: &Value,
) -> Result<Value, String> {
    let file = file_at(database, params.pointer("/textDocument/uri").unwrap_or(&Value::Null))?;
    let text = source_text(database, file);
    let mut items = Vec::new();
    for diagnostic in database.check_file(file) {
        let severity = match diagnostic.severity() {
            ruff_db::diagnostic::Severity::Info => 3,
            ruff_db::diagnostic::Severity::Warning => 2,
            ruff_db::diagnostic::Severity::Error | ruff_db::diagnostic::Severity::Fatal => 1,
        };
        let range = diagnostic
            .primary_span()
            .and_then(|span| span.range())
            .map(|range| range_at(text.as_str(), range))
            .transpose()?
            .unwrap_or_else(|| {
                json!({
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 0 },
                })
            });
        items.push(json!({
            "range": range,
            "severity": severity,
            "code": diagnostic.id().to_string(),
            "message": diagnostic.concise_message().to_string(),
        }));
    }
    Ok(json!({ "kind": "full", "items": items }))
}
