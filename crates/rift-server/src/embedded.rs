//! The embedded ty engine: Python semantics served in process.
//!
//! [`started_session`] opens a standard [`EngineSession`] over an in-memory
//! duplex transport whose far end a spawned task serves, speaking the LSP
//! base protocol backed by the linked-in ty crates. Every consumption site
//! keeps its one engine contract: capability negotiation, settlement, and
//! position encoding run exactly as they do over a spawned process, so
//! references and diagnostics need no embedded-specific path.
//!
//! The served surface is the subset the server itself asks engines for:
//! `initialize`, document open and close, `textDocument/references`, call
//! hierarchy (`textDocument/prepareCallHierarchy` and
//! `callHierarchy/outgoingCalls`), and the `textDocument/diagnostic` pull.
//! Ranges cross the wire in UTF-8 positions, the encoding the answer
//! advertises.
//!
//! ty analyzes the tree on disk: project discovery runs only when the tree
//! carries a `pyproject.toml` or `ty.toml` marker, and a document open feeds the
//! database the file's current on-disk state. The session hands indexed source
//! that the server has already witnessed against disk, so the two views agree by
//! the time an exchange runs. The Python environment comes from the tree alone:
//! the one its ty configuration names, else its `.venv`, else none.

mod hermetic;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use rift_dependency::{PROJECT_ENVIRONMENT_DIRECTORY, PROJECT_ENVIRONMENT_MARKER, SitePackages};
use rift_lsp::{EngineError, EngineLaunch, EngineSession, Framing, PositionEncoding};
use ruff_db::Db as _;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use ty_project::metadata::options::{EnvironmentOptions, Options};
use ty_project::metadata::value::RelativePathBuf;
use ty_project::watch::{ChangeEvent, ChangedKind, CreatedKind, DeletedKind};
use ty_project::{Db as _, ProjectDatabase, ProjectMetadata, SemanticDb as _};

use crate::dependency::{FilesystemInputs, ResolutionPolicy};
use hermetic::HermeticSystem;

/// Bytes the in-memory transport buffers per direction before a writer waits.
const DUPLEX_BYTES: usize = 256 * 1024;

/// Bytes one framed payload from the session may hold; a change tool's
/// document open carries the whole file, so the bound tracks the largest
/// source the Python provider accepts.
const PAYLOAD_BYTES_MAX: usize = 8 * 1024 * 1024;

/// The scheme and empty authority ty's `VendoredPath` display puts before a vendored
/// stub's path.
const VENDORED_URI_PREFIX: &str = "vendored://";

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
/// ty answers each request on demand against the tree's database and
/// announces no work-done progress, so the session is declared ready at its
/// start ([`EngineSession::declare_ready`]) instead of reading unconfirmed
/// until a `settle_delay` passes, and stays ready across a file change.
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
    let mut session =
        EngineSession::start_over_transport(launch, workspace_root, client, tokio::io::empty())
            .await?;
    session.declare_ready();
    Ok(session)
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

        ("textDocument/references", Some(id)) => {
            answered(&id, root, documents, &params, DOCUMENT_URI, references)
        }
        ("textDocument/prepareCallHierarchy", Some(id)) => answered(
            &id,
            root,
            documents,
            &params,
            DOCUMENT_URI,
            prepared_call_hierarchy,
        ),
        ("callHierarchy/outgoingCalls", Some(id)) => {
            answered(&id, root, documents, &params, ITEM_URI, outgoing_calls)
        }
        ("textDocument/diagnostic", Some(id)) => answered(
            &id,
            root,
            documents,
            &params,
            DOCUMENT_URI,
            pulled_diagnostics,
        ),
        (_, Some(id)) => Handled::Reply(error_reply(
            &id,
            METHOD_NOT_FOUND,
            &format!("the embedded ty engine does not serve {method}"),
        )),
    }
}

/// Where a document request names its document.
const DOCUMENT_URI: &str = "/textDocument/uri";
/// Where a call hierarchy request names the file of the item it re-sends.
const ITEM_URI: &str = "/item/uri";

/// Runs one answer against the tree's database and wraps it as a reply.
///
/// `uri_pointer` names the document whose sent text the answer converts
/// positions against.
fn answered(
    id: &Value,
    root: &Path,
    documents: &DocumentStore,
    params: &Value,
    uri_pointer: &str,
    answer: fn(&mut ProjectDatabase, &Exchange<'_>, &Value) -> Result<Value, AnswerRefusal>,
) -> Handled {
    let sent = params
        .pointer(uri_pointer)
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

/// The capabilities this engine advertises: UTF-8 positions, references,
/// call hierarchy, and the diagnostic pull. No file-operation capability is
/// declared, so moves warn instead of asking.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "positionEncoding": "utf-8",
            "referencesProvider": true,
            "callHierarchyProvider": true,
            "diagnosticProvider": {
                "interFileDependencies": true,
                "workspaceDiagnostics": false,
            },
        },
        "serverInfo": { "name": "ty (embedded)" },
    })
}

/// One tree's database and the project environment it was built under.
struct TreeDatabase {
    database: ProjectDatabase,
    environment: TreeEnvironment,
}

/// The process-wide database cache, keyed by canonical tree root: one
/// workspace server serves one tree, and a replaced session reuses the
/// database its predecessor built.
fn databases() -> &'static Mutex<HashMap<PathBuf, TreeDatabase>> {
    static DATABASES: OnceLock<Mutex<HashMap<PathBuf, TreeDatabase>>> = OnceLock::new();
    DATABASES.get_or_init(Mutex::default)
}

/// Runs `answer` against the tree's database, building it on first use.
/// The lock is held for the whole call, which serializes semantic work per
/// process; every answer extracts owned data before returning.
///
/// Each call observes the tree's project environment once and rebuilds the
/// database when the observation differs from the one it was built under, so
/// a `.venv` created, removed, or recreated after the first request, and a
/// distribution installed into it, takes effect on the next one. A failed
/// rebuild keeps the previous database and refuses the call; the next call
/// tries again.
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
    let environment = TreeEnvironment::observe(&root);
    let tree = match databases.entry(root.clone()) {
        Entry::Occupied(entry) if entry.get().environment == environment => entry.into_mut(),
        Entry::Occupied(mut entry) => {
            entry.insert(TreeDatabase::built(&root, environment)?);
            entry.into_mut()
        }
        Entry::Vacant(entry) => entry.insert(TreeDatabase::built(&root, environment)?),
    };
    answer(&mut tree.database, &root)
}

impl TreeDatabase {
    /// Builds a project database for the tree at `root` under `environment`.
    ///
    /// The database runs on a [`HermeticSystem`], so the process's variables,
    /// programs, and user configuration never reach it, and ty's project
    /// discovery reads the tree's own `pyproject.toml` or `ty.toml` and nothing
    /// above the root: the project is the served tree. Imports resolve through
    /// the Python environment the tree's ty configuration names, else through
    /// the tree's `.venv` when it holds a virtual environment, as
    /// [`TreeEnvironment::options`] states, else through none: the project's own
    /// modules and the vendored typeshed standard library. ty's look for an
    /// environment around its own executable finds none, since the Rift binary
    /// is named neither `ty` nor a Python interpreter.
    fn built(root: &Path, environment: TreeEnvironment) -> Result<Self, AnswerRefusal> {
        let system_root = SystemPathBuf::from_path_buf(root.to_path_buf()).map_err(|path| {
            AnswerRefusal::Invalid(format!("tree root is not UTF-8: {}", path.display()))
        })?;
        let system = HermeticSystem::new(&system_root);
        let mut metadata = ProjectMetadata::discover_without_uv(&system_root, &system)
            .map_err(|error| AnswerRefusal::Invalid(format!("ty project discovery: {error}")))?;
        if let Some(options) = environment.options(&system_root) {
            metadata.apply_fallback_options(options);
        }
        let database = ProjectDatabase::fallible(metadata, system)
            .map_err(|error| AnswerRefusal::Invalid(format!("ty database: {error}")))?;
        Ok(Self {
            database,
            environment,
        })
    }
}

/// The tree's project environment as one request observed it.
///
/// The dependency resolver reads the tree's installed packages from its project
/// environment, `.venv`, and the interpreter version from that environment's
/// `pyvenv.cfg`, so an engine resolving imports through another environment names callees
/// in files no package the resolver found holds. A `.venv` without `pyvenv.cfg` holds no
/// virtual environment and names none. A marked environment is observed the way the
/// resolver observes it, through [`SitePackages::observe`], so a distribution `uv sync`
/// installs, removes, or upgrades changes the observation as it changes the resolver's.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TreeEnvironment {
    /// No `pyvenv.cfg` stands in the tree's `.venv`.
    Absent,
    /// `.venv/pyvenv.cfg` stands.
    Marked {
        /// When the marker was last modified, as the filesystem reports it; `None` on a
        /// filesystem that reports none.
        marker_modified: Option<SystemTime>,
        /// The environment's `site-packages` and its listing; `None` when neither layout
        /// stands.
        site_packages: Option<SitePackages>,
    },
}

impl TreeEnvironment {
    /// Observes the tree's `.venv`: one metadata read of its marker and, when it is
    /// marked, the resolver's `site-packages` listing.
    fn observe(root: &Path) -> Self {
        let marker = root
            .join(PROJECT_ENVIRONMENT_DIRECTORY)
            .join(PROJECT_ENVIRONMENT_MARKER);
        match std::fs::metadata(marker) {
            Ok(metadata) if metadata.is_file() => Self::Marked {
                marker_modified: metadata.modified().ok(),
                site_packages: SitePackages::observe(
                    root,
                    &mut FilesystemInputs::new(ResolutionPolicy::default()),
                ),
            },
            Ok(_) | Err(_) => Self::Absent,
        }
    }

    /// Options naming the tree's `.venv` as ty's Python environment when it is marked;
    /// `None` otherwise. The options sit below the project's own ty configuration, which
    /// still names another environment when it sets one.
    fn options(&self, root: &SystemPath) -> Option<Options> {
        (*self != Self::Absent).then(|| Options {
            environment: Some(EnvironmentOptions {
                python: Some(RelativePathBuf::cli(
                    root.join(PROJECT_ENVIRONMENT_DIRECTORY),
                )),
                ..EnvironmentOptions::default()
            }),
            ..Options::default()
        })
    }
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
    let Some(TreeDatabase { database, .. }) = databases.get_mut(&root) else {
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
///
/// The session spells a Windows document `file:///C:/dir/file.py`, whose path
/// is `C:\dir\file.py`: the drive is the URI path's first segment.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let uri = url::Url::parse(uri).ok()?;
    if uri.scheme() != "file" {
        return None;
    }
    uri.to_file_path().ok()
}

/// The `file://` URI one absolute path spells, percent-escaping what RFC 3986
/// keeps out of a path segment; a Windows drive becomes the first segment.
fn path_to_uri(path: &Path) -> Result<String, AnswerRefusal> {
    url::Url::from_file_path(path)
        .map(String::from)
        .map_err(|()| {
            AnswerRefusal::Invalid(format!("path spells no file URI: {}", path.display()))
        })
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
    let uri = path_to_uri(&spelled)?;
    let text = source_text(database, file);
    let range = range_at(text.as_str(), range)?;
    Ok(Some((uri, range)))
}

/// Answers `textDocument/prepareCallHierarchy`: one item per callable
/// definition at the position, and `null` off a function, method, or class.
fn prepared_call_hierarchy(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(
        database,
        params.pointer(DOCUMENT_URI).unwrap_or(&Value::Null),
    )?;
    database.project().open_file(database, file);
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let text = exchange.conversion_text(text.as_str());
    let offset = offset_at(text, params.pointer("/position").unwrap_or(&Value::Null))?;
    let items = ty_ide::prepare_call_hierarchy(database, program_file, offset).unwrap_or_default();
    let mut prepared = Vec::with_capacity(items.len());
    for item in &items {
        if let Some(item) = hierarchy_item(database, exchange, item)? {
            prepared.push(item);
        }
    }
    Ok(if prepared.is_empty() {
        Value::Null
    } else {
        Value::Array(prepared)
    })
}

/// Answers `callHierarchy/outgoingCalls` for an item this engine prepared.
///
/// The item is found again from its URI and the start of its selection
/// range, the key `ty_ide::outgoing_calls` reads. Each call's `fromRanges`
/// sit in the item's own file.
fn outgoing_calls(
    database: &mut ProjectDatabase,
    exchange: &Exchange<'_>,
    params: &Value,
) -> Result<Value, AnswerRefusal> {
    let file = file_at(database, params.pointer(ITEM_URI).unwrap_or(&Value::Null))?;
    database.project().open_file(database, file);
    let program_file = database.program_file(file);
    let text = source_text(database, file);
    let offset = offset_at(
        exchange.conversion_text(text.as_str()),
        params
            .pointer("/item/selectionRange/start")
            .unwrap_or(&Value::Null),
    )?;
    let mut calls = Vec::new();
    for call in ty_ide::outgoing_calls(database, program_file, offset) {
        let Some(to) = hierarchy_item(database, exchange, &call.to)? else {
            continue;
        };
        let from_ranges = call
            .from_ranges
            .iter()
            .map(|range| range_at(text.as_str(), *range))
            .collect::<Result<Vec<_>, _>>()?;
        calls.push(json!({ "to": to, "fromRanges": from_ranges }));
    }
    Ok(Value::Array(calls))
}

/// One `ty_ide` call hierarchy item in its LSP spelling; `None` for a file
/// that is neither on the filesystem nor a vendored stub.
///
/// A vendored typeshed stub keeps its `vendored://stdlib/<path>` spelling as
/// the URI, so the server can name the standard library module it belongs to.
fn hierarchy_item(
    database: &ProjectDatabase,
    exchange: &Exchange<'_>,
    item: &ty_ide::CallHierarchyItem,
) -> Result<Option<Value>, AnswerRefusal> {
    let text = source_text(database, item.file);
    let (uri, range) = match item.file.path(database).as_vendored_path() {
        Some(vendored) => (
            vendored_uri(vendored),
            range_at(text.as_str(), item.full_range)?,
        ),
        None => match target_location(database, exchange, item.file, item.full_range)? {
            Some(location) => location,
            None => return Ok(None),
        },
    };
    Ok(Some(json!({
        "name": item.name.as_str(),
        "kind": symbol_kind(item.kind),
        "detail": item.detail,
        "uri": uri,
        "range": range,
        "selectionRange": range_at(text.as_str(), item.selection_range)?,
    })))
}

/// A vendored stub's URI, its path segments joined with `/`.
///
/// ty joins a vendored path with the host's separator, so on Windows the stub reads
/// `stdlib\builtins.pyi`, and a URI spelled from it the way `VendoredPath` displays it
/// is refused by every URI parser the answer meets.
fn vendored_uri(path: &ruff_db::vendored::VendoredPath) -> String {
    let segments: Vec<&str> = path
        .components()
        .map(|component| component.as_str())
        .collect();
    format!("{VENDORED_URI_PREFIX}{}", segments.join("/"))
}

/// The LSP symbol kind for a `ty_ide` one, as `ty_server` maps it.
fn symbol_kind(kind: ty_ide::SymbolKind) -> lsp_types::SymbolKind {
    match kind {
        ty_ide::SymbolKind::Module | ty_ide::SymbolKind::Import => lsp_types::SymbolKind::MODULE,
        ty_ide::SymbolKind::Class => lsp_types::SymbolKind::CLASS,
        ty_ide::SymbolKind::Method => lsp_types::SymbolKind::METHOD,
        ty_ide::SymbolKind::Function => lsp_types::SymbolKind::FUNCTION,
        ty_ide::SymbolKind::Variable | ty_ide::SymbolKind::Parameter => {
            lsp_types::SymbolKind::VARIABLE
        }
        ty_ide::SymbolKind::Constant => lsp_types::SymbolKind::CONSTANT,
        ty_ide::SymbolKind::Property => lsp_types::SymbolKind::PROPERTY,
        ty_ide::SymbolKind::Field => lsp_types::SymbolKind::FIELD,
        ty_ide::SymbolKind::Constructor => lsp_types::SymbolKind::CONSTRUCTOR,
        ty_ide::SymbolKind::TypeParameter => lsp_types::SymbolKind::TYPE_PARAMETER,
    }
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
    use ruff_db::system::System as _;

    use super::*;

    fn empty_documents() -> DocumentStore {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// The URI an absolute path spells in this module's tests.
    fn uri_of(path: &Path) -> String {
        path_to_uri(path).expect("an absolute path spells a file URI")
    }

    /// ty joins a vendored stub's path with the host's separator, as `join` does here, and
    /// the URI joins its segments with `/` on every host.
    #[test]
    fn test_a_vendored_stub_uri_joins_its_segments_with_a_slash() {
        let stub = ruff_db::vendored::VendoredPath::new("stdlib").join("builtins.pyi");
        assert_eq!(
            vendored_uri(stub.as_path()),
            "vendored://stdlib/builtins.pyi"
        );
    }

    #[test]
    fn test_a_tree_with_a_virtual_environment_names_it_as_the_python_environment() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let root = SystemPathBuf::from_path_buf(directory.path().to_path_buf())
            .expect("a UTF-8 temporary root");
        let observed = || TreeEnvironment::observe(directory.path());
        assert_eq!(observed(), TreeEnvironment::Absent);
        assert!(
            observed().options(&root).is_none(),
            "a tree without `.venv` names no environment"
        );

        std::fs::create_dir(directory.path().join(".venv")).expect("environment directory");
        assert_eq!(
            observed(),
            TreeEnvironment::Absent,
            "a `.venv` without `pyvenv.cfg` is no virtual environment"
        );

        std::fs::write(
            directory.path().join(".venv/pyvenv.cfg"),
            "home = /usr/bin\n",
        )
        .expect("environment marker");
        let marked = observed();
        assert!(
            matches!(marked, TreeEnvironment::Marked { .. }),
            "{marked:?}"
        );
        let options = marked
            .options(&root)
            .expect("a marked `.venv` names the environment");
        let python = options
            .environment
            .and_then(|environment| environment.python)
            .expect("the options name a Python environment");
        assert_eq!(python.path(), root.join(".venv").as_path());
    }

    /// A module importing `inside`, which a tree's own `.venv` installs, and `outside`,
    /// which only the environment `VIRTUAL_ENV` names installs.
    const IMPORTS: &str = "import inside\nimport outside\n";

    /// The `site-packages` directory below a virtual environment, in ty's layout for
    /// this host.
    const SITE_PACKAGES: &str = if cfg!(windows) {
        "Lib/site-packages"
    } else {
        "lib/python3.12/site-packages"
    };

    /// Installs `package` 1.0.0 into the environment at `environment` as `uv sync` lays
    /// one out: its module folder and its `.dist-info` with a `RECORD`.
    fn installed_package(environment: &Path, package: &str) {
        let site_packages = environment.join(SITE_PACKAGES);
        let module = site_packages.join(package);
        std::fs::create_dir_all(&module).expect("site-packages directory");
        std::fs::write(
            module.join("__init__.py"),
            "def greet() -> str:\n    return \"\"\n",
        )
        .expect("package module");
        let dist_info = site_packages.join(format!("{package}-1.0.0.dist-info"));
        std::fs::create_dir_all(&dist_info).expect("metadata directory");
        std::fs::write(
            dist_info.join("RECORD"),
            format!("{package}/__init__.py,,\n{package}-1.0.0.dist-info/RECORD,,\n"),
        )
        .expect("package record");
    }

    /// Writes a virtual environment at `environment` holding the one package `package`.
    fn installed_environment(environment: &Path, package: &str) {
        installed_package(environment, package);
        std::fs::write(
            environment.join(PROJECT_ENVIRONMENT_MARKER),
            format!("home = {}\nversion_info = 3.12.4\n", environment.display()),
        )
        .expect("environment marker");
    }

    /// A tree holding `app.py` with [`IMPORTS`], at its canonical path.
    fn importing_tree() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("fixture directory");
        std::fs::write(directory.path().join("app.py"), IMPORTS).expect("fixture module");
        let root = directory.path().canonicalize().expect("canonical root");
        (directory, root)
    }

    /// The `unresolved-import` messages among `diagnostics`.
    fn unresolved_imports<'item>(
        diagnostics: impl IntoIterator<Item = (&'item str, &'item str)>,
    ) -> Vec<&'item str> {
        diagnostics
            .into_iter()
            .filter(|(code, _)| *code == "unresolved-import")
            .map(|(_, message)| message)
            .collect()
    }

    /// Whether one of `messages` names the module `module`.
    fn names_module(messages: &[impl AsRef<str>], module: &str) -> bool {
        let quoted = format!("`{module}`");
        messages
            .iter()
            .any(|message| message.as_ref().contains(&quoted))
    }

    /// The `unresolved-import` messages for `app.py` from a database ty builds by
    /// default for the tree at `root`: its own discovery on the OS system.
    fn default_unresolved_imports(root: &Path) -> Vec<String> {
        let system_root =
            SystemPathBuf::from_path_buf(root.to_path_buf()).expect("a UTF-8 temporary root");
        let system = ruff_db::system::OsSystem::new(&system_root);
        let metadata =
            ProjectMetadata::discover_without_uv(&system_root, &system).expect("ty's discovery");
        let database = ProjectDatabase::fallible(metadata, system).expect("a default database");
        let app = system_path_to_file(&database, system_root.join("app.py")).expect("app.py");
        let findings: Vec<(String, String)> = database
            .check_file(app)
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic.id().to_string(),
                    diagnostic.concise_message().to_string(),
                )
            })
            .collect();
        unresolved_imports(
            findings
                .iter()
                .map(|(code, message)| (code.as_str(), message.as_str())),
        )
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    /// The engine's `unresolved-import` messages for the tree's `app.py`, as the
    /// diagnostic pull answers them.
    fn engine_unresolved_imports(root: &Path) -> Vec<String> {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri_of(&root.join("app.py")) } },
        });
        let Handled::Reply(reply) = handle_message(&message, root, &empty_documents()) else {
            panic!("the pull must reply");
        };
        let items = reply["result"]["items"].as_array().expect("items");
        let pairs: Vec<(&str, &str)> = items
            .iter()
            .map(|item| {
                (
                    item["code"].as_str().unwrap_or_default(),
                    item["message"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        unresolved_imports(pairs)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    /// Runs in a child process whose `VIRTUAL_ENV` names an environment installing
    /// `outside`. A database on the OS system, as ty builds one by default, resolves
    /// `outside` through it, so the variable names a live environment; the engine
    /// resolves no import through it, and a tree whose `.venv` installs `inside`
    /// resolves that one alone.
    #[test]
    #[ignore = "probe run by test_another_environment_named_by_virtual_env_stays_unread in a child process"]
    fn test_another_environment_named_by_virtual_env_stays_unread_probe() {
        let named = std::env::var("VIRTUAL_ENV").expect("the parent names an environment");
        let (_bare, bare_root) = importing_tree();
        let default_unresolved = default_unresolved_imports(&bare_root);
        assert!(
            !names_module(&default_unresolved, "outside"),
            "ty's own discovery reads `{named}`: {default_unresolved:?}"
        );
        let system_root =
            SystemPathBuf::from_path_buf(bare_root.clone()).expect("a UTF-8 temporary root");
        assert_eq!(
            HermeticSystem::new(&system_root).env_var("VIRTUAL_ENV"),
            Err(std::env::VarError::NotPresent)
        );

        let bare = engine_unresolved_imports(&bare_root);
        assert!(names_module(&bare, "inside"), "{bare:?}");
        assert!(
            names_module(&bare, "outside"),
            "no import resolves through `{named}`: {bare:?}"
        );

        let (_installed, installed_root) = importing_tree();
        installed_environment(
            &installed_root.join(PROJECT_ENVIRONMENT_DIRECTORY),
            "inside",
        );
        let installed = engine_unresolved_imports(&installed_root);
        assert!(
            !names_module(&installed, "inside"),
            "the tree's `.venv` resolves `inside`: {installed:?}"
        );
        assert!(names_module(&installed, "outside"), "{installed:?}");
    }

    /// A `.venv` created after the first request takes effect on the next one, and so
    /// does its removal.
    #[test]
    fn test_a_tree_environment_created_or_removed_after_a_request_takes_effect() {
        let (_tree, root) = importing_tree();
        let environment = root.join(PROJECT_ENVIRONMENT_DIRECTORY);
        let before = engine_unresolved_imports(&root);
        assert!(names_module(&before, "inside"), "{before:?}");

        installed_environment(&environment, "inside");
        let created = engine_unresolved_imports(&root);
        assert!(
            !names_module(&created, "inside"),
            "the new `.venv` resolves `inside`: {created:?}"
        );

        std::fs::remove_dir_all(&environment).expect("environment removal");
        let removed = engine_unresolved_imports(&root);
        assert!(
            names_module(&removed, "inside"),
            "the removed `.venv` resolves nothing: {removed:?}"
        );
    }

    /// A distribution installed into the tree's existing `.venv` after the first request
    /// resolves on the next one, though the environment's `pyvenv.cfg` stays untouched.
    #[test]
    fn test_a_package_installed_into_the_tree_environment_resolves_on_the_next_request() {
        let (_tree, root) = importing_tree();
        let environment = root.join(PROJECT_ENVIRONMENT_DIRECTORY);
        installed_environment(&environment, "inside");
        let marker_modified = || {
            std::fs::metadata(environment.join(PROJECT_ENVIRONMENT_MARKER))
                .and_then(|metadata| metadata.modified())
                .expect("marker time")
        };
        let marker = marker_modified();
        let before = engine_unresolved_imports(&root);
        assert!(!names_module(&before, "inside"), "{before:?}");
        assert!(names_module(&before, "outside"), "{before:?}");

        installed_package(&environment, "outside");
        assert_eq!(
            marker_modified(),
            marker,
            "installing leaves `pyvenv.cfg` alone"
        );
        let after = engine_unresolved_imports(&root);
        assert!(
            !names_module(&after, "outside"),
            "the installed `outside` resolves: {after:?}"
        );
    }

    /// An ancestor's `ty.toml` never reaches the engine. ty's own discovery roots the
    /// project at the ancestor and resolves `outside` through its `extra-paths`; the
    /// engine's project stays the served tree, whose plain `pyproject.toml` names none.
    #[test]
    fn test_an_ancestor_ty_configuration_stays_unread() {
        let parent = tempfile::tempdir().expect("fixture directory");
        let parent_root = parent.path().canonicalize().expect("canonical parent");
        std::fs::write(
            parent_root.join("ty.toml"),
            "[environment]\nextra-paths = [\"extra\"]\n",
        )
        .expect("ancestor configuration");
        let outside = parent_root.join("extra/outside");
        std::fs::create_dir_all(&outside).expect("extra path");
        std::fs::write(outside.join("__init__.py"), "value = 1\n").expect("extra module");
        let root = parent_root.join("tree");
        std::fs::create_dir(&root).expect("tree directory");
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname = \"tree\"\nversion = \"0.0.1\"\n",
        )
        .expect("tree marker");
        std::fs::write(root.join("app.py"), IMPORTS).expect("fixture module");

        let default_unresolved = default_unresolved_imports(&root);
        assert!(
            !names_module(&default_unresolved, "outside"),
            "ty's own discovery reads the ancestor's `ty.toml`: {default_unresolved:?}"
        );
        let engine = engine_unresolved_imports(&root);
        assert!(
            names_module(&engine, "outside"),
            "the engine reads no configuration above the tree: {engine:?}"
        );
    }

    #[test]
    fn test_another_environment_named_by_virtual_env_stays_unread() {
        let environment = tempfile::tempdir().expect("environment directory");
        installed_environment(environment.path(), "outside");
        let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([
                "--exact",
                "embedded::tests::test_another_environment_named_by_virtual_env_stays_unread_probe",
                "--ignored",
            ])
            .env("VIRTUAL_ENV", environment.path())
            .output()
            .expect("the probe runs");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "probe must pass: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn test_uri_and_path_spell_each_other_with_escapes_kept() {
        let path = std::env::temp_dir()
            .join("py sources")
            .join("module one.py");
        let uri = uri_of(&path);
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.ends_with("/py%20sources/module%20one.py"), "{uri}");
        assert_eq!(uri_to_path(&uri), Some(path));
        assert_eq!(uri_to_path("http://example"), None);
        assert!(path_to_uri(Path::new("relative.py")).is_err());
    }

    /// The session addresses a document through `rift-lsp`'s [`rift_lsp::TreeRoot`],
    /// and the engine reads back the file that URI was spelled from. On Windows the
    /// root spells as `file:///C:/...`, which a strip of `file://` read as `/C:/...`.
    #[test]
    fn test_a_session_document_uri_names_the_file_it_was_spelled_from() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let root = rift_lsp::TreeRoot::new(directory.path()).expect("an absolute root");
        let project_path =
            rift_core::ProjectPath::new("pkg/service.py").expect("a valid project path");
        let uri = root
            .document_uri(&project_path)
            .expect("the root spells a document URI");
        assert_eq!(
            uri_to_path(uri.as_str()),
            Some(directory.path().join("pkg").join("service.py"))
        );
    }

    #[test]
    fn test_initialize_advertises_utf8_references_and_the_pull() {
        let capabilities = initialize_result();
        assert_eq!(capabilities["capabilities"]["positionEncoding"], "utf-8");
        assert_eq!(capabilities["capabilities"]["referencesProvider"], true);
        assert_eq!(capabilities["capabilities"]["callHierarchyProvider"], true);
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

    #[test]
    fn test_the_diagnostic_pull_reports_an_invalid_assignment() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let path = directory.path().join("service.py");
        std::fs::write(&path, "count: int = \"eight\"\n").expect("fixture file");
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "textDocument/diagnostic",
            "params": { "textDocument": { "uri": uri_of(&path) } },
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
        let uri = uri_of(&directory.path().join("a.py"));
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
                    .is_some_and(|uri| uri.starts_with(&uri_of(directory.path())))
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

    /// Prepare anchors a function and answers `null` off one; outgoing calls
    /// name a callee in the tree by its file URI and a builtin by its
    /// vendored typeshed path, each with the call site in the seed's file.
    #[test]
    fn test_call_hierarchy_prepares_a_function_and_names_its_callees() {
        let directory = tempfile::tempdir().expect("fixture directory");
        std::fs::write(
            directory.path().join("a.py"),
            "def helper():\n    return 1\n",
        )
        .expect("fixture a");
        std::fs::write(
            directory.path().join("b.py"),
            "from a import helper\n\ndef total():\n    return helper() + len([])\n",
        )
        .expect("fixture b");
        let uri = uri_of(&directory.path().join("b.py"));
        let prepare = |position: Value| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "textDocument/prepareCallHierarchy",
                "params": { "textDocument": { "uri": uri }, "position": position },
            })
        };
        let Handled::Reply(prepared) = handle_message(
            &prepare(serde_json::json!({ "line": 2, "character": 4 })),
            directory.path(),
            &empty_documents(),
        ) else {
            panic!("prepare must reply");
        };
        let item = prepared["result"][0].clone();
        assert_eq!(item["name"], "total", "{prepared:#}");
        assert_eq!(
            item["kind"],
            serde_json::json!(lsp_types::SymbolKind::FUNCTION)
        );
        assert_eq!(item["uri"], serde_json::json!(uri));

        let outgoing = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "callHierarchy/outgoingCalls",
            "params": { "item": item },
        });
        let Handled::Reply(reply) = handle_message(&outgoing, directory.path(), &empty_documents())
        else {
            panic!("outgoing calls must reply");
        };
        let calls = reply["result"].as_array().expect("a call list");
        let helper = calls
            .iter()
            .find(|call| call["to"]["name"] == "helper")
            .unwrap_or_else(|| panic!("helper is a callee: {reply:#}"));
        assert_eq!(
            helper["to"]["uri"],
            serde_json::json!(uri_of(&directory.path().join("a.py")))
        );
        assert_eq!(
            helper["fromRanges"][0]["start"],
            serde_json::json!({ "line": 3, "character": 11 })
        );
        assert!(
            calls.iter().any(|call| call["to"]["name"] == "len"
                && call["to"]["uri"] == "vendored://stdlib/builtins.pyi"),
            "a builtin keeps its vendored stub path: {reply:#}"
        );

        let Handled::Reply(off_function) = handle_message(
            &prepare(serde_json::json!({ "line": 1, "character": 0 })),
            directory.path(),
            &empty_documents(),
        ) else {
            panic!("prepare must reply");
        };
        assert_eq!(off_function["result"], Value::Null);
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

    /// A project marker routes database construction through project
    /// discovery, and the pull still answers findings from that tree.
    #[test]
    fn test_a_pyproject_marker_routes_through_project_discovery() {
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
            "params": { "textDocument": { "uri": uri_of(&path) } },
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
        let uri = uri_of(&path);
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
        let created_uri = uri_of(&created_path);
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
            "params": { "textDocument": { "uri": uri_of(&path), "text": "x = 1\n" } },
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
