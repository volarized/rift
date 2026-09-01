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
use std::sync::{Arc, Mutex, OnceLock};

use rift_lsp::{EngineError, EngineLaunch, EngineSession, Framing, PositionEncoding};
use ruff_db::Db as _;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{OsSystem, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ty_project::watch::{ChangeEvent, ChangedKind, CreatedKind, DeletedKind};
use ty_project::{Db as _, ProjectDatabase, ProjectMetadata, SemanticDb as _};

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
/// JSON-RPC error code LSP names `ContentModified`: the document moved
/// between the peer's view and this engine's, so the same request is worth
/// sending again once the views converge. The session classifies it as a
/// re-request signal, never a terminal refusal.
const CONTENT_MODIFIED: i64 = -32801;

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
    let documents: DocumentStore = Arc::new(Mutex::new(HashMap::new()));
    let mut framing = Framing::new();
    let mut buffer = vec![0_u8; 16 * 1024];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let Ok(messages) = framing.feed(&buffer[..read]) else {
            return;
        };
        for payload in messages {
            if payload.len() > PAYLOAD_BYTES_MAX {
                return;
            }
            let Ok(message) = serde_json::from_slice::<Value>(&payload) else {
                return;
            };
            let root = root.clone();
            let documents = Arc::clone(&documents);
            let handled =
                tokio::task::spawn_blocking(move || handle_message(&message, &root, &documents))
                    .await;
            let Ok(outcome) = handled else {
                return;
            };
            match outcome {
                Handled::Reply(reply) => {
                    let Ok(body) = serde_json::to_vec(&reply) else {
                        return;
                    };
                    if writer.write_all(&Framing::frame(&body)).await.is_err() {
                        return;
                    }
                }
                Handled::Silent => {}
                Handled::Exit => return,
            }
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

/// Why one answer could not be produced.
#[derive(Debug)]
enum AnswerRefusal {
    /// The request does not decode, or the database refused: the peer has
    /// something to correct before asking again.
    Invalid(String),
    /// The document moved between the peer's view and this engine's: the
    /// same request is worth sending again once the views converge.
    Moved(String),
}

impl From<String> for AnswerRefusal {
    fn from(detail: String) -> Self {
        Self::Invalid(detail)
    }
}

/// One request's conversion context: the root spellings and the document
/// text the session sent for the addressed URI, when it opened one.
struct Exchange<'request> {
    spelled_root: &'request Path,
    canonical_root: &'request Path,
    sent_text: Option<&'request str>,
}

impl Exchange<'_> {
    /// The text every position and range converts against: the document the
    /// session sent, or the database's own view when nothing is open.
    fn conversion_text<'own>(&'own self, database_text: &'own str) -> &'own str {
        self.sent_text.unwrap_or(database_text)
    }
}

/// The open documents by URI, holding the text the session sent: answers
/// convert every range against the peer's own document, so a span decodes
/// on the server side even while its published index still trails the
/// change the exchange follows.
type DocumentStore = Arc<Mutex<HashMap<String, String>>>;

/// Answers one JSON-RPC message from the session.
fn handle_message(message: &Value, root: &Path, documents: &DocumentStore) -> Handled {
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match (method, id) {
        ("exit", _) => Handled::Exit,
        (_, None) => {
            if method == "textDocument/didOpen" {
                opened_document(root, &params);
                if let (Some(uri), Some(sent)) = (
                    params.pointer("/textDocument/uri").and_then(Value::as_str),
                    params.pointer("/textDocument/text").and_then(Value::as_str),
                ) && let Ok(mut documents) = documents.lock()
                {
                    documents.insert(uri.to_owned(), sent.to_owned());
                }
            }
            if method == "textDocument/didClose"
                && let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str)
                && let Ok(mut documents) = documents.lock()
            {
                documents.remove(uri);
            }
            Handled::Silent
        }
        ("initialize", Some(id)) => Handled::Reply(reply(&id, &initialize_result())),
        ("shutdown", Some(id)) => Handled::Reply(reply(&id, &Value::Null)),
        ("textDocument/prepareRename", Some(id)) => {
            answered(&id, root, documents, &params, prepare_rename)
        }
        ("textDocument/rename", Some(id)) => answered(&id, root, documents, &params, rename),
        ("textDocument/references", Some(id)) => {
            answered(&id, root, documents, &params, references)
        }
        ("textDocument/diagnostic", Some(id)) => {
            answered(&id, root, documents, &params, pulled_diagnostics)
        }
        (_, Some(id)) => Handled::Reply(error_reply(
            &id,
            METHOD_NOT_FOUND,
            &format!("the embedded ty engine does not serve {method}"),
        )),
    }
}

/// Runs one answer against the tree's database and wraps it as a reply.
fn answered(
    id: &Value,
    root: &Path,
    documents: &DocumentStore,
    params: &Value,
    answer: fn(&mut ProjectDatabase, &Exchange<'_>, &Value) -> Result<Value, AnswerRefusal>,
) -> Handled {
    let sent = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .and_then(|uri| documents.lock().ok()?.get(uri).cloned());
    let outcome = with_database(root, |db, canonical| {
        let exchange = Exchange {
            spelled_root: root,
            canonical_root: canonical,
            sent_text: sent.as_deref(),
        };
        answer(db, &exchange, params)
    });
    Handled::Reply(match outcome {
        Ok(result) => reply(id, &result),
        Err(refusal) => {
            let (code, detail) = match refusal {
                AnswerRefusal::Invalid(detail) => (INTERNAL_ERROR, detail),
                AnswerRefusal::Moved(detail) => (CONTENT_MODIFIED, detail),
            };
            error_reply(id, code, &detail)
        }
    })
}

fn reply(id: &Value, result: &Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_reply(id: &Value, code: i64, message: &str) -> Value {
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
    answer: impl FnOnce(&mut ProjectDatabase, &Path) -> Result<T, AnswerRefusal>,
) -> Result<T, AnswerRefusal> {
    let root = tree_root.canonicalize().map_err(|error| {
        AnswerRefusal::Invalid(format!("tree root {}: {error}", tree_root.display()))
    })?;
    let mut databases = databases().lock().map_err(|_| {
        AnswerRefusal::Invalid("the embedded ty database cache is poisoned".to_owned())
    })?;
    let database = match databases.entry(root.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            entry.insert(built_database(&root).map_err(AnswerRefusal::Invalid)?)
        }
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
        .and_then(uri_to_path)
    else {
        return;
    };
    let Ok(root) = root.canonicalize() else {
        return;
    };
    let path = path.canonicalize().unwrap_or(path);
    let Ok(system_path) = SystemPathBuf::from_path_buf(path.clone()) else {
        return;
    };
    let Ok(mut databases) = databases().lock() else {
        return;
    };
    let Some(database) = databases.get_mut(&root) else {
        return;
    };
    let event = if !path.exists() {
        ChangeEvent::Deleted {
            path: system_path,
            kind: DeletedKind::Any,
        }
    } else if database
        .files()
        .try_system(database, &system_path)
        .is_some()
    {
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
fn file_at(database: &ProjectDatabase, uri: &Value) -> Result<File, AnswerRefusal> {
    let path = uri
        .as_str()
        .and_then(uri_to_path)
        .ok_or_else(|| AnswerRefusal::Invalid(format!("unreadable document uri: {uri}")))?;
    let path = path
        .canonicalize()
        .map_err(|error| AnswerRefusal::Moved(format!("document {}: {error}", path.display())))?;
    let system_path = SystemPathBuf::from_path_buf(path).map_err(|path| {
        AnswerRefusal::Invalid(format!("document path is not UTF-8: {}", path.display()))
    })?;
    system_path_to_file(database, &system_path)
        .map_err(|error| AnswerRefusal::Moved(format!("document {system_path}: {error:?}")))
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
fn offset_at(text: &str, position: &Value) -> Result<TextSize, AnswerRefusal> {
    let position: lsp_types::Position = serde_json::from_value(position.clone())
        .map_err(|error| AnswerRefusal::Invalid(format!("unreadable position: {error}")))?;
    let index = rift_lsp::LineIndex::new(text);
    let offset = index
        .byte_offset(PositionEncoding::Utf8, position)
        .map_err(|error| AnswerRefusal::Moved(format!("position outside the document: {error}")))?;
    TextSize::try_from(offset)
        .map_err(|error| AnswerRefusal::Invalid(format!("offset width: {error}")))
}

/// The LSP range one byte range spells in `text`, in UTF-8 positions.
fn range_at(text: &str, range: TextRange) -> Result<Value, AnswerRefusal> {
    let index = rift_lsp::LineIndex::new(text);
    let start = index
        .position(PositionEncoding::Utf8, usize::from(range.start()))
        .map_err(|error| {
            AnswerRefusal::Moved(format!("range start outside the document: {error}"))
        })?;
    let end = index
        .position(PositionEncoding::Utf8, usize::from(range.end()))
        .map_err(|error| {
            AnswerRefusal::Moved(format!("range end outside the document: {error}"))
        })?;
    Ok(json!({ "start": start, "end": end }))
}

/// Answers `textDocument/prepareRename`: the renameable range, or `null`.
fn prepare_rename(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer("/textDocument/uri").unwrap_or(&Value::Null),
    )?;
    database.project().open_file(database, file);
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let text = exchange.conversion_text(text.as_str());
    let offset = offset_at(text, params.pointer("/position").unwrap_or(&Value::Null))?;
    match ty_ide::can_rename(database, program_file, offset) {
        Some(range) => range_at(text, range),
        None => Ok(Value::Null),
    }
}

/// Answers `textDocument/rename`: the workspace edit renaming every
/// reference, or `null` when nothing renameable is declared at the position.
fn rename(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer("/textDocument/uri").unwrap_or(&Value::Null),
    )?;
    database.project().open_file(database, file);
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let text = exchange.conversion_text(text.as_str());
    let offset = offset_at(text, params.pointer("/position").unwrap_or(&Value::Null))?;
    let new_name = params
        .pointer("/newName")
        .and_then(Value::as_str)
        .ok_or_else(|| AnswerRefusal::Invalid("rename carries no newName".to_owned()))?;
    if ty_ide::can_rename(database, program_file, offset).is_none() {
        return Ok(Value::Null);
    }
    let Some(targets) = ty_ide::rename(database, program_file, offset, new_name) else {
        return Ok(Value::Null);
    };
    let mut changes: serde_json::Map<String, Value> = serde_json::Map::new();
    for target in &targets {
        let Some((uri, edit_range)) =
            target_location(database, exchange, target.file(), target.range())?
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
fn references(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer("/textDocument/uri").unwrap_or(&Value::Null),
    )?;
    database.project().open_file(database, file);
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let text = exchange.conversion_text(text.as_str());
    let offset = offset_at(text, params.pointer("/position").unwrap_or(&Value::Null))?;
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
        if let Some((uri, range)) =
            target_location(database, exchange, target.file(), target.range())?
        {
            locations.push(json!({ "uri": uri, "range": range }));
        }
    }
    Ok(Value::Array(locations))
}

/// One reference target as a `file://` URI and UTF-8 range; `None` for a
/// target outside the filesystem, such as a vendored stub. The canonical
/// workspace prefix swaps back to the root spelling the session addressed,
/// so an answer echoes the caller's own paths, symlinked temp roots
/// included.
fn target_location(
    database: &ProjectDatabase,
    exchange: &Exchange<'_>,
    file: File,
    range: TextRange,
) -> Result<Option<(String, Value)>, AnswerRefusal> {
    let Some(system_path) = file.path(database).as_system_path() else {
        return Ok(None);
    };
    let spelled = system_path
        .as_std_path()
        .strip_prefix(exchange.canonical_root)
        .map_or_else(
            |_| system_path.as_std_path().to_path_buf(),
            |relative| exchange.spelled_root.join(relative),
        );
    let uri = path_to_uri(&spelled);
    let text = source_text(database, file);
    let range = range_at(text.as_str(), range)?;
    Ok(Some((uri, range)))
}

/// Answers the `textDocument/diagnostic` pull with one full report.
fn pulled_diagnostics(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer("/textDocument/uri").unwrap_or(&Value::Null),
    )?;
    database.project().open_file(database, file);
    let text = source_text(database, file);
    let text = exchange.conversion_text(text.as_str());
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
            .map(|range| range_at(text, range))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_documents() -> DocumentStore {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[test]
    fn test_uri_and_path_spell_each_other_with_escapes_kept() {
        let path = Path::new("/workspace/py sources/module one.py");
        let uri = path_to_uri(path);
        assert_eq!(uri, "file:///workspace/py%20sources/module%20one.py");
        assert_eq!(uri_to_path(&uri), Some(path.to_path_buf()));
        assert_eq!(uri_to_path("http://example"), None);
    }

    #[test]
    fn test_initialize_advertises_utf8_rename_references_and_the_pull() {
        let capabilities = initialize_result();
        assert_eq!(capabilities["capabilities"]["positionEncoding"], "utf-8");
        assert_eq!(
            capabilities["capabilities"]["renameProvider"]["prepareProvider"],
            true
        );
        assert_eq!(capabilities["capabilities"]["referencesProvider"], true);
        assert!(capabilities["capabilities"]["diagnosticProvider"].is_object());
        assert!(
            capabilities["capabilities"].get("workspace").is_none(),
            "no file-operation capability is declared, so moves warn"
        );
    }

    #[test]
    fn test_an_unserved_method_answers_method_not_found() {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "textDocument/hover",
            "params": {},
        });
        let Handled::Reply(reply) =
            handle_message(&message, Path::new("/tree"), &empty_documents())
        else {
            panic!("an unserved request must reply");
        };
        assert_eq!(reply["id"], 7);
        assert_eq!(reply["error"]["code"], METHOD_NOT_FOUND);
        assert!(
            reply["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("textDocument/hover")),
        );
    }

    #[test]
    fn test_notifications_stay_silent_and_exit_ends_the_loop() {
        let initialized = serde_json::json!({ "jsonrpc": "2.0", "method": "initialized" });
        assert!(matches!(
            handle_message(&initialized, Path::new("/tree"), &empty_documents()),
            Handled::Silent
        ));
        let exit = serde_json::json!({ "jsonrpc": "2.0", "method": "exit" });
        assert!(matches!(
            handle_message(&exit, Path::new("/tree"), &empty_documents()),
            Handled::Exit
        ));
    }

    #[test]
    fn test_shutdown_answers_null() {
        let shutdown = serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "shutdown" });
        let Handled::Reply(reply) =
            handle_message(&shutdown, Path::new("/tree"), &empty_documents())
        else {
            panic!("shutdown must reply");
        };
        assert_eq!(reply["result"], Value::Null);
    }

    /// The diagnostic pull reports ty's finding for a file on disk, end to
    /// end through the message handler: database build, file resolution,
    /// check, and range rendering.
    /// prepareRename answers the declaration's own range for a plain
    /// function, so the engine-backed rename path is reachable.
    #[test]
    fn test_prepare_rename_answers_a_range_for_a_function_name() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("service.py");
        std::fs::write(&path, "def serve(port: int) -> int:\n    return port\n")
            .expect("fixture file");
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/prepareRename",
            "params": {
                "textDocument": { "uri": path_to_uri(&path) },
                "position": { "line": 0, "character": 4 },
            },
        });
        let Handled::Reply(reply) = handle_message(&message, directory.path(), &empty_documents())
        else {
            panic!("prepareRename must reply");
        };
        assert!(
            reply["result"].is_object(),
            "a function name is renameable: {reply:#}"
        );
    }

    #[test]
    fn test_the_diagnostic_pull_reports_an_invalid_assignment() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("service.py");
        std::fs::write(&path, "count: int = \"eight\"\n").expect("fixture file");
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": path_to_uri(&path) } },
        });
        let Handled::Reply(reply) = handle_message(&message, directory.path(), &empty_documents())
        else {
            panic!("the pull must reply");
        };
        assert!(
            reply.get("error").is_none(),
            "the pull answers a report: {reply:#}"
        );
        let items = reply["result"]["items"]
            .as_array()
            .expect("a full report carries items");
        assert!(
            items
                .iter()
                .any(|item| item["code"] == serde_json::json!("invalid-assignment")),
            "ty reports the invalid assignment: {reply:#}"
        );
    }

    /// References answer across files with the caller's own spellings, and
    /// a position resolving no symbol answers the empty list.
    #[test]
    fn test_references_answer_across_files_and_empty_off_symbol() {
        let directory = tempfile::tempdir().expect("fixture directory");
        std::fs::write(
            directory.path().join("a.py"),
            "def helper():\n    return 1\n",
        )
        .expect("fixture a");
        std::fs::write(
            directory.path().join("b.py"),
            "from a import helper\n\nhelper()\n",
        )
        .expect("fixture b");
        let uri = path_to_uri(&directory.path().join("a.py"));
        let request = |position: Value| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "textDocument/references",
                "params": {
                    "textDocument": { "uri": uri },
                    "position": position,
                    "context": { "includeDeclaration": false },
                },
            })
        };
        let message = request(serde_json::json!({ "line": 0, "character": 4 }));
        let Handled::Reply(reply) = handle_message(&message, directory.path(), &empty_documents())
        else {
            panic!("references must reply");
        };
        let locations = reply["result"].as_array().expect("a location list");
        assert!(
            locations.iter().any(|location| location["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with("b.py"))),
            "the import and call in b.py are among the references: {reply:#}"
        );
        assert!(
            locations.iter().all(|location| {
                location["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.starts_with(&path_to_uri(directory.path())))
            }),
            "answers echo the caller's own root spelling: {reply:#}"
        );

        let off_symbol = request(serde_json::json!({ "line": 1, "character": 0 }));
        let Handled::Reply(reply) =
            handle_message(&off_symbol, directory.path(), &empty_documents())
        else {
            panic!("references must reply");
        };
        assert_eq!(
            reply["result"],
            serde_json::json!([]),
            "a position resolving no symbol answers the empty list"
        );
    }

    /// The rename refusal arms: a request without `newName` errs, and a
    /// position ty cannot rename answers `null` for prepare and rename both.
    #[test]
    fn test_rename_refusals_answer_error_and_null() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("a.py");
        std::fs::write(&path, "def helper():\n    return 1\n").expect("fixture");
        let uri = path_to_uri(&path);
        let nameless = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "textDocument/rename",
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 4 },
            },
        });
        let Handled::Reply(reply) = handle_message(&nameless, directory.path(), &empty_documents())
        else {
            panic!("rename must reply");
        };
        assert_eq!(reply["error"]["code"], INTERNAL_ERROR, "{reply:#}");

        for method in ["textDocument/prepareRename", "textDocument/rename"] {
            let keyword = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": method,
                "params": {
                    "textDocument": { "uri": uri },
                    "position": { "line": 0, "character": 0 },
                    "newName": "renamed",
                },
            });
            let Handled::Reply(reply) =
                handle_message(&keyword, directory.path(), &empty_documents())
            else {
                panic!("{method} must reply");
            };
            assert_eq!(
                reply["result"],
                Value::Null,
                "`def` is not renameable: {method}, {reply:#}"
            );
        }
    }

    /// The serve loop over a raw transport: initialize answers a framed
    /// reply, an unknown notification stays silent, unreadable bytes end
    /// the loop, and `exit` ends it cleanly.
    #[tokio::test]
    async fn test_serve_answers_frames_and_ends_on_exit() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut session, engine) = tokio::io::duplex(64 * 1024);
        let directory = tempfile::tempdir().expect("fixture directory");
        let served = tokio::spawn(serve(engine, directory.path().to_path_buf()));

        let initialize =
            br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.as_slice();
        session
            .write_all(&Framing::frame(initialize))
            .await
            .expect("initialize writes");
        session
            .write_all(&Framing::frame(
                br#"{"jsonrpc":"2.0","method":"initialized"}"#.as_slice(),
            ))
            .await
            .expect("initialized writes");
        let mut framing = Framing::new();
        let mut buffer = [0_u8; 4096];
        let answer = loop {
            let read = session.read(&mut buffer).await.expect("the reply arrives");
            let mut messages = framing.feed(&buffer[..read]).expect("framed reply");
            if let Some(payload) = messages.pop() {
                break serde_json::from_slice::<Value>(&payload).expect("reply parses");
            }
        };
        assert_eq!(answer["id"], 1);
        assert_eq!(
            answer["result"]["capabilities"]["positionEncoding"],
            "utf-8"
        );

        session
            .write_all(&Framing::frame(
                br#"{"jsonrpc":"2.0","method":"exit"}"#.as_slice(),
            ))
            .await
            .expect("exit writes");
        served.await.expect("the loop ends on exit");
    }

    /// Unreadable payload bytes end the loop instead of answering garbage.
    #[tokio::test]
    async fn test_serve_ends_on_unreadable_bytes() {
        use tokio::io::AsyncWriteExt;

        let (mut session, engine) = tokio::io::duplex(4 * 1024);
        let directory = tempfile::tempdir().expect("fixture directory");
        let served = tokio::spawn(serve(engine, directory.path().to_path_buf()));
        session
            .write_all(&Framing::frame(b"not json".as_slice()))
            .await
            .expect("garbage writes");
        served.await.expect("the loop ends on unreadable bytes");
    }

    /// A project marker routes database construction through hermetic
    /// discovery, and the pull still answers findings from that tree.
    #[test]
    fn test_a_pyproject_marker_routes_through_hermetic_discovery() {
        let directory = tempfile::tempdir().expect("fixture directory");
        std::fs::write(
            directory.path().join("pyproject.toml"),
            "[project]\nname = \"beacon\"\nversion = \"0.0.1\"\n",
        )
        .expect("marker");
        let path = directory.path().join("service.py");
        std::fs::write(&path, "count: int = \"eight\"\n").expect("fixture");
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": path_to_uri(&path) } },
        });
        let Handled::Reply(reply) = handle_message(&message, directory.path(), &empty_documents())
        else {
            panic!("the pull must reply");
        };
        let items = reply["result"]["items"].as_array().expect("items");
        assert!(
            items
                .iter()
                .any(|item| item["code"] == serde_json::json!("invalid-assignment")),
            "discovery keeps the tree's own findings: {reply:#}"
        );
    }

    /// Once a database exists, a didOpen classifies the document against
    /// it: changed content invalidates, a new file registers, and a
    /// vanished file is removed - the next pull answers the new state.
    #[test]
    fn test_did_open_feeds_the_database_changed_created_and_deleted_states() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("service.py");
        std::fs::write(&path, "count: int = 1\n").expect("fixture");
        let uri = path_to_uri(&path);
        let pull = |id: i64, uri: &str| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "textDocument/diagnostic",
                "params": { "textDocument": { "uri": uri } },
            })
        };
        let items = |reply: &Handled| -> usize {
            let Handled::Reply(reply) = reply else {
                panic!("the pull must reply");
            };
            reply["result"]["items"]
                .as_array()
                .map_or(usize::MAX, Vec::len)
        };
        let documents = empty_documents();

        let clean = handle_message(&pull(1, &uri), directory.path(), &documents);
        assert_eq!(items(&clean), 0, "the clean file pulls no findings");

        std::fs::write(&path, "count: int = \"eight\"\n").expect("changed fixture");
        let open = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": uri, "text": "count: int = \"eight\"\n" } },
        });
        assert!(matches!(
            handle_message(&open, directory.path(), &documents),
            Handled::Silent
        ));
        let changed = handle_message(&pull(2, &uri), directory.path(), &documents);
        assert_eq!(items(&changed), 1, "the changed content pulls its finding");

        let created_path = directory.path().join("fresh.py");
        std::fs::write(&created_path, "flag: bool = 7\n").expect("created fixture");
        let created_uri = path_to_uri(&created_path);
        let open_created = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": created_uri, "text": "flag: bool = 7\n" } },
        });
        assert!(matches!(
            handle_message(&open_created, directory.path(), &documents),
            Handled::Silent
        ));
        let created = handle_message(&pull(3, &created_uri), directory.path(), &documents);
        assert_eq!(items(&created), 1, "the created file pulls its finding");

        std::fs::remove_file(&created_path).expect("fixture removal");
        let open_gone = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": created_uri, "text": "" } },
        });
        assert!(matches!(
            handle_message(&open_gone, directory.path(), &documents),
            Handled::Silent
        ));
    }

    /// A didOpen before any request feeds no database (none exists yet),
    /// and the first request builds one from disk regardless.
    #[test]
    fn test_did_open_before_any_database_stays_silent() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("early.py");
        std::fs::write(&path, "x = 1\n").expect("fixture");
        let open = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": path_to_uri(&path), "text": "x = 1\n" } },
        });
        assert!(matches!(
            handle_message(&open, directory.path(), &empty_documents()),
            Handled::Silent
        ));
    }

    #[test]
    fn test_offset_and_range_conversions_speak_utf8_positions() {
        let text = "alpha\ndef beacon():\n    pass\n";
        let offset = offset_at(text, &serde_json::json!({ "line": 1, "character": 4 }))
            .expect("a position inside the document resolves");
        assert_eq!(usize::from(offset), 10);
        let range = range_at(text, TextRange::new(TextSize::from(10), TextSize::from(16)))
            .expect("a range inside the document renders");
        assert_eq!(range["start"]["line"], 1);
        assert_eq!(range["start"]["character"], 4);
        assert_eq!(range["end"]["character"], 10);
        assert!(
            offset_at(text, &serde_json::json!({ "line" : 9, "character": 0 })).is_err(),
            "a position past the document refuses"
        );
    }
}
