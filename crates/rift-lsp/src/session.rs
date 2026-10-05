//! One language engine child, spoken to over LSP stdio.
//!
//! The session is request-scoped and sequential: it reads the engine's
//! stdout only while a call runs, answers server-initiated requests inline,
//! and retains published diagnostics in a bounded record. It also reads the
//! engine's `$/progress` traffic, so the holder can ask whether the engine
//! was still analyzing when it answered, and whether it has ever announced
//! any work at all. Every wait is bounded by a timeout, and an engine that
//! overstays one is killed and reaped - a child is never left unobserved.
//! The child starts from the environment the server inherited, with the
//! launch's `environment` entries laid on top. The session records the
//! documents it holds open and whether a frame is part-written, so a holder
//! whose exchange was dropped midway can tell whether the session still
//! serves ([`EngineSession::is_intact`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use lsp_types::error_codes::{CONTENT_MODIFIED, SERVER_CANCELLED};
use lsp_types::notification::{
    DidChangeWatchedFiles, DidCloseTextDocument, DidOpenTextDocument, Exit, Initialized,
    Notification, Progress, PublishDiagnostics,
};
use lsp_types::request::{
    CallHierarchyOutgoingCalls, CallHierarchyPrepare, DocumentDiagnosticRequest, Initialize,
    References, RegisterCapability, Request, Shutdown, WorkDoneProgressCreate,
    WorkspaceConfiguration, WorkspaceDiagnosticRefresh,
};
use lsp_types::{
    CallHierarchyItem, CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams,
    CallHierarchyPrepareParams, ConfigurationParams, Diagnostic, DidChangeWatchedFilesParams,
    DidChangeWatchedFilesRegistrationOptions, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, FileChangeType, FileEvent, FileSystemWatcher, GlobPattern,
    InitializeParams, InitializedParams, Location, PartialResultParams, Position, ProgressParams,
    ProgressParamsValue, ProgressToken, PublishDiagnosticsParams, ReferenceContext,
    ReferenceParams, RegistrationParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, WatchKind, WorkDoneProgress, WorkDoneProgressCreateParams,
    WorkDoneProgressParams, WorkspaceFolder,
};
use rift_core::{CapturedStream, ProjectPath, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};
use rift_error::{RiftError, errors};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::Instant;

use crate::capabilities::{Capabilities, glob_matches, offered};
use crate::correlation::{self, Correlation, METHOD_NOT_FOUND_CODE, RequestId};
use crate::framing::Framing;
use crate::uri::TreeRoot;

/// Wall-clock bound on the shutdown request and on the exit wait.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum documents with retained published diagnostics; later documents
/// are dropped.
const PUBLISHED_DOCUMENTS_MAX: usize = 64;

/// Maximum diagnostics retained per document; later entries are dropped.
const DOCUMENT_DIAGNOSTICS_MAX: usize = 256;

/// Maximum documents the session records as opened and not yet closed.
///
/// A holder opens the documents one exchange asks about and closes them before the
/// exchange returns, and it closes what a dropped exchange left open before the next
/// one begins ([`EngineSession::close_open_documents`]), so a live session holds the
/// documents of at most two exchanges: the running one and the one a drop left behind.
/// The bound leaves room for exchanges that open several. A document opened past it
/// goes unrecorded, and the session stops reading as intact
/// ([`EngineSession::is_intact`]), since it can no longer close everything it opened.
pub const OPEN_DOCUMENTS_MAX: usize = 64;

/// Maximum `workspace/configuration` items answered with `null` each.
const CONFIGURATION_ITEMS_MAX: usize = 256;

/// Maximum work-done progress tokens retained as outstanding at once.
///
/// The record only has to answer whether any work runs, so a token past
/// the bound is dropped: the record is already non-empty, and the session
/// already reads as analyzing.
const PROGRESS_TOKENS_MAX: usize = 64;

/// Maximum watched-file glob patterns retained across every
/// `client/registerCapability` call for `workspace/didChangeWatchedFiles`.
///
/// A pattern past the bound is dropped: the record already has enough
/// patterns to match against, and one more registration changes nothing a
/// caller can observe beyond which paths a notification names.
const WATCHED_FILE_WATCHERS_MAX: usize = 64;

/// Maximum registrations one `client/registerCapability` call contributes.
const WATCHED_FILE_REGISTRATIONS_MAX: usize = 16;

/// The refusal codes that name a transient condition, not a bad request.
///
/// `SERVER_CANCELLED` (-32802) is the engine cancelling a request it
/// serves cancellably, which the specification tells the client to
/// retrigger; `CONTENT_MODIFIED` (-32801) is an answer made outdated by a
/// document change, which the client re-issues. `REQUEST_CANCELLED`
/// (-32800) is the client's own cancellation and every other code is the
/// engine's verdict on the request itself, so neither is resent.
const RETRYABLE_REFUSAL_CODES: [i64; 2] = [SERVER_CANCELLED, CONTENT_MODIFIED];

/// How to start one engine child: the executable and the bounds it runs
/// under.
///
/// The program is never empty, never an absolute
/// path, looked up through the child's `PATH`.
#[derive(Clone, Debug)]
pub struct EngineLaunch {
    /// The executable name; an absolute executable path is refused.
    pub program: String,
    /// Arguments handed to the program.
    pub arguments: Vec<String>,
    /// Environment entries laid over the inherited environment.
    pub environment: BTreeMap<String, String>,
    /// Options handed to the engine in the initialize request, verbatim.
    pub initialization_options: Option<Value>,
    /// Wall-clock bound on the initialize handshake.
    pub startup_timeout: Duration,
    /// Wall-clock bound on each later request.
    pub request_timeout: Duration,
    /// Quiet the work record holds, up to the session's last read of engine
    /// output, before the session calls the engine ready.
    pub settle_delay: Duration,
    /// Bytes of standard error kept as the captured prefix.
    pub stderr_capture_bytes: usize,
}

/// Whether an error means session cannot serve another exchange.
fn ends_session(error: &RiftError) -> bool {
    let slug = error.slug();
    slug == errors::lsp::engine_connection_closed::SLUG
        || slug == errors::lsp::engine_timed_out::SLUG
        || slug == errors::lsp::engine_message_unreadable::SLUG
        || slug == errors::lsp::framing_header_too_long::SLUG
        || slug == errors::lsp::framing_message_too_long::SLUG
        || slug == errors::lsp::framing_header_malformed::SLUG
        || slug == errors::lsp::framing_content_length_missing::SLUG
        || slug == errors::lsp::framing_content_length_invalid::SLUG
        || slug == errors::lsp::correlation_pending_requests_exceeded::SLUG
        || slug == errors::lsp::correlation_response_unknown::SLUG
}

/// What the engine has said about its own work over `$/progress`.
///
/// Two facts ride here: which tokens are outstanding right now, and whether
/// engine work completed since workspace bytes last changed. Changed bytes
/// invalidate earlier readiness without discarding outstanding tokens.
/// What one engine has proven about its own settlement, read from its
/// `$/progress` traffic alone and independent of what language it serves.
///
/// Initialization completing is not enough on its own: every work-done
/// progress token the engine announced must also have ended before an
/// answer is trusted as settled. Announcing work alone never confirms
/// readiness - it names [`EngineReadiness::Analyzing`], not
/// [`EngineReadiness::Ready`] - and an engine that announces no work after
/// latest changed bytes reaches neither: it stays
/// [`EngineReadiness::Unconfirmed`], a state rather than a failure, because
/// nothing distinguishes it from one still loading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineReadiness {
    /// The engine has announced no work since latest invalidation. Its answers may
    /// be the answers of a settled engine, or of one that has not started
    /// analyzing yet; nothing here tells the two apart.
    Unconfirmed,
    /// The engine announced work it has not yet ended. Its most recent
    /// answer is provisional.
    Analyzing,
    /// The engine announced work and every token it began has since ended,
    /// or it declared itself ready at its start and has no work outstanding
    /// ([`EngineSession::declare_ready`]).
    Ready,
}

/// One bounded document diagnostic report from a language engine.
#[derive(Clone, Debug, PartialEq)]
pub struct PulledDiagnostics {
    items: Vec<Diagnostic>,
    result_id: Option<String>,
    full: bool,
}

impl PulledDiagnostics {
    /// Findings carried by this report.
    #[must_use]
    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    /// Result id carried by this report, when the engine supplied one.
    #[must_use]
    pub fn result_id(&self) -> Option<&str> {
        self.result_id.as_deref()
    }

    /// Whether this answer carries a full report.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.full
    }
}

/// One document's most recently published diagnostics, and the version
/// the engine said they describe.
///
/// `textDocument/publishDiagnostics` carries an optional `version`; an
/// engine that omits it is retained with `version: None`, indistinguishable
/// from a document no publish has named at all - both mean no version
/// evidence, never evidence the document is current.
#[derive(Clone, Debug, PartialEq)]
struct PublishedReport {
    diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
}

/// Maps an LSP file event to its registered watcher flag.
fn watch_kind(change: FileChangeType) -> Option<WatchKind> {
    match change {
        value if value == FileChangeType::CREATED => Some(WatchKind::Create),
        value if value == FileChangeType::CHANGED => Some(WatchKind::Change),
        value if value == FileChangeType::DELETED => Some(WatchKind::Delete),
        _ => None,
    }
}

/// The prefix of the progress token rust-analyzer names its `cargo check`
/// with: `format!("rust-analyzer/flycheck/{id}")` in
/// `crates/rust-analyzer/src/main_loop.rs` at rust-analyzer 1.98.1.
///
/// Call hierarchy answers do not wait for the check, so a walk's readiness
/// leaves these tokens out while a diagnostics read still waits for them.
const FLYCHECK_TOKEN_PREFIX: &str = "rust-analyzer/flycheck/";

/// Whether one token names rust-analyzer's `cargo check`.
fn is_flycheck(token: &ProgressToken) -> bool {
    matches!(token, ProgressToken::String(name) if name.starts_with(FLYCHECK_TOKEN_PREFIX))
}

#[derive(Debug, Default)]
struct WorkProgress {
    announced: bool,
    outstanding: Vec<ProgressToken>,
    last_transition: Option<Instant>,
    last_read: Option<Instant>,
    first_read: Option<Instant>,
    /// Announcement and last transition over every token but the flycheck ones.
    walk_announced: bool,
    walk_transition: Option<Instant>,
    /// The engine declared itself ready at its start: it answers each
    /// request on demand, so with nothing announced it reads ready, not
    /// unconfirmed. A change leaves the declaration in place.
    declared_ready: bool,
}

impl WorkProgress {
    /// Records one token as outstanding, bounded.
    ///
    /// A token already outstanding stays as it is, and a new token past
    /// [`PROGRESS_TOKENS_MAX`] is dropped: the record is already
    /// non-empty, so the session reads as analyzing either way, and an
    /// end for the dropped token retires nothing. The announcement holds
    /// whatever the bound does with the token.
    fn began(&mut self, token: ProgressToken, now: Instant) {
        self.announced = true;
        self.last_transition = Some(now);
        if !is_flycheck(&token) {
            self.walk_announced = true;
            self.walk_transition = Some(now);
        }
        if self.outstanding.len() >= PROGRESS_TOKENS_MAX || self.outstanding.contains(&token) {
            return;
        }
        self.outstanding.push(token);
    }

    /// Retires one token the engine ended.
    fn ended(&mut self, token: &ProgressToken, now: Instant) {
        let held = self.outstanding.len();
        self.outstanding.retain(|held| held != token);
        if self.outstanding.len() < held {
            self.announced = true;
            self.last_transition = Some(now);
            if !is_flycheck(token) {
                self.walk_announced = true;
                self.walk_transition = Some(now);
            }
        }
    }

    /// Invalidates settlement after workspace bytes change.
    ///
    /// Outstanding work remains outstanding. With no outstanding token,
    /// next answer needs fresh progress or repeated report evidence, unless
    /// the engine declared itself ready: an engine that answers on demand
    /// reads the change at its next request, so waiting changes no answer.
    fn invalidated(&mut self, now: Instant) {
        self.announced = false;
        self.last_transition = Some(now);
        self.walk_announced = false;
        self.walk_transition = Some(now);
    }

    /// What an engine that announced nothing reads: ready when it declared
    /// itself ready at its start, unconfirmed otherwise.
    fn unannounced(&self) -> EngineReadiness {
        if self.declared_ready {
            EngineReadiness::Ready
        } else {
            EngineReadiness::Unconfirmed
        }
    }

    /// Stamps the moment the session last read the engine's own output.
    ///
    /// Settlement is read past, never observed from outside: the record
    /// answers ready only once the session has read engine bytes at least
    /// one settle delay after the record's last transition.
    fn read(&mut self, now: Instant) {
        self.first_read.get_or_insert(now);
        self.last_read = Some(now);
    }

    /// The readiness a call hierarchy walk reads: [`WorkProgress::readiness`]
    /// with the flycheck tokens left out.
    fn walk_readiness(&self, settle_delay: Duration) -> EngineReadiness {
        if self.outstanding.iter().any(|token| !is_flycheck(token)) {
            EngineReadiness::Analyzing
        } else if !self.walk_announced {
            self.unannounced()
        } else if self.walk_is_quiet(settle_delay) {
            EngineReadiness::Ready
        } else {
            EngineReadiness::Analyzing
        }
    }

    /// Whether the session has read engine output at least `settle_delay`
    /// after the walk's last transition, or after its first read when
    /// nothing was announced since the session started.
    fn walk_is_quiet(&self, settle_delay: Duration) -> bool {
        let (Some(since), Some(read)) = (self.walk_transition.or(self.first_read), self.last_read)
        else {
            return false;
        };
        read.saturating_duration_since(since) >= settle_delay
    }

    /// The next moment after `now` at which a read could prove a settle
    /// delay of quiet: one settle delay past the last transition, the walk's
    /// last transition, or the first read. `None` when every such moment has
    /// passed.
    fn next_quiet_boundary(&self, now: Instant, settle_delay: Duration) -> Option<Instant> {
        [self.last_transition, self.walk_transition, self.first_read]
            .into_iter()
            .flatten()
            .map(|since| since + settle_delay)
            .filter(|boundary| *boundary > now)
            .min()
    }

    /// Whether the record has stayed empty for `settle_delay`, up to the
    /// session's last read of engine output.
    ///
    /// A load's phases end and begin in bursts, so the set empties several
    /// times inside one load. An emptiness the session has not read past
    /// proves nothing, and neither does one shorter than the engine's own
    /// silence between phases.
    fn is_settled(&self, settle_delay: Duration) -> bool {
        let (Some(transition), Some(read)) = (self.last_transition, self.last_read) else {
            return false;
        };
        read.saturating_duration_since(transition) >= settle_delay
    }

    /// Whether any announced work is still outstanding.
    fn is_outstanding(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// Whether engine work has been announced since latest invalidation.
    fn is_announced(&self) -> bool {
        self.announced
    }

    /// The readiness this record proves: what was announced, what is
    /// outstanding, and how long the record has been quiet.
    fn readiness(&self, settle_delay: Duration) -> EngineReadiness {
        if self.is_outstanding() {
            EngineReadiness::Analyzing
        } else if !self.is_announced() {
            self.unannounced()
        } else if self.is_settled(settle_delay) {
            EngineReadiness::Ready
        } else {
            EngineReadiness::Analyzing
        }
    }
}

/// What the session's most recent answer said.
///
/// Every request clears `latest` before it is sent, so a verdict cannot
/// outlive the answer that set it. Reference requests set the verdict.
#[derive(Debug, Default)]
struct EmptyAnswers {
    latest: bool,
}

impl EmptyAnswers {
    /// Forgets the previous answer's verdict; every request starts here.
    fn forget(&mut self) {
        self.latest = false;
    }

    /// Records whether the reference request returned no locations.
    fn record(&mut self, empty: bool) {
        self.latest = empty;
    }

    /// Whether latest operation answered nothing.
    fn is_empty(&self) -> bool {
        self.latest
    }
}

/// The engine's writable half: a real child's stdin, or a test transport's
/// own write half.
type EngineWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// The engine's readable half: a real child's stdout, or a test transport's
/// own read half.
type EngineReader = Box<dyn AsyncRead + Send + Unpin>;

impl std::ops::Deref for PulledDiagnostics {
    type Target = [Diagnostic];

    fn deref(&self) -> &Self::Target {
        &self.items
    }
}

/// One running engine and the conversation state around it.
///
/// `child` is `Some` for a spawned process and `None` for a session started
/// over an already-connected transport; `EngineSession::end` and
/// [`EngineSession::shutdown`] only wait on and kill a process that exists.
pub struct EngineSession {
    child: Option<Child>,
    stdin: EngineWriter,
    stdout: EngineReader,
    stderr_drain: tokio::task::JoinHandle<CapturedStream>,
    framing: Framing,
    correlation: Correlation,
    queue: VecDeque<Vec<u8>>,
    capabilities: Capabilities,
    root: TreeRoot,
    request_timeout: Duration,
    settle_delay: Duration,
    published: BTreeMap<ProjectPath, PublishedReport>,
    progress: WorkProgress,
    diagnostic_refresh_revision: u64,
    empty_answers: EmptyAnswers,
    watched_file_watchers: Vec<FileSystemWatcher>,
    document_version: i32,
    open_documents: BTreeSet<ProjectPath>,
    open_documents_unrecorded: bool,
    frame_in_flight: bool,
    ended: bool,
}

impl std::fmt::Debug for EngineSession {
    /// Prints the session's observable state; the boxed byte streams carry
    /// no `Debug` impl of their own and are omitted.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineSession")
            .field("child_pid", &self.child.as_ref().and_then(Child::id))
            .field("capabilities", &self.capabilities)
            .field("root", &self.root)
            .field("document_version", &self.document_version)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl EngineSession {
    /// Spawns the engine and completes the initialize handshake.
    ///
    /// The child inherits the server's environment with the launch's
    /// `environment` entries laid on top, runs inside `workspace_root`, and
    /// must answer initialize within `startup_timeout` or it is killed.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for a refused program, a failed spawn, or a
    /// failed handshake; the child never outlives the failure.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future mid-handshake drops the child with kill-on-drop
    /// armed: the runtime kills and reaps it in the background.
    pub async fn start(launch: EngineLaunch, workspace_root: &Path) -> Result<Self, RiftError> {
        refuse_program(&launch.program)?;
        let mut command = Command::new(&launch.program);
        command
            .args(&launch.arguments)
            .envs(&launch.environment)
            .current_dir(workspace_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|source| errors::lsp::engine_launch_failed().source(source).error())?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill().await;
            return errors::lsp::engine_launch_failed()
                .source(std::io::Error::other("child pipes were not handed over"))
                .fail();
        };
        let stderr_drain = tokio::spawn(drain(stderr, launch.stderr_capture_bytes));
        Self::assembled(
            Some(child),
            Box::new(stdin),
            Box::new(stdout),
            stderr_drain,
            workspace_root,
            launch,
        )
        .await
    }

    /// Completes the handshake over an already-connected transport, with a
    /// synthetic standard-error stream and no process to spawn, wait, or
    /// kill.
    ///
    /// Exercises framing, correlation, capability negotiation, and the
    /// standard-error drain from an exchange-level test that owns both ends
    /// of the byte stream. `launch.program`, `launch.arguments`, and
    /// `launch.environment` are never read; only the handshake and capture
    /// bounds apply. Production sessions always start through
    /// [`EngineSession::start`].
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] under the same conditions as
    /// [`EngineSession::start`], minus a failed spawn.
    pub async fn start_over_transport(
        launch: EngineLaunch,
        workspace_root: &Path,
        transport: impl AsyncRead + AsyncWrite + Send + 'static,
        stderr: impl AsyncRead + Send + Unpin + 'static,
    ) -> Result<Self, RiftError> {
        let (read_half, write_half) = tokio::io::split(transport);
        let stderr_drain = tokio::spawn(drain(stderr, launch.stderr_capture_bytes));
        Self::assembled(
            None,
            Box::new(write_half),
            Box::new(read_half),
            stderr_drain,
            workspace_root,
            launch,
        )
        .await
    }

    /// Builds the session over an acquired transport and runs the
    /// handshake, ending the session and propagating the failure if it
    /// fails. Shared by [`EngineSession::start`] and
    /// [`EngineSession::start_over_transport`], the only two constructors.
    async fn assembled(
        child: Option<Child>,
        stdin: EngineWriter,
        stdout: EngineReader,
        stderr_drain: tokio::task::JoinHandle<CapturedStream>,
        workspace_root: &Path,
        launch: EngineLaunch,
    ) -> Result<Self, RiftError> {
        let root = TreeRoot::new(workspace_root)?;
        let mut session = Self {
            child,
            stdin,
            stdout,
            stderr_drain,
            framing: Framing::new(),
            correlation: Correlation::new(),
            queue: VecDeque::new(),
            capabilities: Capabilities::default(),
            root,
            request_timeout: launch.request_timeout,
            settle_delay: launch.settle_delay,
            published: BTreeMap::new(),
            progress: WorkProgress::default(),
            diagnostic_refresh_revision: 0,
            empty_answers: EmptyAnswers::default(),
            watched_file_watchers: Vec::new(),
            document_version: 0,
            open_documents: BTreeSet::new(),
            open_documents_unrecorded: false,
            frame_in_flight: false,
            ended: false,
        };
        if let Err(error) = session
            .handshake(
                workspace_root,
                launch.startup_timeout,
                launch.initialization_options,
            )
            .await
        {
            session.end().await;
            return error.fail();
        }
        Ok(session)
    }

    /// What the engine advertised at initialize.
    #[must_use]
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// The root anchoring this session's document URIs.
    #[must_use]
    pub fn root(&self) -> &TreeRoot {
        &self.root
    }

    /// The version the most recent `didOpen` carried. Published diagnostics
    /// name this version when the engine reports it.
    #[must_use]
    pub fn document_version(&self) -> i32 {
        self.document_version
    }

    /// Whether the session already killed its engine.
    ///
    /// An ended session refuses every later operation; the holder decides
    /// whether to start a replacement.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Whether the session can serve another exchange after one was dropped midway.
    ///
    /// Three facts hold: the engine still runs, no frame is left part-written on
    /// its input, and every document the session holds open is on record, so
    /// [`EngineSession::close_open_documents`] can close it. A dropped request
    /// leaves its response pending, and the next exchange discards that response
    /// when it arrives. A frame cut while it was being written leaves the engine
    /// reading the next frame as the rest of the cut one, and no later exchange can
    /// repair that.
    #[must_use]
    pub fn is_intact(&self) -> bool {
        !self.ended && !self.frame_in_flight && !self.open_documents_unrecorded
    }

    /// Whether the engine began work-done progress it has not ended.
    ///
    /// An engine loading a project reports that work over `$/progress`,
    /// beginning a token through `window/workDoneProgress/create` and
    /// ending it when the work is done. An answer the engine gives while
    /// a token is outstanding is provisional: the same request may answer
    /// differently once the work ends, so the holder may send it again.
    ///
    /// The record only holds what the session has read, and the session
    /// reads only while a call runs, so the query answers the state as of
    /// the most recent answer. An engine that reports no progress at all
    /// never reads as analyzing, and its answers are final at once. An
    /// engine between two of its own tokens reads as analyzing too, until
    /// the record has been quiet for `lsp.settle_delay`.
    /// Equivalent to `readiness() == EngineReadiness::Analyzing`.
    #[must_use]
    pub fn is_analyzing(&self) -> bool {
        self.readiness() == EngineReadiness::Analyzing
    }

    /// What this session has proven about the engine's own settlement.
    ///
    /// Reads only what the session has read so far, so the answer is as of
    /// the most recent exchange; see [`EngineReadiness`] for what each
    /// state means and [`EngineSession::is_analyzing`] for the analyzing
    /// half of it alone.
    ///
    /// `Ready` needs the work record empty and quiet for the launch's
    /// `settle_delay`, measured up to the session's last read of engine
    /// output. A language server ends and begins its load phases in bursts,
    /// so the record empties several times inside one load; emptiness at one
    /// instant is not settlement.
    #[must_use]
    pub fn readiness(&self) -> EngineReadiness {
        self.progress.readiness(self.settle_delay)
    }

    /// What this session has proven for a call hierarchy walk:
    /// [`EngineSession::readiness`] with rust-analyzer's `cargo check`
    /// tokens (`rust-analyzer/flycheck/<id>`) left out, since call
    /// hierarchy answers do not wait for the check.
    #[must_use]
    pub fn walk_readiness(&self) -> EngineReadiness {
        self.progress.walk_readiness(self.settle_delay)
    }

    /// Whether the session has read engine output at least `settle_delay`
    /// after the last transition a walk counts, or after its first read
    /// when the engine announced nothing: the quiet an
    /// [`EngineReadiness::Unconfirmed`] engine must show before a walk
    /// trusts its answer.
    #[must_use]
    pub fn walk_is_quiet(&self) -> bool {
        self.progress.walk_is_quiet(self.settle_delay)
    }

    /// Records that the engine answers each request on demand from its
    /// current state, so it reads [`EngineReadiness::Ready`] from its start
    /// with no work to announce and no `settle_delay` to wait out.
    ///
    /// The declaration survives a file change: the engine reads the
    /// change at its next request, so after the change the session still
    /// reads [`EngineReadiness::Ready`] unless the engine announces work.
    pub fn declare_ready(&mut self) {
        self.progress.declared_ready = true;
    }

    /// Revision of diagnostic refresh requests received from engine.
    ///
    /// A changed revision invalidates an earlier pull even when both reports
    /// carry equal findings.
    #[must_use]
    pub fn diagnostic_refresh_revision(&self) -> u64 {
        self.diagnostic_refresh_revision
    }

    /// Whether latest operation answered nothing.
    ///
    /// Caller retry policy bounds resends. Progress describes engine work,
    /// but does not bind that work to one semantic request, so it cannot
    /// make an empty answer final.
    #[must_use]
    pub fn latest_answer_is_empty(&self) -> bool {
        self.empty_answers.is_empty()
    }

    /// Opens one document with the text the caller hands in.
    ///
    /// The session records the document as open once the notification is
    /// written, and [`EngineSession::close`] removes it from the record.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended or the engine's side
    /// of the connection broke.
    ///
    /// # Cancel safety
    ///
    /// The notification is written in one frame. Dropping the future while that
    /// frame is being written leaves the session not intact
    /// ([`EngineSession::is_intact`]); the document is recorded open only once the
    /// frame is written.
    pub async fn open(
        &mut self,
        path: &ProjectPath,
        language_id: &str,
        text: String,
    ) -> Result<(), RiftError> {
        let uri = self.document_uri(path)?;
        self.document_version += 1;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: language_id.to_owned(),
                version: self.document_version,
                text,
            },
        };
        self.notify::<DidOpenTextDocument>(&params).await?;
        self.record_open(path);
        Ok(())
    }

    /// Records `path` as opened, or marks the record incomplete past
    /// [`OPEN_DOCUMENTS_MAX`].
    fn record_open(&mut self, path: &ProjectPath) {
        if self.open_documents.len() >= OPEN_DOCUMENTS_MAX && !self.open_documents.contains(path) {
            self.open_documents_unrecorded = true;
            return;
        }
        self.open_documents.insert(path.clone());
    }

    /// Closes one previously opened document.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended or the engine's side
    /// of the connection broke.
    ///
    /// # Cancel safety
    ///
    /// The notification is written in one frame. Dropping the future while that
    /// frame is being written leaves the session not intact
    /// ([`EngineSession::is_intact`]); the document stays recorded open until the
    /// frame is written, so [`EngineSession::close_open_documents`] closes it again.
    pub async fn close(&mut self, path: &ProjectPath) -> Result<(), RiftError> {
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: self.document_uri(path)?,
            },
        };
        self.notify::<DidCloseTextDocument>(&params).await?;
        self.open_documents.remove(path);
        Ok(())
    }

    /// Closes every document the session records as open, in path order.
    ///
    /// A holder calls this before an exchange begins, so documents an earlier,
    /// dropped exchange opened and never closed stop holding the engine to the
    /// bytes that exchange sent: while a document is open, the engine takes its
    /// content from the session and not from disk. With nothing recorded, nothing
    /// is sent. At most [`OPEN_DOCUMENTS_MAX`] notifications go out.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended or the engine's side of the
    /// connection broke.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves every document not yet closed on record, so a
    /// later call closes it; one dropped while a frame is being written leaves the
    /// session not intact ([`EngineSession::is_intact`]).
    pub async fn close_open_documents(&mut self) -> Result<(), RiftError> {
        let open: Vec<ProjectPath> = self.open_documents.iter().cloned().collect();
        for path in &open {
            self.close(path).await?;
        }
        Ok(())
    }

    /// The locations the engine names for the declaration at one position, its own
    /// occurrence included.
    ///
    /// `context.include_declaration` is `true`, so an engine that resolved the declaration
    /// names at least that declaration's own occurrence. The caller separates it from the
    /// references, and reads an answer naming nothing at all as the answer of an engine
    /// that does not hold the file, never as proof the declaration is unreferenced. An
    /// engine answering `null` names nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when references are not advertised or the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call discards the engine's
    /// stale response. Dropped while the request's frame is being written, it leaves the
    /// session not intact ([`EngineSession::is_intact`]).
    pub async fn references(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Vec<Location>, RiftError> {
        require(self.capabilities.references, References::METHOD)?;
        let params = ReferenceParams {
            text_document_position: self.position_params(path, position)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: true,
            },
        };
        let locations = self.request::<References>(params).await?;
        let locations = locations.unwrap_or_default();
        self.empty_answers.record(locations.is_empty());
        Ok(locations)
    }

    /// The call hierarchy items the engine prepares at one position: the
    /// callable declarations there, none off one. An engine answering `null`
    /// prepares nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when call hierarchy is not advertised or the
    /// exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call discards
    /// the engine's stale response. Dropped while the request's frame is being
    /// written, it leaves the session not intact ([`EngineSession::is_intact`]).
    pub async fn prepare_call_hierarchy(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Vec<CallHierarchyItem>, RiftError> {
        require(
            self.capabilities.call_hierarchy,
            CallHierarchyPrepare::METHOD,
        )?;
        let params = CallHierarchyPrepareParams {
            text_document_position_params: self.position_params(path, position)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let items = self.request::<CallHierarchyPrepare>(params).await?;
        Ok(items.unwrap_or_default())
    }

    /// The calls one prepared item makes: each callee, and the ranges of its
    /// calls inside the item. An engine answering `null` names no call.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when call hierarchy is not advertised or the
    /// exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call discards
    /// the engine's stale response. Dropped while the request's frame is being
    /// written, it leaves the session not intact ([`EngineSession::is_intact`]).
    pub async fn outgoing_calls(
        &mut self,
        item: CallHierarchyItem,
    ) -> Result<Vec<CallHierarchyOutgoingCall>, RiftError> {
        require(
            self.capabilities.call_hierarchy,
            CallHierarchyOutgoingCalls::METHOD,
        )?;
        let params = CallHierarchyOutgoingCallsParams {
            item,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let calls = self.request::<CallHierarchyOutgoingCalls>(params).await?;
        Ok(calls.unwrap_or_default())
    }

    /// Reads engine output until `until`, or until `settled` holds: the wait
    /// between two attempts, spent reading instead of sleeping.
    ///
    /// Progress is stamped when it arrives rather than at the next request,
    /// and the engine's own requests are answered while the caller waits. A
    /// stretch with no output counts as read up to the moment the wait saw it
    /// end, since the session was reading the whole time. The wait wakes one
    /// settle delay past the last transition, so a caller waiting for
    /// readiness stops there instead of at its next scheduled attempt.
    ///
    /// A response read here belongs to the next exchange, which settles it:
    /// it goes back to the queue unread, and the wait sleeps out the rest of
    /// its time, so the attempt that reads it keeps its place in the retry
    /// schedule.
    ///
    /// Each iteration consumes one engine message or ends at a wake-up, so
    /// the loop is bounded by what the engine sends before `until`.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended or the engine's side of
    /// the connection broke; a broken connection ends the session.
    ///
    /// # Cancel safety
    ///
    /// Cancel safe: a stream read that did not complete read nothing. The wait
    /// also answers requests the engine sends, and one dropped while such an
    /// answer's frame is being written leaves the session not intact
    /// ([`EngineSession::is_intact`]).
    pub async fn read_output(
        &mut self,
        until: Instant,
        settled: impl Fn(&Self) -> bool,
    ) -> Result<(), RiftError> {
        self.refuse_ended()?;
        loop {
            let now = Instant::now();
            if settled(self) || now >= until {
                return Ok(());
            }
            let wake = self
                .progress
                .next_quiet_boundary(now, self.settle_delay)
                .map_or(until, |boundary| boundary.min(until));
            let read = tokio::time::timeout_at(wake, self.next_payload(Progress::METHOD)).await;
            let unread = match read {
                Err(_elapsed) => {
                    self.progress.read(Instant::now());
                    Ok(None)
                }
                Ok(Ok(payload)) => self.route_waited(payload).await,
                Ok(Err(error)) => error.fail(),
            };
            match unread {
                Ok(None) => {}
                Ok(Some(response)) => {
                    self.queue.push_front(response);
                    tokio::time::sleep_until(until).await;
                    return Ok(());
                }
                Err(error) => {
                    if ends_session(&error) {
                        self.end().await;
                    }
                    return error.fail();
                }
            }
        }
    }

    /// Routes one message [`EngineSession::read_output`] read: an engine
    /// notification or request is handled here, and a response comes back
    /// unread, since it belongs to the next exchange.
    async fn route_waited(&mut self, payload: Vec<u8>) -> Result<Option<Vec<u8>>, RiftError> {
        let Some(incoming) = correlation::classify(&payload) else {
            return errors::lsp::engine_message_unreadable().fail();
        };
        let Some(method) = incoming.method else {
            return Ok(Some(payload));
        };
        self.route_unrequested(&method, incoming.id, incoming.params, Progress::METHOD)
            .await?;
        Ok(None)
    }

    /// Routes one message the engine sent on its own: a notification is
    /// recorded and an engine request is answered.
    ///
    /// `during` names the exchange a broken connection is reported under.
    async fn route_unrequested(
        &mut self,
        method: &str,
        id: Option<Value>,
        params: Option<Value>,
        during: &str,
    ) -> Result<(), RiftError> {
        let Some(request_id) = id else {
            self.record_notification(method, params);
            return Ok(());
        };
        self.record_server_request(method, params.clone());
        let answer = answer_server_request(method, &request_id, params);
        self.write_payload(answer, during).await
    }

    /// Pulls the engine's current diagnostics for one document.
    ///
    /// Full reports keep their result id and bounded findings. Findings are
    /// sorted before the bound so equivalent reports compare equal when an
    /// engine changes only their order.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when diagnostic pulls are not advertised or
    /// the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call
    /// discards the engine's stale response. Dropped while the request's frame
    /// is being written, it leaves the session not intact
    /// ([`EngineSession::is_intact`]).
    pub async fn pull_diagnostics(
        &mut self,
        path: &ProjectPath,
    ) -> Result<PulledDiagnostics, RiftError> {
        require(
            self.capabilities.pull_diagnostics,
            DocumentDiagnosticRequest::METHOD,
        )?;
        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier {
                uri: self.document_uri(path)?,
            },
            identifier: self.capabilities.diagnostic_identifier.clone(),
            previous_result_id: None,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let answer = self.request::<DocumentDiagnosticRequest>(params).await?;
        let report = match answer {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
                let mut items = report.full_document_diagnostic_report.items;
                items.sort_by_cached_key(|item| format!("{item:?}"));
                items.truncate(DOCUMENT_DIAGNOSTICS_MAX);
                PulledDiagnostics {
                    items,
                    result_id: report.full_document_diagnostic_report.result_id,
                    full: true,
                }
            }
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(report)) => {
                PulledDiagnostics {
                    items: Vec::new(),
                    result_id: Some(report.unchanged_document_diagnostic_report.result_id),
                    full: false,
                }
            }
            DocumentDiagnosticReportResult::Partial(_) => PulledDiagnostics {
                items: Vec::new(),
                result_id: None,
                full: false,
            },
        };
        Ok(report)
    }

    /// Diagnostics the engine published for one document, if any arrived.
    #[must_use]
    pub fn published_diagnostics(&self, path: &ProjectPath) -> Option<&[Diagnostic]> {
        self.published
            .get(path)
            .map(|report| report.diagnostics.as_slice())
    }

    /// The document version the most recent publish for `path` named, when
    /// the engine sent one.
    ///
    /// `textDocument/publishDiagnostics` carries an optional `version`. A
    /// `None` here covers two different facts the same way: no publish has
    /// arrived for `path` at all, or the engine's publish omitted the
    /// field. Either way there is no version evidence, and a caller must
    /// never read `None` as proof the document is current.
    #[must_use]
    pub fn published_diagnostics_version(&self, path: &ProjectPath) -> Option<i32> {
        self.published.get(path)?.version
    }

    /// Sends one classified `workspace/didChangeWatchedFiles` batch.
    ///
    /// Each path retains its create, change, or delete classification. The
    /// session includes it only when one registered watcher matches both
    /// path and classification, preserving caller order in one notification.
    ///
    /// Readiness resets only when the batch matched a registered watcher:
    /// an engine that registers none, such as
    /// typescript-language-server, which watches files itself, is told
    /// nothing, so its readiness stands.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended, a path cannot form
    /// a document URI, or the engine's connection broke.
    ///
    /// # Cancel safety
    ///
    /// The batch is written in one frame, the only point where the future
    /// waits. Dropping the future while that frame is being written leaves the
    /// session not intact ([`EngineSession::is_intact`]), and the engine's view
    /// of the changed paths is then unknown.
    pub async fn notify_changed_paths(
        &mut self,
        paths: &[(ProjectPath, FileChangeType)],
    ) -> Result<Vec<ProjectPath>, RiftError> {
        let matched: Vec<(ProjectPath, FileChangeType)> = paths
            .iter()
            .filter(|(path, change)| self.matches_watched_file(path, *change))
            .cloned()
            .collect();
        if matched.is_empty() {
            return Ok(Vec::new());
        }
        self.progress.invalidated(Instant::now());
        let mut changes = Vec::with_capacity(matched.len());
        for (path, change) in &matched {
            changes.push(FileEvent {
                uri: self.document_uri(path)?,
                typ: *change,
            });
        }
        self.notify::<DidChangeWatchedFiles>(&DidChangeWatchedFilesParams { changes })
            .await?;
        Ok(matched.into_iter().map(|(path, _change)| path).collect())
    }

    /// Whether one watcher matches both path and classified event.
    fn matches_watched_file(&self, path: &ProjectPath, change: FileChangeType) -> bool {
        let Some(kind) = watch_kind(change) else {
            return false;
        };
        self.watched_file_watchers.iter().any(|watcher| {
            watcher.kind.is_none_or(|watched| watched.contains(kind))
                && glob_pattern_matches(&watcher.glob_pattern, path.as_str())
        })
    }

    /// Ends the engine: shutdown and exit under their timeout, then a kill.
    ///
    /// The captured standard error comes back so a failing engine's output
    /// is not lost. The child is always reaped before this returns.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future drops the child with kill-on-drop armed: the
    /// runtime kills and reaps it in the background.
    pub async fn shutdown(mut self) -> CapturedStream {
        if !self.ended {
            let _shutdown = self.request_within::<Shutdown>((), SHUTDOWN_TIMEOUT).await;
        }
        if !self.ended {
            let _exit = self.notify::<Exit>(&()).await;
            if let Some(child) = self.child.as_mut() {
                let waited = tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await;
                if !matches!(waited, Ok(Ok(_))) {
                    // The child overstayed shutdown or cannot be observed:
                    // kill and reap it rather than leave it running.
                    let _ = child.kill().await;
                }
            }
            self.ended = true;
        }
        self.stderr_drain.await.unwrap_or_default()
    }

    /// Completes initialize and initialized under the startup timeout.
    #[expect(
        deprecated,
        reason = "root_uri is deprecated in favor of workspace_folders, but engines \
                  predating workspace folders still read it"
    )]
    async fn handshake(
        &mut self,
        workspace_root: &Path,
        startup_timeout: Duration,
        initialization_options: Option<Value>,
    ) -> Result<(), RiftError> {
        let root_uri = self.root.root_uri()?;
        let folder_name = workspace_root.file_name().map_or_else(
            || "workspace".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: Some(root_uri.clone()),
            initialization_options,
            capabilities: offered(),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: folder_name,
            }]),
            ..InitializeParams::default()
        };
        // Boxed: the initialize exchange's future measures 11,056 bytes, and this handshake
        // runs once per session start, so the allocation is paid once.
        let answer = Box::pin(self.request_within::<Initialize>(params, startup_timeout)).await?;
        self.capabilities = Capabilities::negotiated(&answer)?;
        self.notify::<Initialized>(&InitializedParams {}).await
    }

    /// Sends one request and reads until its response, under the timeout.
    async fn request<R: Request>(&mut self, params: R::Params) -> Result<R::Result, RiftError> {
        self.request_within::<R>(params, self.request_timeout).await
    }

    /// Sends one request under an explicit timeout.
    ///
    /// A timeout or a broken exchange ends the session. The previous
    /// answer's emptiness verdict is forgotten here, so the one the
    /// holder reads always belongs to the request it just made.
    async fn request_within<R: Request>(
        &mut self,
        params: R::Params,
        timeout: Duration,
    ) -> Result<R::Result, RiftError> {
        self.refuse_ended()?;
        self.empty_answers.forget();
        let id = self.correlation.begin(R::METHOD)?;
        let value = serde_json::to_value(params).map_err(|source| {
            errors::lsp::engine_result_invalid()
                .method(R::METHOD)
                .source(source)
                .error()
        })?;
        let payload = correlation::request(id, R::METHOD, &value);
        match tokio::time::timeout(timeout, self.exchange::<R>(id, payload)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                if ends_session(&error) {
                    self.end().await;
                }
                error.fail()
            }
            Err(_elapsed) => {
                self.end().await;
                errors::lsp::engine_timed_out()
                    .method(R::METHOD)
                    .timeout_ms(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
                    .fail()
            }
        }
    }

    /// Writes one request and pumps messages until its response arrives.
    ///
    /// Server-initiated requests are answered inline, notifications are
    /// recorded, and a response to a request a caller cancelled is settled
    /// and discarded. Every pump iteration consumes one engine message, so
    /// the loop is bounded by what the engine sends within the caller's
    /// timeout and by the framing bounds.
    async fn exchange<R: Request>(
        &mut self,
        id: RequestId,
        payload: Vec<u8>,
    ) -> Result<R::Result, RiftError> {
        self.write_payload(payload, R::METHOD).await?;
        loop {
            let payload = self.next_payload(R::METHOD).await?;
            let Some(incoming) = correlation::classify(&payload) else {
                return errors::lsp::engine_message_unreadable().fail();
            };
            if let Some(method) = incoming.method {
                self.route_unrequested(&method, incoming.id, incoming.params, R::METHOD)
                    .await?;
                continue;
            }
            let response_id = incoming.id.unwrap_or(Value::Null);
            let method = self.correlation.conclude(&response_id)?;
            if response_id.as_u64() != Some(id.value()) {
                // The settled response answers a cancelled call; discard it.
                continue;
            }
            if let Some(refusal) = incoming.error {
                let code = refusal.code;
                let method = method.to_owned();
                let message = refusal.message;
                if RETRYABLE_REFUSAL_CODES.contains(&code) {
                    return errors::lsp::engine_refused_retryable()
                        .method(method)
                        .code(code)
                        .message(message)
                        .fail();
                }
                return errors::lsp::engine_refused_terminal()
                    .method(method)
                    .code(code)
                    .message(message)
                    .fail();
            }
            let result = incoming.result.unwrap_or(Value::Null);
            return serde_json::from_value(result).map_err(|source| {
                errors::lsp::engine_result_invalid()
                    .method(method)
                    .source(source)
                    .error()
            });
        }
    }

    /// Sends one notification with a bounded write.
    ///
    /// The request timeout bounds the write, so a non-reading engine
    /// cannot stall the session.
    async fn notify<N: Notification>(&mut self, params: &N::Params) -> Result<(), RiftError> {
        self.refuse_ended()?;
        self.empty_answers.forget();
        let value = serde_json::to_value(params).map_err(|source| {
            errors::lsp::engine_result_invalid()
                .method(N::METHOD)
                .source(source)
                .error()
        })?;
        let payload = correlation::notification(N::METHOD, &value);
        let timeout = self.request_timeout;
        let sent = match tokio::time::timeout(timeout, self.write_payload(payload, N::METHOD)).await
        {
            Ok(written) => written,
            Err(_elapsed) => errors::lsp::engine_timed_out()
                .method(N::METHOD)
                .timeout_ms(u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX))
                .fail(),
        };
        if sent.is_err() {
            self.end().await;
        }
        sent
    }

    /// The next complete engine message, from the queue or fresh reads.
    ///
    /// Each iteration either returns a queued message or reads at least one
    /// byte; the framing bounds refuse unbounded buffering and the caller's
    /// timeout bounds the wall clock.
    async fn next_payload(&mut self, method: &str) -> Result<Vec<u8>, RiftError> {
        loop {
            if let Some(payload) = self.queue.pop_front() {
                return Ok(payload);
            }
            let mut chunk = [0_u8; STREAM_READ_BYTES];
            let read = self.stdout.read(&mut chunk).await.map_err(|_| {
                errors::lsp::engine_connection_closed()
                    .method(method)
                    .error()
            })?;
            if read == 0 {
                return errors::lsp::engine_connection_closed()
                    .method(method)
                    .fail();
            }
            self.progress.read(Instant::now());
            let messages = self.framing.feed(&chunk[..read])?;
            self.queue.extend(messages);
        }
    }

    /// Frames and writes one payload to the engine's stdin.
    ///
    /// The frame counts as in flight from before its first byte until the flush
    /// returns, so a caller that drops the write partway leaves the session not
    /// intact ([`EngineSession::is_intact`]): the engine may hold part of the frame,
    /// and its framing would read the next frame's bytes as the rest of this one. A
    /// failed write leaves the frame in flight too, and the caller ends the session.
    async fn write_payload(&mut self, payload: Vec<u8>, method: &str) -> Result<(), RiftError> {
        let closed = || {
            errors::lsp::engine_connection_closed()
                .method(method)
                .error()
        };
        let framed = Framing::frame(&payload);
        self.frame_in_flight = true;
        self.stdin.write_all(&framed).await.map_err(|_| closed())?;
        self.stdin.flush().await.map_err(|_| closed())?;
        self.frame_in_flight = false;
        Ok(())
    }

    /// Records the two notifications the session keeps state for; every
    /// other notification is consumed without record.
    fn record_notification(&mut self, method: &str, params: Option<Value>) {
        match method {
            PublishDiagnostics::METHOD => self.record_published(params),
            Progress::METHOD => self.record_progress(params),
            _ => {}
        }
    }

    /// Retains one published-diagnostics notification, bounded.
    fn record_published(&mut self, params: Option<Value>) {
        let Ok(published) =
            serde_json::from_value::<PublishDiagnosticsParams>(params.unwrap_or(Value::Null))
        else {
            return;
        };
        let Ok(path) = self.root.project_path(&published.uri) else {
            return;
        };
        retain_published(
            &mut self.published,
            path,
            published.diagnostics,
            published.version,
        );
    }

    /// Records what one `$/progress` notification says about its token.
    ///
    /// A begin or a report leaves the token outstanding; an end retires
    /// it. A report on a token no begin announced still counts as work
    /// running, because the report is itself the engine saying so.
    fn record_progress(&mut self, params: Option<Value>) {
        let Ok(progress) = serde_json::from_value::<ProgressParams>(params.unwrap_or(Value::Null))
        else {
            return;
        };
        let ProgressParamsValue::WorkDone(work) = progress.value;
        let now = Instant::now();
        match work {
            WorkDoneProgress::Begin(_) | WorkDoneProgress::Report(_) => {
                self.progress.began(progress.token, now);
            }
            WorkDoneProgress::End(_) => self.progress.ended(&progress.token, now),
        }
    }

    /// Records what one server-initiated request reveals about the
    /// engine's own subscriptions.
    ///
    /// Capability registration retains watched-file subscriptions. Work
    /// progress creation marks its token outstanding before the engine can
    /// send the matching begin notification.
    fn record_server_request(&mut self, method: &str, params: Option<Value>) {
        match method {
            RegisterCapability::METHOD => self.record_watched_file_registration(params),
            WorkDoneProgressCreate::METHOD => {
                let Ok(created) = serde_json::from_value::<WorkDoneProgressCreateParams>(
                    params.unwrap_or(Value::Null),
                ) else {
                    return;
                };
                self.progress.began(created.token, Instant::now());
            }
            WorkspaceDiagnosticRefresh::METHOD => {
                self.diagnostic_refresh_revision =
                    self.diagnostic_refresh_revision.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Retains the glob patterns one `client/registerCapability` call
    /// registered for `workspace/didChangeWatchedFiles`, bounded.
    ///
    /// A registration this session cannot parse, or one naming a
    /// different method, contributes nothing: the session always answers
    /// the registration with success regardless, so a malformed one costs
    /// the engine no capability it would otherwise have.
    fn record_watched_file_registration(&mut self, params: Option<Value>) {
        let Ok(registration) =
            serde_json::from_value::<RegistrationParams>(params.unwrap_or(Value::Null))
        else {
            return;
        };
        for entry in registration
            .registrations
            .into_iter()
            .take(WATCHED_FILE_REGISTRATIONS_MAX)
        {
            if entry.method != DidChangeWatchedFiles::METHOD {
                continue;
            }
            let Some(options) = entry.register_options else {
                continue;
            };
            let Ok(options) =
                serde_json::from_value::<DidChangeWatchedFilesRegistrationOptions>(options)
            else {
                continue;
            };
            for watcher in options.watchers {
                if self.watched_file_watchers.len() >= WATCHED_FILE_WATCHERS_MAX {
                    return;
                }
                self.watched_file_watchers.push(watcher);
            }
        }
    }

    /// The file URI for one project path, as an engine error on refusal.
    fn document_uri(&self, path: &ProjectPath) -> Result<lsp_types::Uri, RiftError> {
        self.root.document_uri(path)
    }

    /// The document-and-position parameters for one addressed position.
    fn position_params(
        &self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<TextDocumentPositionParams, RiftError> {
        Ok(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: self.document_uri(path)?,
            },
            position,
        })
    }

    /// Refuses every operation after the engine was killed.
    fn refuse_ended(&self) -> Result<(), RiftError> {
        if self.ended {
            errors::lsp::engine_ended().fail()
        } else {
            Ok(())
        }
    }

    /// Kills and reaps a spawned engine; later operations are refused.
    ///
    /// A session with no process (started over a transport) has nothing to
    /// kill; dropping its boxed streams closes its side of the connection.
    async fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        if let Some(child) = self.child.as_mut() {
            // A kill on an already-exited child only re-observes it; the
            // result carries nothing actionable beyond the error in flight.
            let _ = child.kill().await;
        }
    }
}

/// Retains one document's published diagnostics and named version under
/// the record bounds.
///
/// A publish replaces the document's earlier entry, version included; a
/// publish for a new document is dropped once [`PUBLISHED_DOCUMENTS_MAX`]
/// documents are retained, and each entry keeps at most
/// `DOCUMENT_DIAGNOSTICS_MAX` items.
fn retain_published(
    record: &mut BTreeMap<ProjectPath, PublishedReport>,
    path: ProjectPath,
    mut diagnostics: Vec<Diagnostic>,
    version: Option<i32>,
) {
    if !record.contains_key(&path) && record.len() >= PUBLISHED_DOCUMENTS_MAX {
        return;
    }
    diagnostics.truncate(DOCUMENT_DIAGNOSTICS_MAX);
    record.insert(
        path,
        PublishedReport {
            diagnostics,
            version,
        },
    );
}

/// Whether one registered glob pattern matches a slash-separated relative
/// path.
///
/// A [`GlobPattern::Relative`] pattern is matched by its own glob alone:
/// every session anchors one workspace root, so the base URI a real engine
/// names is always that same root, and folding it into the match would
/// only restate the scope this session already has.
fn glob_pattern_matches(pattern: &GlobPattern, path: &str) -> bool {
    let glob = match pattern {
        GlobPattern::String(glob) => glob,
        GlobPattern::Relative(relative) => &relative.pattern,
    };
    glob_matches(glob, false, path)
}

/// Refuses the operation when the engine was advertised without it.
fn require(served: bool, capability: &str) -> Result<(), RiftError> {
    if served {
        Ok(())
    } else {
        errors::lsp::engine_capability_absent()
            .capability(capability)
            .fail()
    }
}

/// Refuses an empty program and an absolute executable path.
///
/// "Absolute" is what configuration acceptance refuses, on every platform: a
/// `/` root, a backslash, or a drive prefix.
fn refuse_program(program: &str) -> Result<(), RiftError> {
    if program.is_empty() {
        return errors::lsp::engine_program_empty().fail();
    }
    if rift_core::is_absolute_program(program) {
        return errors::lsp::engine_program_absolute()
            .program(program)
            .fail();
    }
    Ok(())
}

/// Answers one server-initiated request without consulting the caller.
///
/// `workspace/configuration` gets `null` per item (at most
/// [`CONFIGURATION_ITEMS_MAX`]), capability registration and progress
/// creation get an empty success, and anything else gets the JSON-RPC
/// method-not-found error, which the protocol permits a client to answer.
fn answer_server_request(method: &str, id: &Value, params: Option<Value>) -> Vec<u8> {
    match method {
        WorkspaceConfiguration::METHOD => {
            let items = params
                .and_then(|params| serde_json::from_value::<ConfigurationParams>(params).ok())
                .map_or(0, |parsed| parsed.items.len())
                .min(CONFIGURATION_ITEMS_MAX);
            correlation::response(id, &Value::Array(vec![Value::Null; items]))
        }
        RegisterCapability::METHOD
        | WorkDoneProgressCreate::METHOD
        | WorkspaceDiagnosticRefresh::METHOD => correlation::response(id, &Value::Null),
        _ => correlation::error_response(id, METHOD_NOT_FOUND_CODE, "method not served"),
    }
}

/// Drains one stream under the capture policy.
///
/// The first `capture_bytes` are kept and the rest counted. Each read
/// returns at least one byte, so the loop iterates at most
/// [`STREAM_TOTAL_BYTES_MAX`] times before end-of-file, an error, or the
/// ceiling stops it.
async fn drain(mut stream: impl AsyncRead + Unpin, capture_bytes: usize) -> CapturedStream {
    let mut kept: Vec<u8> = Vec::with_capacity(capture_bytes.min(STREAM_READ_BYTES));
    let mut total_bytes: u64 = 0;
    let mut buffer = [0_u8; STREAM_READ_BYTES];
    while total_bytes < STREAM_TOTAL_BYTES_MAX {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read_bytes) => {
                total_bytes = STREAM_TOTAL_BYTES_MAX.min(total_bytes + read_bytes as u64);
                if kept.len() < capture_bytes {
                    let taken = read_bytes.min(capture_bytes - kept.len());
                    kept.extend_from_slice(&buffer[..taken]);
                }
            }
        }
    }
    CapturedStream {
        text: String::from_utf8_lossy(&kept).into_owned(),
        captured_bytes: kept.len() as u64,
        total_bytes,
        truncated: total_bytes > kept.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_stops_counting_at_the_stream_ceiling() {
        let captured = drain(tokio::io::repeat(b'x'), 16).await;
        assert_eq!(captured.total_bytes, STREAM_TOTAL_BYTES_MAX);
        assert_eq!(captured.captured_bytes, 16);
        assert!(captured.truncated);
    }

    #[tokio::test]
    async fn drain_reports_a_short_stream_without_truncation() {
        let captured = drain(&b"engine says hi"[..], 64).await;
        assert_eq!(captured.text, "engine says hi");
        assert_eq!(captured.total_bytes, captured.captured_bytes);
        assert!(!captured.truncated);
    }

    #[test]
    fn program_refusals_name_invalid_executables() {
        let empty = refuse_program("").expect_err("empty program");
        assert_eq!(empty.slug(), errors::lsp::engine_program_empty::SLUG);
        let absolute = refuse_program("/usr/bin/engine").expect_err("absolute program");
        assert_eq!(absolute.slug(), errors::lsp::engine_program_absolute::SLUG);
        assert!(
            absolute
                .context()
                .any(|(key, value)| key == "program" && value == "/usr/bin/engine")
        );
        refuse_program("engine").expect("bare names are accepted");
    }

    #[test]
    fn server_requests_are_answered_per_the_routing_policy() {
        let configuration = answer_server_request(
            WorkspaceConfiguration::METHOD,
            &serde_json::json!(1),
            Some(serde_json::json!({"items": [{}, {}]})),
        );
        let parsed: Value = serde_json::from_slice(&configuration).expect("valid JSON");
        assert_eq!(parsed["result"], serde_json::json!([null, null]));
        let registration =
            answer_server_request(RegisterCapability::METHOD, &serde_json::json!(2), None);
        let parsed: Value = serde_json::from_slice(&registration).expect("valid JSON");
        assert_eq!(parsed["result"], Value::Null);
        let unknown = answer_server_request("engine/probe", &serde_json::json!(3), None);
        let parsed: Value = serde_json::from_slice(&unknown).expect("valid JSON");
        assert_eq!(
            parsed["error"]["code"],
            serde_json::json!(METHOD_NOT_FOUND_CODE)
        );
    }

    #[test]
    fn engine_errors_keep_registered_identity_and_sources() {
        let launch = errors::lsp::engine_launch_failed()
            .source(std::io::Error::other("spawn torn down"))
            .error();
        assert_eq!(launch.slug(), errors::lsp::engine_launch_failed::SLUG);
        assert!(std::error::Error::source(&launch).is_some());

        let child = errors::lsp::framing_header_malformed().error();
        assert_eq!(child.slug(), errors::lsp::framing_header_malformed::SLUG);

        let timeout = errors::lsp::engine_timed_out()
            .method("initialize")
            .timeout_ms(300_u64)
            .error();
        assert!(ends_session(&timeout));
        assert_ne!(timeout.slug(), errors::lsp::engine_refused_retryable::SLUG);
        assert_ne!(timeout.slug(), errors::lsp::engine_refused_terminal::SLUG);
    }

    #[test]
    fn refusal_code_selects_registered_retry_behavior() {
        for (code, retryable) in [
            (SERVER_CANCELLED, true),
            (CONTENT_MODIFIED, true),
            (lsp_types::error_codes::REQUEST_CANCELLED, false),
            (METHOD_NOT_FOUND_CODE, false),
            (1, false),
        ] {
            let error = if retryable {
                errors::lsp::engine_refused_retryable()
                    .method("textDocument/diagnostic")
                    .code(code)
                    .message("engine words")
                    .error()
            } else {
                errors::lsp::engine_refused_terminal()
                    .method("textDocument/diagnostic")
                    .code(code)
                    .message("engine words")
                    .error()
            };
            assert!(
                error.slug() == errors::lsp::engine_refused_retryable::SLUG
                    || error.slug() == errors::lsp::engine_refused_terminal::SLUG
            );
            assert_eq!(
                error.slug() == errors::lsp::engine_refused_retryable::SLUG,
                retryable
            );
        }
        let ended = errors::lsp::engine_ended().error();
        assert_ne!(ended.slug(), errors::lsp::engine_refused_retryable::SLUG);
        assert_ne!(ended.slug(), errors::lsp::engine_refused_terminal::SLUG);
    }

    #[test]
    fn published_diagnostics_record_is_bounded_by_documents_and_items() {
        let mut record = BTreeMap::new();
        let path =
            |index: usize| ProjectPath::new(format!("src/file_{index}.rs")).expect("fixture path");
        let item = Diagnostic::default();
        for index in 0..PUBLISHED_DOCUMENTS_MAX {
            retain_published(&mut record, path(index), vec![item.clone()], None);
        }
        assert_eq!(record.len(), PUBLISHED_DOCUMENTS_MAX);
        retain_published(
            &mut record,
            path(PUBLISHED_DOCUMENTS_MAX),
            vec![item.clone()],
            None,
        );
        assert_eq!(
            record.len(),
            PUBLISHED_DOCUMENTS_MAX,
            "a new document is dropped at the bound"
        );
        retain_published(
            &mut record,
            path(0),
            vec![item.clone(); DOCUMENT_DIAGNOSTICS_MAX + 10],
            Some(3),
        );
        assert_eq!(
            record.get(&path(0)).map(|report| report.diagnostics.len()),
            Some(DOCUMENT_DIAGNOSTICS_MAX),
            "a retained document is replaced and truncated at the bound"
        );
        assert_eq!(
            record.get(&path(0)).and_then(|report| report.version),
            Some(3),
            "a retained document keeps the version its publish named"
        );
    }

    /// The settle delay every record test in this module reads against.
    const TEST_SETTLE_DELAY: Duration = Duration::from_millis(500);

    #[test]
    fn progress_record_retires_ended_tokens_and_stays_bounded() {
        let token = |index: usize| ProgressToken::String(format!("rift/work/{index}"));
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut record = WorkProgress::default();
        assert!(
            !record.is_announced() && !record.is_outstanding(),
            "a session that has read no progress has heard no announcement"
        );
        record.began(token(0), at(0));
        record.began(token(0), at(0));
        assert_eq!(
            record.outstanding.len(),
            1,
            "a token already outstanding is not doubled"
        );
        record.ended(&token(0), at(0));
        assert!(!record.is_outstanding(), "an ended token retires");
        assert!(
            record.is_announced(),
            "the announcement outlives the work it announced"
        );
        record.invalidated(at(0));
        record.read(at(1_000));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Unconfirmed
        );
        record.began(token(1), at(1_000));
        record.invalidated(at(1_000));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "invalidation retains outstanding work"
        );
        record.ended(&token(1), at(1_000));
        record.read(at(1_500));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Ready,
            "ending retained work proves readiness after invalidation"
        );
        for index in 0..PROGRESS_TOKENS_MAX {
            record.began(token(index), at(2_000));
        }
        record.began(token(PROGRESS_TOKENS_MAX), at(2_000));
        assert_eq!(
            record.outstanding.len(),
            PROGRESS_TOKENS_MAX,
            "a token past the bound is dropped"
        );
    }

    /// An emptied record reads analyzing until the session has read engine
    /// output at least one settle delay after the last transition.
    ///
    /// rust-analyzer ends and creates its load tokens in bursts:
    /// `rustAnalyzer/Fetching` ends at 0.711s and
    /// `rustAnalyzer/Building CrateGraph` is created one millisecond later,
    /// while the phase that resolves references ends at 16.354s. A read
    /// taken in one of those gaps must not report the load finished.
    #[test]
    fn an_emptied_record_reads_analyzing_until_a_read_passes_the_settle_delay() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let fetching = ProgressToken::String("rustAnalyzer/Fetching".to_owned());
        let crate_graph = ProgressToken::String("rustAnalyzer/Building CrateGraph".to_owned());
        let mut record = WorkProgress::default();

        record.began(fetching.clone(), at(0));
        record.ended(&fetching, at(1));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "a record the session has read nothing past states no quiet at all"
        );
        record.began(fetching.clone(), at(1));
        record.read(at(711));
        record.ended(&fetching, at(711));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "a read inside the gap between two tokens has read no quiet"
        );
        record.read(at(712));
        record.began(crate_graph.clone(), at(712));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "the next phase's token is outstanding"
        );
        record.read(at(712));
        record.ended(&crate_graph, at(712));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "a quiet the session has not read past proves nothing"
        );
        record.read(at(1_211));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing,
            "one millisecond short of the delay is not settled"
        );
        record.read(at(1_212));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Ready,
            "a read one delay past the last transition settles the record"
        );
        assert!(
            !record.is_outstanding(),
            "settlement never contradicts the outstanding half"
        );
        record.invalidated(at(1_212));
        assert_eq!(
            record.readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Unconfirmed,
            "changed workspace bytes move a settled record back"
        );
    }

    /// A recorded warm start of rust-analyzer 1.98.1 on this repository:
    /// every `$/progress` begin and end, in milliseconds from the engine's
    /// start.
    const RUST_ANALYZER_WARM_START: &[(u64, bool, &str)] = &[
        (20, true, "rustAnalyzer/Fetching"),
        (400, false, "rustAnalyzer/Fetching"),
        (420, true, "rustAnalyzer/Building CrateGraph"),
        (440, false, "rustAnalyzer/Building CrateGraph"),
        (440, true, "rustAnalyzer/Roots Scanned"),
        (450, true, "rustAnalyzer/Fetching"),
        (730, false, "rustAnalyzer/Roots Scanned"),
        (950, false, "rustAnalyzer/Fetching"),
        (950, true, "rustAnalyzer/Fetching"),
        (1320, false, "rustAnalyzer/Fetching"),
        (1320, true, "rustAnalyzer/Building CrateGraph"),
        (1340, false, "rustAnalyzer/Building CrateGraph"),
        (1340, true, "rustAnalyzer/Building compile-time-deps"),
        (1340, true, "rustAnalyzer/Loading proc-macros"),
        (1350, false, "rustAnalyzer/Loading proc-macros"),
        (1670, false, "rustAnalyzer/Building compile-time-deps"),
        (1670, true, "rustAnalyzer/Building CrateGraph"),
        (1750, false, "rustAnalyzer/Building CrateGraph"),
        (1750, true, "rustAnalyzer/Roots Scanned"),
        (1770, true, "rustAnalyzer/Loading proc-macros"),
        (1950, false, "rustAnalyzer/Loading proc-macros"),
        (1960, false, "rustAnalyzer/Roots Scanned"),
        (2070, true, "rustAnalyzer/Fetching"),
        (2460, false, "rustAnalyzer/Fetching"),
        (2460, true, "rustAnalyzer/cachePriming"),
        (7970, false, "rustAnalyzer/cachePriming"),
        (8160, true, "rust-analyzer/flycheck/0"),
        (8710, false, "rust-analyzer/flycheck/0"),
    ];

    /// Replays [`RUST_ANALYZER_WARM_START`] with a read every 10 ms and
    /// answers the first read at which the walk, and the session, read
    /// ready.
    fn replay_ready_at(timeline: &[(u64, bool, &str)]) -> (u64, u64) {
        let start = Instant::now();
        let mut record = WorkProgress::default();
        let mut events = timeline.iter().peekable();
        let (mut walk, mut session) = (None, None);
        for millis in (0..=12_000_u64).step_by(10) {
            let now = start + Duration::from_millis(millis);
            while let Some((_, begin, name)) = events.next_if(|(at, _, _)| *at <= millis) {
                let token = ProgressToken::String((*name).to_owned());
                if *begin {
                    record.began(token, now);
                } else {
                    record.ended(&token, now);
                }
            }
            record.read(now);
            if walk.is_none() && record.walk_readiness(TEST_SETTLE_DELAY) == EngineReadiness::Ready
            {
                walk = Some(millis);
            }
            if session.is_none() && record.readiness(TEST_SETTLE_DELAY) == EngineReadiness::Ready {
                session = Some(millis);
            }
        }
        (
            walk.expect("the walk reads ready inside the replay"),
            session.expect("the session reads ready inside the replay"),
        )
    }

    /// A walk reads rust-analyzer ready at `cachePriming` end plus the
    /// settle delay, while the session's own readiness waits for the
    /// `cargo check` flycheck token.
    #[test]
    fn a_walk_reads_rust_analyzer_ready_at_cache_priming_end_and_skips_flycheck() {
        let (walk, session) = replay_ready_at(RUST_ANALYZER_WARM_START);
        assert_eq!(walk, 7_970 + 500, "cachePriming ends at 7.97s");
        assert_eq!(session, 8_710 + 500, "flycheck/0 ends at 8.71s");

        let unchecked: Vec<_> = RUST_ANALYZER_WARM_START
            .iter()
            .copied()
            .filter(|(_, _, name)| !is_flycheck(&ProgressToken::String((*name).to_owned())))
            .collect();
        assert_eq!(
            replay_ready_at(&unchecked),
            (8_470, 8_470),
            "without the check both readings agree"
        );
    }

    /// An engine that announces nothing reads unconfirmed for a walk,
    /// and quiet once a read lands one settle delay past its first read. A
    /// record that has read nothing is never quiet.
    #[test]
    fn a_walk_reads_an_engine_that_announces_nothing_as_unconfirmed_and_quiet_after_the_delay() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut record = WorkProgress::default();
        assert!(!record.walk_is_quiet(TEST_SETTLE_DELAY));
        record.read(at(0));
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Unconfirmed
        );
        assert!(!record.walk_is_quiet(TEST_SETTLE_DELAY));
        record.read(at(499));
        assert!(!record.walk_is_quiet(TEST_SETTLE_DELAY));
        record.read(at(500));
        assert!(record.walk_is_quiet(TEST_SETTLE_DELAY));
        record.invalidated(at(600));
        record.read(at(700));
        assert!(
            !record.walk_is_quiet(TEST_SETTLE_DELAY),
            "a file change restarts the quiet"
        );
        record.read(at(1_100));
        assert!(record.walk_is_quiet(TEST_SETTLE_DELAY));
    }

    /// An engine that declared itself ready reads ready from its
    /// first read, for a walk and for the session, with no settle delay to
    /// wait out; announced work still holds it, and a change leaves it ready.
    #[test]
    fn a_declared_engine_reads_ready_at_start_and_after_a_change() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut record = WorkProgress {
            declared_ready: true,
            ..WorkProgress::default()
        };
        record.read(at(0));
        assert_eq!(record.readiness(TEST_SETTLE_DELAY), EngineReadiness::Ready);
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Ready
        );

        let token = ProgressToken::String("ty/check".to_owned());
        record.began(token.clone(), at(10));
        record.read(at(20));
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing
        );
        record.ended(&token, at(30));
        record.read(at(529));
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Analyzing
        );
        record.read(at(530));
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Ready
        );

        record.invalidated(at(600));
        record.read(at(601));
        assert_eq!(record.readiness(TEST_SETTLE_DELAY), EngineReadiness::Ready);
        assert_eq!(
            record.walk_readiness(TEST_SETTLE_DELAY),
            EngineReadiness::Ready,
            "the declaration survives a change, with no settle delay to wait out"
        );
    }

    #[test]
    fn empty_answers_record_latest_operation_and_forget_between_requests() {
        let mut record = EmptyAnswers::default();
        assert!(!record.is_empty(), "session has no empty answer yet");
        record.record(true);
        assert!(record.is_empty(), "empty references are recorded");
        record.record(true);
        assert!(record.is_empty(), "empty references are recorded");
        record.record(false);
        assert!(!record.is_empty(), "nonempty answer clears verdict");
        record.record(true);
        record.forget();
        assert!(!record.is_empty(), "next request forgets old verdict");
    }

    #[test]
    fn oversized_configuration_item_lists_are_answered_at_the_bound() {
        let items: Vec<Value> = vec![serde_json::json!({}); CONFIGURATION_ITEMS_MAX + 10];
        let answer = answer_server_request(
            WorkspaceConfiguration::METHOD,
            &serde_json::json!(1),
            Some(serde_json::json!({ "items": items })),
        );
        let parsed: Value = serde_json::from_slice(&answer).expect("valid JSON");
        let results = parsed["result"].as_array().expect("array result");
        assert_eq!(results.len(), CONFIGURATION_ITEMS_MAX);
    }
}
