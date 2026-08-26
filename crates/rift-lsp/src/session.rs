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
//! launch's `environment` entries laid on top.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::time::Duration;

use lsp_types::error_codes::{CONTENT_MODIFIED, SERVER_CANCELLED};
use lsp_types::notification::{
    DidCloseTextDocument, DidOpenTextDocument, Exit, Initialized, Notification, Progress,
    PublishDiagnostics,
};
use lsp_types::request::{
    DocumentDiagnosticRequest, Initialize, PrepareRenameRequest, References, RegisterCapability,
    Rename, Request, Shutdown, WillRenameFiles, WorkDoneProgressCreate, WorkspaceConfiguration,
};
use lsp_types::{
    ConfigurationParams, Diagnostic, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DocumentChangeOperation, DocumentChanges, DocumentDiagnosticParams, DocumentDiagnosticReport,
    DocumentDiagnosticReportResult, FileRename, InitializeParams, InitializedParams, Location,
    PartialResultParams, Position, PrepareRenameResponse, ProgressParams, ProgressParamsValue,
    ProgressToken, PublishDiagnosticsParams, ReferenceContext, ReferenceParams, RenameFilesParams,
    RenameParams, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    WorkDoneProgress, WorkDoneProgressParams, WorkspaceEdit, WorkspaceFolder,
};
use rift_core::{
    CapturedStream, Error, ErrorCode, ErrorContext, ErrorName, Fault, ProjectPath,
    STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX, fault_label,
};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::capabilities::{Capabilities, CapabilitiesError, offered};
use crate::correlation::{self, Correlation, CorrelationError, METHOD_NOT_FOUND_CODE, RequestId};
use crate::framing::{Framing, FramingError};
use crate::uri::{TreeRoot, UriError};

/// Wall-clock bound on the shutdown request and on the exit wait.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum documents with retained published diagnostics; later documents
/// are dropped.
const PUBLISHED_DOCUMENTS_MAX: usize = 64;

/// Maximum diagnostics retained per document; later entries are dropped.
const DOCUMENT_DIAGNOSTICS_MAX: usize = 256;

/// Maximum `workspace/configuration` items answered with `null` each.
const CONFIGURATION_ITEMS_MAX: usize = 256;

/// Maximum work-done progress tokens retained as outstanding at once.
///
/// The record only has to answer whether any work runs, so a token past
/// the bound is dropped: the record is already non-empty, and the session
/// already reads as analyzing.
const PROGRESS_TOKENS_MAX: usize = 64;

/// Resends one operation whose empty answer says nothing gets per
/// session.
///
/// The resend closes the one window the engine cannot report: the first
/// request of a session, answered before the engine has announced any
/// work. One resend closes it; a second would wait on an engine that may
/// simply have nothing to say.
const EMPTY_ANSWER_RESENDS_MAX: u32 = 1;

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
/// The program is resolved like a hook's: never empty, never an absolute
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
    /// Bytes of standard error kept as the captured prefix.
    pub stderr_capture_bytes: usize,
}

/// How one engine session failed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineFault {
    /// The configured program is empty.
    ProgramEmpty,
    /// The configured program is an absolute executable path.
    ProgramAbsolute {
        /// The program as configured.
        program: String,
    },
    /// The program could not be started.
    LaunchFailed {
        /// The launch failure.
        #[serde(skip)]
        source: std::io::Error,
    },
    /// The engine ended its side of the connection mid-request.
    ConnectionClosed {
        /// The request in flight.
        method: String,
    },
    /// The engine overstayed its timeout and was killed.
    TimedOut {
        /// The request in flight.
        method: String,
        /// The bound that was overstayed, in milliseconds.
        timeout_ms: u64,
    },
    /// The engine sent a payload that is not a JSON-RPC envelope.
    MessageUnreadable,
    /// The engine broke the base protocol's framing.
    Framing {
        /// The framing refusal.
        #[serde(skip)]
        source: FramingError,
    },
    /// The engine broke request correlation.
    Correlation {
        /// The correlation refusal.
        #[serde(skip)]
        source: CorrelationError,
    },
    /// The engine's initialize answer was refused.
    Negotiation {
        /// The capability refusal.
        #[serde(skip)]
        source: CapabilitiesError,
    },
    /// A document address could not be converted.
    Document {
        /// The URI refusal.
        #[serde(skip)]
        source: UriError,
    },
    /// The engine answered the request with a JSON-RPC error.
    ///
    /// The code decides whether the same request is worth sending again;
    /// [`EngineFault::is_retryable_refusal`] reads that verdict.
    Refused {
        /// The refused request.
        method: String,
        /// The engine's error code.
        code: i64,
        /// The engine's error message.
        message: String,
    },
    /// The engine's answer does not deserialize as the method's result.
    ResultInvalid {
        /// The answered request.
        method: String,
        /// The deserialization failure.
        #[serde(skip)]
        source: serde_json::Error,
    },
    /// The engine never advertised the operation.
    CapabilityAbsent {
        /// The unserved method.
        capability: String,
    },
    /// The engine answered every attempt while it was still analyzing.
    ///
    /// The answers were provisional: the engine had work-done progress
    /// outstanding each time, so the same request may answer differently
    /// once that work ends. The holder spent its whole attempt bound
    /// waiting, and reports the wait rather than the provisional answer.
    Analyzing {
        /// Attempts the operation was given.
        attempts: u64,
    },
    /// The session already killed its engine; nothing further is served.
    Ended,
}

impl EngineFault {
    /// Whether the fault leaves the engine unusable, so it must be ended.
    fn ends_session(&self) -> bool {
        matches!(
            self,
            Self::ConnectionClosed { .. }
                | Self::TimedOut { .. }
                | Self::MessageUnreadable
                | Self::Framing { .. }
                | Self::Correlation { .. }
        )
    }

    /// Whether the fault is the engine's own reply to the request.
    ///
    /// Only [`EngineFault::Refused`] is: the engine read the request and
    /// answered it with an error code. Every other fault names something
    /// around the request - an absent capability, a broken exchange, a
    /// killed child - and no amount of further analysis changes it.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }

    /// Whether the engine refused with a code that invites the same
    /// request again.
    ///
    /// Only [`EngineFault::Refused`] can answer yes, and only for the
    /// codes in `RETRYABLE_REFUSAL_CODES`: the engine cancelled the
    /// request, or the document moved under it. Every other refusal is
    /// the engine's verdict on the request, and resending it changes
    /// nothing.
    #[must_use]
    pub fn is_retryable_refusal(&self) -> bool {
        matches!(self, Self::Refused { code, .. } if RETRYABLE_REFUSAL_CODES.contains(code))
    }
}

impl Fault for EngineFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::ProgramEmpty | Self::ProgramAbsolute { .. } => {
                ErrorName::Wire(ErrorCode::ConfigurationInvalid)
            }
            Self::ConnectionClosed { .. }
            | Self::TimedOut { .. }
            | Self::Analyzing { .. }
            | Self::Ended => ErrorName::Wire(ErrorCode::TemporarilyUnavailable),
            Self::Refused { .. } if self.is_retryable_refusal() => {
                ErrorName::Wire(ErrorCode::TemporarilyUnavailable)
            }
            Self::Refused { .. } => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::Framing { source } => source.name(),
            Self::Correlation { source } => source.name(),
            Self::Negotiation { source } => source.name(),
            Self::Document { source } => source.name(),
            _ => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Framing { source } => source.context(),
            Self::Correlation { source } => source.context(),
            Self::Negotiation { source } => source.context(),
            Self::Document { source } => source.context(),
            _ => {
                let mut context = vec![ErrorContext::new("fault", fault_label(self))];
                match self {
                    Self::ProgramAbsolute { program } => {
                        context.push(ErrorContext::new("program", program.clone()));
                    }
                    Self::ConnectionClosed { method } | Self::ResultInvalid { method, .. } => {
                        context.push(ErrorContext::new("method", method.clone()));
                    }
                    Self::TimedOut { method, timeout_ms } => {
                        context.push(ErrorContext::new("method", method.clone()));
                        context.push(ErrorContext::new("timeout_ms", timeout_ms.to_string()));
                    }
                    Self::Refused {
                        method,
                        code,
                        message,
                    } => {
                        context.push(ErrorContext::new("method", method.clone()));
                        context.push(ErrorContext::new("code", code.to_string()));
                        context.push(ErrorContext::new("message", message.clone()));
                    }
                    Self::CapabilityAbsent { capability } => {
                        context.push(ErrorContext::new("capability", capability.clone()));
                    }
                    Self::Analyzing { attempts } => {
                        context.push(ErrorContext::new("attempts", attempts.to_string()));
                    }
                    _ => {}
                }
                context
            }
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LaunchFailed { source } => Some(source),
            Self::ResultInvalid { source, .. } => Some(source),
            Self::Framing { source } => Some(source),
            Self::Correlation { source } => Some(source),
            Self::Negotiation { source } => Some(source),
            Self::Document { source } => Some(source),
            _ => None,
        }
    }
}

/// A failed engine session or operation.
pub type EngineError = Error<EngineFault>;

/// What the engine has said about its own work over `$/progress`.
///
/// Two facts ride here, and the second outlives the first: which tokens
/// are outstanding right now, and whether the engine has ever announced
/// any work at all. An engine that began one token and ended it has
/// announced work with nothing outstanding.
#[derive(Debug, Default)]
struct WorkProgress {
    announced: bool,
    outstanding: Vec<ProgressToken>,
}

impl WorkProgress {
    /// Records one token as outstanding, bounded.
    ///
    /// A token already outstanding stays as it is, and a new token past
    /// [`PROGRESS_TOKENS_MAX`] is dropped: the record is already
    /// non-empty, so the session reads as analyzing either way, and an
    /// end for the dropped token retires nothing. The announcement holds
    /// whatever the bound does with the token.
    fn began(&mut self, token: ProgressToken) {
        self.announced = true;
        if self.outstanding.len() >= PROGRESS_TOKENS_MAX || self.outstanding.contains(&token) {
            return;
        }
        self.outstanding.push(token);
    }

    /// Retires one token the engine ended.
    fn ended(&mut self, token: &ProgressToken) {
        self.outstanding.retain(|held| held != token);
    }

    /// Whether any announced work is still outstanding.
    fn is_outstanding(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// Whether the engine has ever announced work.
    fn is_announced(&self) -> bool {
        self.announced
    }
}

/// One request whose empty answer says nothing a real answer would not.
///
/// An engine that has not indexed the tree yet answers
/// `workspace/willRenameFiles` with no edit and `textDocument/diagnostic`
/// with no item - the same answers a settled engine gives for a move that
/// needs no import rewritten and for a file that is clean.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum EmptyAnswer {
    /// `workspace/willRenameFiles` proposed no edit.
    WillRenameFiles,
    /// `textDocument/diagnostic` reported no item.
    Diagnostics,
}

/// What the session's most recent answer said, and the resends already
/// spent on each operation.
///
/// Every request clears `latest` before it is sent, so a verdict cannot
/// outlive the answer that set it, and only the two operations above ever
/// set one. `resends` holds one counter per variant, so it is bounded by
/// the enum, and each operation is sent again at most
/// [`EMPTY_ANSWER_RESENDS_MAX`] times per session.
#[derive(Debug, Default)]
struct EmptyAnswers {
    latest: Option<EmptyAnswer>,
    resends: BTreeMap<EmptyAnswer, u32>,
}

impl EmptyAnswers {
    /// Forgets the previous answer's verdict; every request starts here.
    fn forget(&mut self) {
        self.latest = None;
    }

    /// Records whether one answer to `operation` said nothing.
    fn record(&mut self, operation: EmptyAnswer, empty: bool) {
        self.latest = empty.then_some(operation);
    }

    /// Claims one resend for the answer just given.
    ///
    /// Answers `false` when that answer said something, or when this
    /// operation's resend is already spent.
    fn claim(&mut self) -> bool {
        let Some(operation) = self.latest else {
            return false;
        };
        let spent = self.resends.entry(operation).or_default();
        if *spent >= EMPTY_ANSWER_RESENDS_MAX {
            return false;
        }
        *spent += 1;
        true
    }
}

/// The engine's writable half: a real child's stdin, or a test transport's
/// own write half.
type EngineWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// The engine's readable half: a real child's stdout, or a test transport's
/// own read half.
type EngineReader = Box<dyn AsyncRead + Send + Unpin>;

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
    published: BTreeMap<ProjectPath, Vec<Diagnostic>>,
    progress: WorkProgress,
    empty_answers: EmptyAnswers,
    document_version: i32,
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
    /// Returns [`EngineError`] for a refused program, a failed spawn, or a
    /// failed handshake; the child never outlives the failure.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future mid-handshake drops the child with kill-on-drop
    /// armed: the runtime kills and reaps it in the background.
    pub async fn start(launch: EngineLaunch, workspace_root: &Path) -> Result<Self, EngineError> {
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
            .map_err(|source| Error::new(EngineFault::LaunchFailed { source }))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill().await;
            return Err(Error::new(EngineFault::LaunchFailed {
                source: std::io::Error::other("child pipes were not handed over"),
            }));
        };
        let stderr_drain = tokio::spawn(drain(stderr, launch.stderr_capture_bytes));
        Self::assembled(
            Some(child),
            Box::new(stdin),
            Box::new(stdout),
            stderr_drain,
            workspace_root,
            launch.startup_timeout,
            launch.request_timeout,
            launch.initialization_options,
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
    /// Returns [`EngineError`] under the same conditions as
    /// [`EngineSession::start`], minus a failed spawn.
    pub async fn start_over_transport(
        launch: EngineLaunch,
        workspace_root: &Path,
        transport: impl AsyncRead + AsyncWrite + Send + 'static,
        stderr: impl AsyncRead + Send + Unpin + 'static,
    ) -> Result<Self, EngineError> {
        let (read_half, write_half) = tokio::io::split(transport);
        let stderr_drain = tokio::spawn(drain(stderr, launch.stderr_capture_bytes));
        Self::assembled(
            None,
            Box::new(write_half),
            Box::new(read_half),
            stderr_drain,
            workspace_root,
            launch.startup_timeout,
            launch.request_timeout,
            launch.initialization_options,
        )
        .await
    }

    /// Builds the session over an acquired transport and runs the
    /// handshake, ending the session and propagating the failure if it
    /// fails. Shared by [`EngineSession::start`] and
    /// [`EngineSession::start_over_transport`], the only two constructors.
    #[expect(
        clippy::too_many_arguments,
        reason = "one field per assembled struct member"
    )]
    async fn assembled(
        child: Option<Child>,
        stdin: EngineWriter,
        stdout: EngineReader,
        stderr_drain: tokio::task::JoinHandle<CapturedStream>,
        workspace_root: &Path,
        startup_timeout: Duration,
        request_timeout: Duration,
        initialization_options: Option<Value>,
    ) -> Result<Self, EngineError> {
        let root = TreeRoot::new(workspace_root)
            .map_err(|source| Error::new(EngineFault::Document { source }))?;
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
            request_timeout,
            published: BTreeMap::new(),
            progress: WorkProgress::default(),
            empty_answers: EmptyAnswers::default(),
            document_version: 0,
            ended: false,
        };
        if let Err(error) = session
            .handshake(workspace_root, startup_timeout, initialization_options)
            .await
        {
            session.end().await;
            return Err(error);
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

    /// The version the most recent `didOpen` carried. An engine echoes it
    /// on a versioned edit, and a compiler of that edit compares against
    /// this value.
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
    /// never reads as analyzing, and its answers are final at once.
    #[must_use]
    pub fn is_analyzing(&self) -> bool {
        self.progress.is_outstanding()
    }

    /// Whether the engine has never announced any work of its own.
    ///
    /// [`EngineSession::is_analyzing`] answers what the engine is doing
    /// now; this answers whether it has ever said what it is doing. Until
    /// one `$/progress` begin has arrived, nothing distinguishes an engine
    /// that has finished its work from one that has not started
    /// announcing it, so an empty answer given in that window is not
    /// evidence that there was nothing to answer.
    #[must_use]
    pub fn has_never_announced_work(&self) -> bool {
        !self.progress.is_announced()
    }

    /// Claims one resend for an answer that said nothing where something
    /// was expected.
    ///
    /// Answers `true` only when the engine has never announced work, the
    /// request just served answered nothing
    /// (`workspace/willRenameFiles` with no edit,
    /// `textDocument/diagnostic` with no item), and this session has not
    /// already sent that operation again. Each operation claims at most
    /// `EMPTY_ANSWER_RESENDS_MAX` resends per session, so the holder
    /// cannot loop on an engine that has nothing to say. Once the engine
    /// has announced work even once, its empty answers are its own and
    /// this claim is refused.
    #[must_use]
    pub fn claim_empty_answer_resend(&mut self) -> bool {
        if self.progress.is_announced() {
            return false;
        }
        self.empty_answers.claim()
    }

    /// Opens one document with the text the caller hands in.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the session ended or the engine's side
    /// of the connection broke.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future may leave the notification unsent; the engine's
    /// document state is then unknown and the next operation still runs.
    pub async fn open(
        &mut self,
        path: &ProjectPath,
        language_id: &str,
        text: String,
    ) -> Result<(), EngineError> {
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
        self.notify::<DidOpenTextDocument>(&params).await
    }

    /// Closes one previously opened document.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the session ended or the engine's side
    /// of the connection broke.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future may leave the notification unsent; the engine's
    /// document state is then unknown and the next operation still runs.
    pub async fn close(&mut self, path: &ProjectPath) -> Result<(), EngineError> {
        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: self.document_uri(path)?,
            },
        };
        self.notify::<DidCloseTextDocument>(&params).await
    }

    /// The engine's workspace edit renaming the symbol at one position.
    ///
    /// An engine answering `null` proposes no edit; the empty edit comes
    /// back so every answer has the same shape.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when rename is not advertised, the engine
    /// refuses the name, or the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call
    /// discards the engine's stale response.
    pub async fn rename(
        &mut self,
        path: &ProjectPath,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, EngineError> {
        require(self.capabilities.rename, Rename::METHOD)?;
        let params = RenameParams {
            text_document_position: self.position_params(path, position)?,
            new_name: new_name.to_owned(),
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        let edit = self.request::<Rename>(params).await?;
        Ok(edit.unwrap_or_default())
    }

    /// The engine's verdict on renaming at one position.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when prepared renames are not advertised or
    /// the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call
    /// discards the engine's stale response.
    pub async fn prepare_rename(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Option<PrepareRenameResponse>, EngineError> {
        require(
            self.capabilities.prepare_rename,
            PrepareRenameRequest::METHOD,
        )?;
        let params = self.position_params(path, position)?;
        self.request::<PrepareRenameRequest>(params).await
    }

    /// The engine's workspace edit for moving one file, if it proposes one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when will-rename requests are not advertised
    /// or the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call
    /// discards the engine's stale response.
    pub async fn will_rename_files(
        &mut self,
        from: &ProjectPath,
        to: &ProjectPath,
    ) -> Result<Option<WorkspaceEdit>, EngineError> {
        require(
            self.capabilities.will_rename_files(),
            WillRenameFiles::METHOD,
        )?;
        let params = RenameFilesParams {
            files: vec![FileRename {
                old_uri: self.document_uri(from)?.as_str().to_owned(),
                new_uri: self.document_uri(to)?.as_str().to_owned(),
            }],
        };
        let edit = self.request::<WillRenameFiles>(params).await?;
        self.empty_answers.record(
            EmptyAnswer::WillRenameFiles,
            proposes_no_edit(edit.as_ref()),
        );
        Ok(edit)
    }

    /// The locations referencing the declaration at one position.
    ///
    /// `context.include_declaration` is `false`: the declaration itself is what a caller is
    /// about to remove, so counting it would refuse every removal. An engine answering
    /// `null` reports no references.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when references are not advertised or the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call discards the engine's
    /// stale response.
    pub async fn references(
        &mut self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<Vec<Location>, EngineError> {
        require(self.capabilities.references, References::METHOD)?;
        let params = ReferenceParams {
            text_document_position: self.position_params(path, position)?,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration: false,
            },
        };
        let locations = self.request::<References>(params).await?;
        Ok(locations.unwrap_or_default())
    }

    /// Pulls the engine's current diagnostics for one document.
    ///
    /// An unchanged or partial report answers no items; the session never
    /// sends a previous result id, so a full report is the served shape.
    /// At most `DOCUMENT_DIAGNOSTICS_MAX` items come back.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when diagnostic pulls are not advertised or
    /// the exchange breaks.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the request pending; a later call
    /// discards the engine's stale response.
    pub async fn pull_diagnostics(
        &mut self,
        path: &ProjectPath,
    ) -> Result<Vec<Diagnostic>, EngineError> {
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
        let mut items = match answer {
            DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
                report.full_document_diagnostic_report.items
            }
            _ => Vec::new(),
        };
        items.truncate(DOCUMENT_DIAGNOSTICS_MAX);
        self.empty_answers
            .record(EmptyAnswer::Diagnostics, items.is_empty());
        Ok(items)
    }

    /// Diagnostics the engine published for one document, if any arrived.
    #[must_use]
    pub fn published_diagnostics(&self, path: &ProjectPath) -> Option<&[Diagnostic]> {
        self.published.get(path).map(Vec::as_slice)
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
    ) -> Result<(), EngineError> {
        let root_uri = self
            .root
            .root_uri()
            .map_err(|source| Error::new(EngineFault::Document { source }))?;
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
        let answer = self
            .request_within::<Initialize>(params, startup_timeout)
            .await?;
        self.capabilities = Capabilities::negotiated(&answer)
            .map_err(|source| Error::new(EngineFault::Negotiation { source }))?;
        self.notify::<Initialized>(&InitializedParams {}).await
    }

    /// Sends one request and reads until its response, under the timeout.
    async fn request<R: Request>(&mut self, params: R::Params) -> Result<R::Result, EngineError> {
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
    ) -> Result<R::Result, EngineError> {
        self.refuse_ended()?;
        self.empty_answers.forget();
        let id = self
            .correlation
            .begin(R::METHOD)
            .map_err(|source| Error::new(EngineFault::Correlation { source }))?;
        let value = serde_json::to_value(params).map_err(|source| {
            Error::new(EngineFault::ResultInvalid {
                method: R::METHOD.to_owned(),
                source,
            })
        })?;
        let payload = correlation::request(id, R::METHOD, &value);
        match tokio::time::timeout(timeout, self.exchange::<R>(id, payload)).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => {
                if error.fault().ends_session() {
                    self.end().await;
                }
                Err(error)
            }
            Err(_elapsed) => {
                self.end().await;
                Err(Error::new(EngineFault::TimedOut {
                    method: R::METHOD.to_owned(),
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                }))
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
    ) -> Result<R::Result, EngineError> {
        self.write_payload(payload, R::METHOD).await?;
        loop {
            let payload = self.next_payload(R::METHOD).await?;
            let Some(incoming) = correlation::classify(&payload) else {
                return Err(Error::new(EngineFault::MessageUnreadable));
            };
            if let Some(method) = incoming.method {
                match incoming.id {
                    Some(request_id) => {
                        let answer = answer_server_request(&method, &request_id, incoming.params);
                        self.write_payload(answer, R::METHOD).await?;
                    }
                    None => self.record_notification(&method, incoming.params),
                }
                continue;
            }
            let response_id = incoming.id.unwrap_or(Value::Null);
            let method = self
                .correlation
                .conclude(&response_id)
                .map_err(|source| Error::new(EngineFault::Correlation { source }))?;
            if response_id.as_u64() != Some(id.value()) {
                // The settled response answers a cancelled call; discard it.
                continue;
            }
            if let Some(refusal) = incoming.error {
                return Err(Error::new(EngineFault::Refused {
                    method: method.to_owned(),
                    code: refusal.code,
                    message: refusal.message,
                }));
            }
            let result = incoming.result.unwrap_or(Value::Null);
            return serde_json::from_value(result).map_err(|source| {
                Error::new(EngineFault::ResultInvalid {
                    method: method.to_owned(),
                    source,
                })
            });
        }
    }

    /// Sends one notification with a bounded write.
    ///
    /// The request timeout bounds the write, so a non-reading engine
    /// cannot stall the session.
    async fn notify<N: Notification>(&mut self, params: &N::Params) -> Result<(), EngineError> {
        self.refuse_ended()?;
        let value = serde_json::to_value(params).map_err(|source| {
            Error::new(EngineFault::ResultInvalid {
                method: N::METHOD.to_owned(),
                source,
            })
        })?;
        let payload = correlation::notification(N::METHOD, &value);
        let timeout = self.request_timeout;
        let sent = match tokio::time::timeout(timeout, self.write_payload(payload, N::METHOD)).await
        {
            Ok(written) => written,
            Err(_elapsed) => Err(Error::new(EngineFault::TimedOut {
                method: N::METHOD.to_owned(),
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            })),
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
    async fn next_payload(&mut self, method: &str) -> Result<Vec<u8>, EngineError> {
        loop {
            if let Some(payload) = self.queue.pop_front() {
                return Ok(payload);
            }
            let mut chunk = [0_u8; STREAM_READ_BYTES];
            let read = self.stdout.read(&mut chunk).await.map_err(|_| {
                Error::new(EngineFault::ConnectionClosed {
                    method: method.to_owned(),
                })
            })?;
            if read == 0 {
                return Err(Error::new(EngineFault::ConnectionClosed {
                    method: method.to_owned(),
                }));
            }
            let messages = self
                .framing
                .feed(&chunk[..read])
                .map_err(|source| Error::new(EngineFault::Framing { source }))?;
            self.queue.extend(messages);
        }
    }

    /// Frames and writes one payload to the engine's stdin.
    async fn write_payload(&mut self, payload: Vec<u8>, method: &str) -> Result<(), EngineError> {
        let closed = || {
            Error::new(EngineFault::ConnectionClosed {
                method: method.to_owned(),
            })
        };
        let framed = Framing::frame(&payload);
        self.stdin.write_all(&framed).await.map_err(|_| closed())?;
        self.stdin.flush().await.map_err(|_| closed())
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
        retain_published(&mut self.published, path, published.diagnostics);
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
        match work {
            WorkDoneProgress::Begin(_) | WorkDoneProgress::Report(_) => {
                self.progress.began(progress.token);
            }
            WorkDoneProgress::End(_) => self.progress.ended(&progress.token),
        }
    }

    /// The file URI for one project path, as an engine fault on refusal.
    fn document_uri(&self, path: &ProjectPath) -> Result<lsp_types::Uri, EngineError> {
        self.root
            .document_uri(path)
            .map_err(|source| Error::new(EngineFault::Document { source }))
    }

    /// The document-and-position parameters for one addressed position.
    fn position_params(
        &self,
        path: &ProjectPath,
        position: Position,
    ) -> Result<TextDocumentPositionParams, EngineError> {
        Ok(TextDocumentPositionParams {
            text_document: TextDocumentIdentifier {
                uri: self.document_uri(path)?,
            },
            position,
        })
    }

    /// Refuses every operation after the engine was killed.
    fn refuse_ended(&self) -> Result<(), EngineError> {
        if self.ended {
            Err(Error::new(EngineFault::Ended))
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
            // result carries nothing actionable beyond the fault in flight.
            let _ = child.kill().await;
        }
    }
}

/// Retains one document's published diagnostics under the record bounds.
///
/// A publish replaces the document's earlier entry; a publish for a new
/// document is dropped once [`PUBLISHED_DOCUMENTS_MAX`] documents are
/// retained, and each entry keeps at most `DOCUMENT_DIAGNOSTICS_MAX`
/// items.
fn retain_published(
    record: &mut BTreeMap<ProjectPath, Vec<Diagnostic>>,
    path: ProjectPath,
    mut diagnostics: Vec<Diagnostic>,
) {
    if !record.contains_key(&path) && record.len() >= PUBLISHED_DOCUMENTS_MAX {
        return;
    }
    diagnostics.truncate(DOCUMENT_DIAGNOSTICS_MAX);
    record.insert(path, diagnostics);
}

/// Whether one will-rename answer proposes no edit at all.
///
/// An engine that has not indexed the moved file's references answers
/// `null`, and one that answers an edit set holding no edit says exactly
/// the same thing. Versioned document changes take precedence over the
/// plain `changes` map, as the compile of the proposal does, and a
/// create, rename, or delete operation counts as an edit even though it
/// carries no text.
#[must_use]
pub fn proposes_no_edit(edit: Option<&WorkspaceEdit>) -> bool {
    let Some(edit) = edit else {
        return true;
    };
    match (&edit.document_changes, &edit.changes) {
        (Some(DocumentChanges::Edits(documents)), _) => {
            documents.iter().all(|document| document.edits.is_empty())
        }
        (Some(DocumentChanges::Operations(operations)), _) => {
            operations.iter().all(operation_edits_nothing)
        }
        (None, Some(changes)) => changes.values().all(Vec::is_empty),
        (None, None) => true,
    }
}

/// Whether one mixed document-change operation changes nothing.
fn operation_edits_nothing(operation: &DocumentChangeOperation) -> bool {
    match operation {
        DocumentChangeOperation::Op(_) => false,
        DocumentChangeOperation::Edit(document) => document.edits.is_empty(),
    }
}

/// Refuses the operation when the engine was advertised without it.
fn require(served: bool, capability: &str) -> Result<(), EngineError> {
    if served {
        Ok(())
    } else {
        Err(Error::new(EngineFault::CapabilityAbsent {
            capability: capability.to_owned(),
        }))
    }
}

/// Refuses an empty program and an absolute executable path, as hooks do.
fn refuse_program(program: &str) -> Result<(), EngineError> {
    if program.is_empty() {
        return Err(Error::new(EngineFault::ProgramEmpty));
    }
    if Path::new(program).is_absolute() {
        return Err(Error::new(EngineFault::ProgramAbsolute {
            program: program.to_owned(),
        }));
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
        RegisterCapability::METHOD | WorkDoneProgressCreate::METHOD => {
            correlation::response(id, &Value::Null)
        }
        _ => correlation::error_response(id, METHOD_NOT_FOUND_CODE, "method not served"),
    }
}

/// Drains one stream under the capture policy hook drains follow.
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
    fn program_refusals_match_the_hook_rules() {
        let empty = refuse_program("").expect_err("empty program");
        assert!(matches!(empty.fault(), EngineFault::ProgramEmpty));
        assert_eq!(
            empty.name(),
            ErrorName::Wire(ErrorCode::ConfigurationInvalid)
        );
        let absolute = refuse_program("/usr/bin/engine").expect_err("absolute program");
        assert!(matches!(
            absolute.fault(),
            EngineFault::ProgramAbsolute { program } if program == "/usr/bin/engine"
        ));
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

    use crate::capabilities::CapabilitiesFault;
    use crate::correlation::CorrelationFault;
    use crate::framing::FramingFault;
    use crate::uri::UriFault;

    /// One row per fault variant: the fault, its classification, whether it
    /// ends the session, and evidence its render must name.
    #[expect(clippy::too_many_lines, reason = "one data row per fault variant")]
    fn engine_fault_rows() -> Vec<(EngineFault, ErrorCode, bool, &'static str)> {
        vec![
            (
                EngineFault::ProgramEmpty,
                ErrorCode::ConfigurationInvalid,
                false,
                "program_empty",
            ),
            (
                EngineFault::ProgramAbsolute {
                    program: "/usr/bin/engine".to_owned(),
                },
                ErrorCode::ConfigurationInvalid,
                false,
                "/usr/bin/engine",
            ),
            (
                EngineFault::LaunchFailed {
                    source: std::io::Error::other("spawn torn down"),
                },
                ErrorCode::CapabilityUnavailable,
                false,
                "launch_failed",
            ),
            (
                EngineFault::ConnectionClosed {
                    method: "textDocument/rename".to_owned(),
                },
                ErrorCode::TemporarilyUnavailable,
                true,
                "textDocument/rename",
            ),
            (
                EngineFault::TimedOut {
                    method: "initialize".to_owned(),
                    timeout_ms: 300,
                },
                ErrorCode::TemporarilyUnavailable,
                true,
                "timeout_ms 300",
            ),
            (
                EngineFault::MessageUnreadable,
                ErrorCode::CapabilityUnavailable,
                true,
                "fault",
            ),
            (
                EngineFault::Framing {
                    source: Error::new(FramingFault::HeaderMalformed),
                },
                ErrorCode::CapabilityUnavailable,
                true,
                "header_malformed",
            ),
            (
                EngineFault::Correlation {
                    source: Error::new(CorrelationFault::ResponseUnknown { id: "7".to_owned() }),
                },
                ErrorCode::CapabilityUnavailable,
                true,
                "response_unknown",
            ),
            (
                EngineFault::Negotiation {
                    source: Error::new(CapabilitiesFault::PositionEncodingUnsupported {
                        encoding: "utf-32".to_owned(),
                    }),
                },
                ErrorCode::CapabilityUnavailable,
                false,
                "utf-32",
            ),
            (
                EngineFault::Document {
                    source: Error::new(UriFault::OutsideRoot),
                },
                ErrorCode::PermissionDenied,
                false,
                "outside_root",
            ),
            (
                EngineFault::Refused {
                    method: "textDocument/rename".to_owned(),
                    code: -32602,
                    message: "not an identifier".to_owned(),
                },
                ErrorCode::InvalidRequest,
                false,
                "not an identifier",
            ),
            (
                EngineFault::Refused {
                    method: "textDocument/diagnostic".to_owned(),
                    code: SERVER_CANCELLED,
                    message: "server cancelled the request".to_owned(),
                },
                ErrorCode::TemporarilyUnavailable,
                false,
                "server cancelled the request",
            ),
            (
                EngineFault::ResultInvalid {
                    method: "shutdown".to_owned(),
                    source: serde_json::from_value::<u64>(Value::Null).expect_err("typed"),
                },
                ErrorCode::CapabilityUnavailable,
                false,
                "shutdown",
            ),
            (
                EngineFault::CapabilityAbsent {
                    capability: "textDocument/diagnostic".to_owned(),
                },
                ErrorCode::CapabilityUnavailable,
                false,
                "textDocument/diagnostic",
            ),
            (
                EngineFault::Analyzing { attempts: 8 },
                ErrorCode::TemporarilyUnavailable,
                false,
                "attempts 8",
            ),
            (
                EngineFault::Ended,
                ErrorCode::TemporarilyUnavailable,
                false,
                "ended",
            ),
        ]
    }

    #[test]
    fn engine_faults_classify_and_render_their_evidence() {
        for (fault, code, ends, evidence) in engine_fault_rows() {
            assert_eq!(fault.ends_session(), ends, "{fault:?}");
            let error = Error::new(fault);
            assert_eq!(error.name(), ErrorName::Wire(code), "{error:?}");
            let rendered = error.to_string();
            assert!(rendered.contains(evidence), "{rendered}");
        }
    }

    #[test]
    fn engine_faults_expose_their_sources() {
        let sourced = [
            Error::new(EngineFault::LaunchFailed {
                source: std::io::Error::other("spawn torn down"),
            }),
            Error::new(EngineFault::Framing {
                source: Error::new(FramingFault::HeaderMalformed),
            }),
            Error::new(EngineFault::Correlation {
                source: Error::new(CorrelationFault::PendingRequestsExceeded),
            }),
            Error::new(EngineFault::Negotiation {
                source: Error::new(CapabilitiesFault::PositionEncodingUnsupported {
                    encoding: "utf-32".to_owned(),
                }),
            }),
            Error::new(EngineFault::Document {
                source: Error::new(UriFault::OutsideRoot),
            }),
            Error::new(EngineFault::ResultInvalid {
                method: "shutdown".to_owned(),
                source: serde_json::from_value::<u64>(Value::Null).expect_err("typed"),
            }),
        ];
        for error in &sourced {
            assert!(std::error::Error::source(error).is_some(), "{error:?}");
        }
        assert!(std::error::Error::source(&Error::new(EngineFault::Ended)).is_none());
    }

    #[test]
    fn refusal_codes_decide_the_retry_verdict_and_the_wire_code() {
        let rows = [
            (SERVER_CANCELLED, true),
            (CONTENT_MODIFIED, true),
            (lsp_types::error_codes::REQUEST_CANCELLED, false),
            (METHOD_NOT_FOUND_CODE, false),
            (1, false),
        ];
        for (code, retryable) in rows {
            let fault = EngineFault::Refused {
                method: "textDocument/diagnostic".to_owned(),
                code,
                message: "engine words".to_owned(),
            };
            assert_eq!(fault.is_retryable_refusal(), retryable, "code {code}");
            let expected = if retryable {
                ErrorCode::TemporarilyUnavailable
            } else {
                ErrorCode::InvalidRequest
            };
            assert_eq!(
                Error::new(fault).name(),
                ErrorName::Wire(expected),
                "code {code}"
            );
        }
        assert!(
            !EngineFault::Ended.is_retryable_refusal(),
            "a fault that is not a refusal is never a retryable refusal"
        );
        assert!(
            !EngineFault::Ended.is_refusal(),
            "a fault the engine never answered is not its reply"
        );
        assert!(
            EngineFault::Refused {
                method: "textDocument/rename".to_owned(),
                code: -32602,
                message: "No references found at position".to_owned(),
            }
            .is_refusal(),
            "a verdict the engine answered is still its reply"
        );
    }

    #[test]
    fn published_diagnostics_record_is_bounded_by_documents_and_items() {
        let mut record = BTreeMap::new();
        let path =
            |index: usize| ProjectPath::new(format!("src/file_{index}.rs")).expect("fixture path");
        let item = Diagnostic::default();
        for index in 0..PUBLISHED_DOCUMENTS_MAX {
            retain_published(&mut record, path(index), vec![item.clone()]);
        }
        assert_eq!(record.len(), PUBLISHED_DOCUMENTS_MAX);
        retain_published(
            &mut record,
            path(PUBLISHED_DOCUMENTS_MAX),
            vec![item.clone()],
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
        );
        assert_eq!(
            record.get(&path(0)).map(Vec::len),
            Some(DOCUMENT_DIAGNOSTICS_MAX),
            "a retained document is replaced and truncated at the bound"
        );
    }

    #[test]
    fn progress_record_retires_ended_tokens_and_stays_bounded() {
        let token = |index: usize| ProgressToken::String(format!("rift/work/{index}"));
        let mut record = WorkProgress::default();
        assert!(
            !record.is_announced() && !record.is_outstanding(),
            "a session that has read no progress has heard no announcement"
        );
        record.began(token(0));
        record.began(token(0));
        assert_eq!(
            record.outstanding.len(),
            1,
            "a token already outstanding is not doubled"
        );
        record.ended(&token(0));
        assert!(!record.is_outstanding(), "an ended token retires");
        assert!(
            record.is_announced(),
            "the announcement outlives the work it announced"
        );
        for index in 0..PROGRESS_TOKENS_MAX {
            record.began(token(index));
        }
        record.began(token(PROGRESS_TOKENS_MAX));
        assert_eq!(
            record.outstanding.len(),
            PROGRESS_TOKENS_MAX,
            "a token past the bound is dropped"
        );
    }

    #[test]
    fn empty_answers_claim_one_resend_per_operation_and_forget_between_requests() {
        let mut record = EmptyAnswers::default();
        assert!(
            !record.claim(),
            "a session that answered nothing yet claims nothing"
        );
        record.record(EmptyAnswer::WillRenameFiles, true);
        assert!(record.claim(), "the first empty answer claims its resend");
        record.record(EmptyAnswer::WillRenameFiles, true);
        assert!(
            !record.claim(),
            "the operation's one resend per session is spent"
        );
        record.record(EmptyAnswer::Diagnostics, true);
        assert!(record.claim(), "each operation carries its own resend");
        record.record(EmptyAnswer::Diagnostics, false);
        assert!(
            !record.claim(),
            "an answer that said something claims nothing"
        );
        record.record(EmptyAnswer::WillRenameFiles, true);
        record.forget();
        assert!(
            !record.claim(),
            "a request forgets the verdict the previous answer left"
        );
    }

    /// One will-rename answer per proposal shape, and whether it proposes
    /// an edit: `null`, both empty carriers, and each carrier holding one.
    fn will_rename_answers() -> Vec<(Option<WorkspaceEdit>, bool, &'static str)> {
        use lsp_types::{
            CreateFile, OneOf, OptionalVersionedTextDocumentIdentifier, ResourceOp,
            TextDocumentEdit, TextEdit,
        };
        let uri: lsp_types::Uri = "file:///workspace/lib.rs".parse().expect("fixture uri");
        let created: lsp_types::Uri = "file:///workspace/new.rs".parse().expect("fixture uri");
        let edit = TextEdit {
            range: lsp_types::Range::default(),
            new_text: "renamed".to_owned(),
        };
        let document =
            |edits: Vec<OneOf<TextEdit, lsp_types::AnnotatedTextEdit>>| TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri.clone(),
                    version: None,
                },
                edits,
            };
        let changed = |edits: Vec<TextEdit>| WorkspaceEdit {
            changes: Some([(uri.clone(), edits)].into_iter().collect()),
            ..WorkspaceEdit::default()
        };
        let versioned = |changes: DocumentChanges| WorkspaceEdit {
            document_changes: Some(changes),
            ..WorkspaceEdit::default()
        };
        vec![
            (None, true, "a null answer proposes nothing"),
            (Some(WorkspaceEdit::default()), true, "no carrier at all"),
            (Some(changed(Vec::new())), true, "a document with no edit"),
            (
                Some(versioned(DocumentChanges::Edits(vec![
                    document(Vec::new()),
                ]))),
                true,
                "a versioned document with no edit",
            ),
            (
                Some(versioned(DocumentChanges::Operations(vec![
                    DocumentChangeOperation::Edit(document(Vec::new())),
                ]))),
                true,
                "a mixed operation list with no edit",
            ),
            (
                Some(changed(vec![edit.clone()])),
                false,
                "one edit in the changes map",
            ),
            (
                Some(versioned(DocumentChanges::Edits(vec![document(vec![
                    OneOf::Left(edit),
                ])]))),
                false,
                "one versioned edit",
            ),
            (
                Some(versioned(DocumentChanges::Operations(vec![
                    DocumentChangeOperation::Op(ResourceOp::Create(CreateFile {
                        uri: created,
                        options: None,
                        annotation_id: None,
                    })),
                ]))),
                false,
                "a file operation is an edit even with no text of its own",
            ),
        ]
    }

    #[test]
    fn a_proposal_without_an_edit_says_what_no_answer_says() {
        for (answer, empty, evidence) in will_rename_answers() {
            assert_eq!(proposes_no_edit(answer.as_ref()), empty, "{evidence}");
        }
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
