//! `rift server` lifecycle commands over the workspace election.
//!
//! `start` spawns a detached `rift server start --foreground` and waits for
//! its published lock document; `stop` asks the recorded server to shut
//! down over its own stop route; `restart` chains the two; `status` prints
//! one probe's classification and changes nothing. Every wait is a bounded
//! poll over [`rift_mcp::probe`] - the election module itself never polls.

use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use jiff::fmt::temporal::DateTimePrinter;
use jiff::tz::TimeZone;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_core::{CliCode, Error, ErrorContext, ErrorName, Fault};
use rift_index::{
    LOG_PAGE_RECORDS_MAX, LexicalIndexError, LogQuery, LogRecord, LogStore, StoredLogRecord,
};
use rift_mcp::{
    ElectionError, ElectionFault, LogDrain, PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT,
    START_WAIT_MAX, ServerPresence, StaleReason, WorkspaceStorage, probe, read_serving,
    serve_elected_with_storage, spawn_detached_server,
};
use rift_protocol::lock::ServerLock;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Longest wait for an asked server to leave the serving state.
///
/// The stop poll runs `STOP_WAIT_MAX / PRESENCE_POLL_INTERVAL` = 100
/// bounded iterations.
const STOP_WAIT_MAX: Duration = Duration::from_secs(10);
/// Probe attempts one stop waits: `STOP_WAIT_MAX` over the interval.
const STOP_POLL_ATTEMPT_COUNT: u32 = 100;
/// Bound on the whole stop request: connect, send, and read the answer.
const STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for queued diagnostics to reach the workspace database.
const LOG_DRAIN_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Wall-clock span between two polls of the store while following.
const LOG_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The form `--tail` accepts, named in every refusal.
const TAIL_COUNT_EXPECTED: &str = "`all`, or a positive integer such as `20`";
/// What a workspace holding no recorded diagnostics prints on stderr.
const NO_RECORDED_LOGS: &str = "💤 no server diagnostics recorded for this workspace yet; \
                                start one with `rift server start`";
/// Renders a logged instant with exactly 3 fractional-second digits.
///
/// `DateTimePrinter::new` and `precision` are both `const fn`, so the
/// configured printer is a compile-time value shared by every render.
const TIMESTAMP_PRINTER: DateTimePrinter = DateTimePrinter::new().precision(Some(3));

/// Failure while running one `rift server` command.
pub(super) type ServerCommandError = Error<ServerCommandFault>;

/// One server-command failure: what kept the workspace's server from
/// reaching the asked state.
#[derive(Debug)]
pub(super) enum ServerCommandFault {
    /// A rift server already serves this workspace, refusing a foreground
    /// start. Carries the holder's document when the probe could read one.
    AlreadyServing { holder: Option<Box<ServerLock>> },
    /// The detached server process could not be spawned.
    SpawnFailed { source: io::Error },
    /// The spawned server did not publish within [`START_WAIT_MAX`].
    StartTimedOut,
    /// The stop request could not be delivered.
    StopRequestFailed { source: reqwest::Error },
    /// The server answered the stop request with something other than
    /// acceptance.
    StopRefused { status: reqwest::StatusCode },
    /// The server accepted the stop but kept serving past
    /// [`STOP_WAIT_MAX`]. Carries the still-serving holder's document.
    StopTimedOut { holder: Box<ServerLock> },
    /// The election refused or failed while serving in the foreground.
    Election(Box<ElectionError>),
    /// The workspace database exists but its recorded diagnostics could not be
    /// read. Carries the store's own failure when a query reached it; a
    /// database that never opened leaves none, having reported on stderr.
    LogsUnavailable {
        source: Option<Box<LexicalIndexError>>,
    },
}

impl Fault for ServerCommandFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::AlreadyServing { .. } => ErrorName::Cli(CliCode::ServerAlreadyServing),
            Self::SpawnFailed { .. } => ErrorName::Cli(CliCode::ServerStartFailed),
            Self::StartTimedOut => ErrorName::Cli(CliCode::ServerStartTimedOut),
            Self::StopRequestFailed { .. }
            | Self::StopRefused { .. }
            | Self::StopTimedOut { .. } => ErrorName::Cli(CliCode::ServerStopFailed),
            Self::LogsUnavailable { .. } => ErrorName::Cli(CliCode::ServerLogsUnavailable),
            Self::Election(source) => source.name(),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::AlreadyServing { holder } => holder_evidence(holder.as_deref()),
            Self::SpawnFailed { .. } => {
                vec![ErrorContext::new("operation", "spawn detached server")]
            }
            Self::StartTimedOut => vec![ErrorContext::new("waited", format!("{START_WAIT_MAX:?}"))],
            Self::StopRequestFailed { .. } => {
                vec![ErrorContext::new("operation", "stop request")]
            }
            Self::StopRefused { status } => stop_refusal_evidence(*status),
            Self::StopTimedOut { holder } => {
                let mut evidence = vec![ErrorContext::new("waited", format!("{STOP_WAIT_MAX:?}"))];
                evidence.extend(holder_evidence(Some(holder.as_ref())));
                evidence
            }
            Self::LogsUnavailable { source: Some(_) } => {
                vec![ErrorContext::new("operation", "read recorded logs")]
            }
            Self::LogsUnavailable { source: None } => vec![ErrorContext::new(
                "detail",
                "the workspace database at `.rift/db` did not open",
            )],
            Self::Election(source) => source.context(),
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyServing { .. }
            | Self::StartTimedOut
            | Self::StopRefused { .. }
            | Self::StopTimedOut { .. } => None,
            Self::SpawnFailed { source } => Some(source),
            Self::StopRequestFailed { source } => Some(source),
            Self::Election(source) => Some(source),
            Self::LogsUnavailable { source } => source
                .as_deref()
                .map(|error| error as &(dyn std::error::Error + 'static)),
        }
    }
}

/// The holder's address facts, when a published document names them.
fn holder_evidence(holder: Option<&ServerLock>) -> Vec<ErrorContext> {
    match holder {
        Some(lock) => vec![
            ErrorContext::new("listening", format!("127.0.0.1:{}", lock.port)),
            ErrorContext::new("pid", lock.pid.to_string()),
        ],
        None => vec![ErrorContext::new(
            "detail",
            "the holding server has not published its lock document yet",
        )],
    }
}

/// Evidence for a refused stop, naming the token mismatch a `401` implies.
fn stop_refusal_evidence(status: reqwest::StatusCode) -> Vec<ErrorContext> {
    let mut evidence = vec![ErrorContext::new("status", status.as_u16().to_string())];
    if status == reqwest::StatusCode::UNAUTHORIZED {
        evidence.push(ErrorContext::new(
            "detail",
            "the recorded bearer token was refused; the lock document may be stale",
        ));
    }
    evidence
}

/// `rift server` subcommands.
#[derive(Debug, clap::Subcommand)]
pub(super) enum ServerCommand {
    /// Start this workspace's server, detached unless --foreground.
    Start {
        /// Serve in this process instead of spawning a detached one.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop this workspace's server.
    Stop,
    /// Stop this workspace's server, then start a fresh detached one.
    Restart,
    /// Report whether this workspace's server is serving.
    Status,
    /// Print this workspace's recorded server diagnostics, oldest first.
    Logs {
        /// Keep printing records as the server writes them.
        #[arg(short, long)]
        follow: bool,
        /// Print only the newest COUNT records; `all` prints every kept record.
        #[arg(short = 'n', long, default_value = "all", value_name = "COUNT")]
        tail: TailCount,
        /// Print only records newer than DURATION ago, such as `10m` or `2h`.
        #[arg(
            long,
            value_name = "DURATION",
            value_parser = rift_protocol::configuration::Duration::parse
        )]
        since: Option<rift_protocol::configuration::Duration>,
        /// Print only records at this severity, as the store spells it.
        #[arg(long, value_name = "LEVEL")]
        level: Option<LogLevel>,
        /// Print only records one component emitted, as its spans label it:
        /// index, search, engine, change, or logs.
        #[arg(long, value_name = "NAME")]
        component: Option<String>,
    },
}

/// How many recorded records the initial print carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TailCount {
    /// Every record the store still keeps.
    All,
    /// The newest `count` records.
    Newest(u64),
}

impl FromStr for TailCount {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text == "all" {
            return Ok(Self::All);
        }
        match text.parse::<u64>() {
            Ok(0) | Err(_) => Err(format!("expected {TAIL_COUNT_EXPECTED}, not {text:?}")),
            Ok(count) => Ok(Self::Newest(count)),
        }
    }
}

/// One severity a logs read is restricted to, in the store's own spelling.
///
/// The variants carry no documentation of their own: clap renders a value's
/// doc comment as per-value help, which turns the whole command's help into
/// its long form, and the five levels need no gloss beyond their names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// The store's own spelling for this severity.
    const fn label(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Where a printed logs read stops.
#[derive(Debug)]
enum LogsMode {
    /// Print what the store holds and return.
    Once,
    /// Print what the store holds, then keep printing what lands.
    Following,
}

/// The clap `--follow` flag as the mode the read dispatches on.
fn logs_mode(follow: bool) -> LogsMode {
    if follow {
        LogsMode::Following
    } else {
        LogsMode::Once
    }
}

/// Where a started server runs.
#[derive(Debug)]
enum StartMode {
    /// Spawn a detached process and wait for its published document.
    Detached,
    /// Serve in this process until interrupted or stopped.
    Foreground,
}

/// The clap `--foreground` flag as the mode everything downstream
/// dispatches on.
fn start_mode(foreground: bool) -> StartMode {
    if foreground {
        StartMode::Foreground
    } else {
        StartMode::Detached
    }
}

/// What a completed server command prints.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ServerOutcome {
    /// A server now serves the workspace at the contained address.
    Listening { port: u16, pid: u32 },
    /// A server was already serving; nothing was started.
    AlreadyListening { port: u16, pid: u32 },
    /// The serving server was stopped.
    Stopped,
    /// No server was serving the workspace.
    NotRunning,
    /// A probe found the workspace's server serving at the contained
    /// address, built at the contained version.
    Serving {
        port: u16,
        pid: u32,
        version: String,
    },
    /// Lock state exists but names no live server; the next starter
    /// replaces it. Carries the probe's reason as one phrase.
    Stale { reason: &'static str },
}

impl fmt::Display for ServerOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listening { port, pid } => {
                write!(
                    formatter,
                    "🚀 rift server listening on 127.0.0.1:{port} (pid {pid})"
                )
            }
            Self::AlreadyListening { port, pid } => write!(
                formatter,
                "✅ rift server already listening on 127.0.0.1:{port} (pid {pid})"
            ),
            Self::Stopped => formatter.write_str("🛑 rift server stopped"),
            Self::NotRunning => {
                formatter.write_str("💤 no rift server is running for this workspace")
            }
            Self::Serving { port, pid, version } => write!(
                formatter,
                "✅ rift server listening on 127.0.0.1:{port} (pid {pid}, v{version})"
            ),
            Self::Stale { reason } => write!(
                formatter,
                "🧹 found a stale .rift/server.json ({reason}); the next rift mcp or rift server start replaces it"
            ),
        }
    }
}

/// Runs one `rift server` command against the current directory's
/// workspace.
///
/// A foreground start prints its listening line itself before blocking, and
/// `logs` prints the records it read; both complete with nothing left to
/// print. Every other command completes with its outcome line.
///
/// # Errors
///
/// Returns [`ServerCommandError`] when the asked state was not reached.
pub(super) async fn run(
    command: ServerCommand,
    drain: Option<LogDrain>,
    retention_records: u64,
) -> Result<Option<ServerOutcome>, ServerCommandError> {
    let root = Path::new(".");
    match command {
        ServerCommand::Start { foreground } => match start_mode(foreground) {
            StartMode::Detached => start_detached(root).await.map(Some),
            StartMode::Foreground => serve_foreground(root, drain, retention_records)
                .await
                .map(|()| None),
        },
        ServerCommand::Stop => stop(root).await.map(Some),
        ServerCommand::Restart => restart(root).await.map(Some),
        ServerCommand::Status => Ok(Some(status(root))),
        ServerCommand::Logs {
            follow,
            tail,
            since,
            level,
            component,
        } => {
            let query = logs_query(tail, since, level, component.as_deref());
            let time_zone = TimeZone::system();
            print_logs(root, &query, tail, &logs_mode(follow), &time_zone)
                .await
                .map(|()| None)
        }
    }
}

/// Reports the workspace's lock state without changing it.
///
/// One probe, no HTTP request, no mutation: a stale document stays in
/// place for the next `rift mcp` or `rift server start` to replace.
fn status(root: &Path) -> ServerOutcome {
    match probe(root) {
        ServerPresence::Serving(lock) => ServerOutcome::Serving {
            port: lock.port,
            pid: lock.pid,
            version: lock.identity.version,
        },
        ServerPresence::Stale(reason) => ServerOutcome::Stale {
            reason: stale_reason_phrase(&reason),
        },
        ServerPresence::Absent => ServerOutcome::NotRunning,
    }
}

/// The probe's stale classification as one operator-facing phrase.
fn stale_reason_phrase(reason: &StaleReason) -> &'static str {
    match reason {
        StaleReason::DocumentUnreadable => "the document could not be read",
        StaleReason::DocumentMalformed => "the document is malformed",
        StaleReason::DocumentInvalid(_) => "the document breaks the lock contract",
        StaleReason::ElectionUnheld => "no process holds the election lock",
        StaleReason::ElectionUnobservable => "the election lock state could not be observed",
    }
}

/// Starts a detached server unless one already serves, and waits for it.
///
/// Repeats are idempotent: an already-serving workspace answers with the
/// running server's address and starts nothing.
async fn start_detached(root: &Path) -> Result<ServerOutcome, ServerCommandError> {
    if let Some(lock) = read_serving(root) {
        return Ok(ServerOutcome::AlreadyListening {
            port: lock.port,
            pid: lock.pid,
        });
    }
    // A stale document keeps its bytes until the elected child scrubs it.
    // Remember them so the wait below never answers with the old document
    // read in the instant the child already holds the election.
    let stale_bytes = std::fs::read(rift_mcp::document_path(root)).ok();
    spawn_detached_server(root)
        .map_err(|source| Error::new(ServerCommandFault::SpawnFailed { source }))?;
    await_serving(root, START_POLL_ATTEMPT_COUNT, stale_bytes.as_deref()).await
}

/// Polls until the workspace serves, bounded by `attempt_count` probes.
///
/// The caller passes [`START_POLL_ATTEMPT_COUNT`], which derives from
/// [`START_WAIT_MAX`] over the poll interval. A poll that finds the document
/// still byte-equal to `stale_bytes` - the pre-spawn leftover - keeps
/// waiting: the started server always publishes a fresh document (its own
/// pid, token, and port), so the leftover can only mean the child has not
/// published yet.
async fn await_serving(
    root: &Path,
    attempt_count: u32,
    stale_bytes: Option<&[u8]>,
) -> Result<ServerOutcome, ServerCommandError> {
    for _ in 0..attempt_count {
        let leftover_unscrubbed = match (stale_bytes, std::fs::read(rift_mcp::document_path(root)))
        {
            (Some(stale), Ok(current)) => stale == current.as_slice(),
            _ => false,
        };
        if !leftover_unscrubbed && let Some(lock) = read_serving(root) {
            return Ok(ServerOutcome::Listening {
                port: lock.port,
                pid: lock.pid,
            });
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err(Error::new(ServerCommandFault::StartTimedOut))
}

/// Serves the workspace in this process until interrupted or stopped.
///
/// The listening line prints before blocking. Ctrl-C cancels the shutdown
/// token; an authorized stop request and the idle timeout end serving the
/// same way. This is the process that records: the drain writes what the
/// tracing layer queued into the workspace database until the same token
/// stops it.
async fn serve_foreground(
    root: &Path,
    drain: Option<LogDrain>,
    retention_records: u64,
) -> Result<(), ServerCommandError> {
    let shutdown = CancellationToken::new();
    let storage = WorkspaceStorage::open(root).await;
    // The drain starts before election, so a start that refuses is recorded too: the
    // workspace that already has a server is exactly the one whose operator is about to
    // ask why this one would not serve.
    let log_drain = match (drain, storage.logs()) {
        (Some(drain), Some(store)) => Some(tokio::spawn(drain.run(
            store,
            retention_records,
            shutdown.clone(),
        ))),
        _ => None,
    };
    let server = match serve_elected_with_storage(root, shutdown.clone(), storage).await {
        Ok(server) => server,
        Err(error) => {
            shutdown.cancel();
            stop_log_drain(log_drain).await;
            return Err(foreground_refused(root, error));
        }
    };
    println!(
        "{}",
        ServerOutcome::Listening {
            port: server.port(),
            pid: std::process::id(),
        }
    );
    let interrupt = tokio::spawn(cancel_on_interrupt(shutdown.clone()));
    let stopped = server
        .stopped()
        .await
        .map_err(|error| Error::new(ServerCommandFault::Election(Box::new(error))));
    shutdown.cancel();
    interrupt.abort();
    let _ = interrupt.await;
    stop_log_drain(log_drain).await;
    stopped
}

/// Joins the diagnostics drain within the foreground server's shutdown deadline.
async fn stop_log_drain(drain: Option<tokio::task::JoinHandle<()>>) {
    let Some(mut drain) = drain else {
        return;
    };
    match tokio::time::timeout(LOG_DRAIN_SHUTDOWN_TIMEOUT, &mut drain).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(component = "logs", %error, "log drain task failed"),
        Err(_) => {
            drain.abort();
            let _ = drain.await;
            tracing::warn!(
                component = "logs",
                timeout = ?LOG_DRAIN_SHUTDOWN_TIMEOUT,
                "log drain missed its shutdown deadline"
            );
        }
    }
}

/// Cancels `shutdown` when the process receives an interrupt.
///
/// # Cancel safety
///
/// The task ends with the process; dropping it merely stops listening for
/// the interrupt.
async fn cancel_on_interrupt(shutdown: CancellationToken) {
    match tokio::signal::ctrl_c().await {
        Ok(()) => shutdown.cancel(),
        Err(error) => tracing::warn!(component = "cli", %error, "interrupt listener failed"),
    }
}

/// Attaches the holder's address facts to a foreground refusal.
fn foreground_refused(root: &Path, error: ElectionError) -> ServerCommandError {
    if matches!(error.fault(), ElectionFault::AlreadyServing) {
        return Error::new(ServerCommandFault::AlreadyServing {
            holder: read_serving(root).map(Box::new),
        });
    }
    Error::new(ServerCommandFault::Election(Box::new(error)))
}

/// Stops the serving server, treating a workspace without one as done.
async fn stop(root: &Path) -> Result<ServerOutcome, ServerCommandError> {
    let lock = match probe(root) {
        ServerPresence::Serving(lock) => lock,
        ServerPresence::Stale(_) => {
            discard_stale_document(root);
            return Ok(ServerOutcome::NotRunning);
        }
        ServerPresence::Absent => return Ok(ServerOutcome::NotRunning),
    };
    match request_stop(&lock).await? {
        StopAnswer::Accepted => {
            await_stopped(root, lock, STOP_POLL_ATTEMPT_COUNT).await?;
            Ok(ServerOutcome::Stopped)
        }
        StopAnswer::NothingListening => Ok(ServerOutcome::Stopped),
    }
}

/// How the recorded server answered the stop request.
#[derive(Debug)]
enum StopAnswer {
    /// `202`: the server accepted the stop and is shutting down.
    Accepted,
    /// The connection was refused: nothing listens on the recorded port,
    /// so the server is already gone.
    NothingListening,
}

/// Delivers the authorized stop request, bounded by
/// [`STOP_REQUEST_TIMEOUT`].
async fn request_stop(lock: &ServerLock) -> Result<StopAnswer, ServerCommandError> {
    let request_failed =
        |source: reqwest::Error| Error::new(ServerCommandFault::StopRequestFailed { source });
    let client = reqwest::Client::builder()
        .timeout(STOP_REQUEST_TIMEOUT)
        .build()
        .map_err(request_failed)?;
    let answer = client
        .post(format!("http://127.0.0.1:{}/api/stop", lock.port))
        .bearer_auth(&lock.token)
        .send()
        .await;
    match answer {
        Ok(response) if response.status() == reqwest::StatusCode::ACCEPTED => {
            Ok(StopAnswer::Accepted)
        }
        Ok(response) => Err(Error::new(ServerCommandFault::StopRefused {
            status: response.status(),
        })),
        Err(error) if error.is_connect() => Ok(StopAnswer::NothingListening),
        Err(error) => Err(request_failed(error)),
    }
}

/// Polls until the workspace stops serving, bounded by `attempt_count`
/// probes.
///
/// The caller passes [`STOP_POLL_ATTEMPT_COUNT`], which derives from
/// [`STOP_WAIT_MAX`] over the poll interval.
async fn await_stopped(
    root: &Path,
    holder: ServerLock,
    attempt_count: u32,
) -> Result<(), ServerCommandError> {
    for _ in 0..attempt_count {
        if !matches!(probe(root), ServerPresence::Serving(_)) {
            return Ok(());
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err(Error::new(ServerCommandFault::StopTimedOut {
        holder: Box::new(holder),
    }))
}

/// Removes a stale lock document, best effort.
///
/// A document that cannot be removed stays classified as stale by every
/// probe, so failure is reported, not raised.
fn discard_stale_document(root: &Path) {
    let document_path = root
        .join(rift_core::constants::RIFT_STATE_DIRECTORY)
        .join(rift_protocol::lock::SERVER_LOCK_FILE_NAME);
    match std::fs::remove_file(&document_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            component = "cli",
            path = %document_path.display(),
            %error,
            "stale server lock document could not be removed"
        ),
    }
}

/// Stops the serving server, then starts a fresh detached one.
async fn restart(root: &Path) -> Result<ServerOutcome, ServerCommandError> {
    stop(root).await?;
    start_detached(root).await
}

/// The store read one `rift server logs` run issues.
///
/// The page is the `--tail` count, bounded by [`LOG_PAGE_RECORDS_MAX`]; `all`
/// reads a whole page at a time. `--since` becomes an absolute floor here, so
/// every page of one run selects the same window.
fn logs_query(
    tail: TailCount,
    since: Option<rift_protocol::configuration::Duration>,
    level: Option<LogLevel>,
    component: Option<&str>,
) -> LogQuery {
    let limit = match tail {
        TailCount::All => LOG_PAGE_RECORDS_MAX,
        TailCount::Newest(count) => usize::try_from(count).unwrap_or(LOG_PAGE_RECORDS_MAX),
    };
    let mut query = LogQuery::newest(limit);
    if let Some(level) = level {
        query = query.at_level(level.label());
    }
    if let Some(component) = component {
        query = query.for_component(component);
    }
    if let Some(since) = since {
        let age_ms = i64::try_from(since.milliseconds()).unwrap_or(i64::MAX);
        query = query.since_ms(now_ms().saturating_sub(age_ms));
    }
    query
}

/// Prints this workspace's recorded diagnostics, oldest first.
///
/// The store is read directly, so a workspace whose server has stopped still
/// answers. A workspace holding no `.rift/db` prints nothing, says so on
/// stderr, and creates no state directory.
async fn print_logs(
    root: &Path,
    query: &LogQuery,
    tail: TailCount,
    mode: &LogsMode,
    time_zone: &TimeZone,
) -> Result<(), ServerCommandError> {
    let database = root
        .join(RIFT_STATE_DIRECTORY)
        .join(WORKSPACE_DATABASE_FILE_NAME);
    if !database.exists() {
        eprintln!("{NO_RECORDED_LOGS}");
        return Ok(());
    }
    let Some(store) = WorkspaceStorage::open(root).await.logs() else {
        return Err(Error::new(ServerCommandFault::LogsUnavailable {
            source: None,
        }));
    };
    let printed = match tail {
        TailCount::All => print_records_after(&store, query, 0, time_zone).await?,
        TailCount::Newest(_) => print_newest_records(&store, query, time_zone).await?,
    };
    match mode {
        LogsMode::Once => Ok(()),
        LogsMode::Following => follow_records(&store, query, printed, time_zone).await,
    }
}

/// Prints every record after `after`, oldest first, and returns the newest
/// identity it printed.
///
/// One read is bounded by the query's own page, itself bounded by
/// [`LOG_PAGE_RECORDS_MAX`]. The loop repeats only while a page comes back
/// full, and the store's retention bounds how many full pages there can be.
async fn print_records_after(
    store: &LogStore,
    query: &LogQuery,
    after: i64,
    time_zone: &TimeZone,
) -> Result<i64, ServerCommandError> {
    let mut newest = after;
    loop {
        let page = store
            .following(&query.clone().after(newest))
            .await
            .map_err(logs_unavailable)?;
        for stored in &page {
            let line = rendered_record(stored, time_zone);
            println!("{line}");
            newest = stored.identity();
        }
        if page.len() < query.limit() {
            return Ok(newest);
        }
    }
}

/// Prints the newest records the query selects, oldest first, and returns the
/// newest identity it printed. The read is bounded by the query's own page.
async fn print_newest_records(
    store: &LogStore,
    query: &LogQuery,
    time_zone: &TimeZone,
) -> Result<i64, ServerCommandError> {
    let mut records = store.recent(query).await.map_err(logs_unavailable)?;
    records.reverse();
    let mut newest = 0;
    for stored in &records {
        let line = rendered_record(stored, time_zone);
        println!("{line}");
        newest = stored.identity();
    }
    Ok(newest)
}

/// Prints records as the server writes them, until the operator interrupts.
async fn follow_records(
    store: &LogStore,
    query: &LogQuery,
    printed: i64,
    time_zone: &TimeZone,
) -> Result<(), ServerCommandError> {
    let interrupted = CancellationToken::new();
    let interrupt = tokio::spawn(cancel_on_interrupt(interrupted.clone()));
    let followed = follow_until_interrupt(store, query, printed, &interrupted, time_zone).await;
    interrupt.abort();
    let _ = interrupt.await;
    followed
}

/// Polls the store until `interrupted` is cancelled, printing each new page.
///
/// The loop has no iteration bound by design: it ends on the operator's
/// interrupt, as `docker logs -f` does. Each iteration reads one page, bounded
/// by the query's own limit.
async fn follow_until_interrupt(
    store: &LogStore,
    query: &LogQuery,
    printed: i64,
    interrupted: &CancellationToken,
    time_zone: &TimeZone,
) -> Result<(), ServerCommandError> {
    let mut newest = printed;
    while !interrupted.is_cancelled() {
        newest = print_records_after(store, query, newest, time_zone).await?;
        tokio::select! {
            () = interrupted.cancelled() => {}
            () = tokio::time::sleep(LOG_FOLLOW_POLL_INTERVAL) => {}
        }
    }
    Ok(())
}

/// One store failure as this command's typed refusal.
fn logs_unavailable(source: LexicalIndexError) -> ServerCommandError {
    Error::new(ServerCommandFault::LogsUnavailable {
        source: Some(Box::new(source)),
    })
}

/// One stored record as one printed line.
fn rendered_record(stored: &StoredLogRecord, time_zone: &TimeZone) -> String {
    rendered_line(stored.record(), time_zone)
}

/// One record as the operator reads it: when it happened, how severe it was,
/// where it came from, what it said, and the fields it carried.
fn rendered_line(record: &LogRecord, time_zone: &TimeZone) -> String {
    let timestamp = rendered_timestamp(record.recorded_at_ms(), time_zone);
    let glyph = level_glyph(record.level());
    let level = record.level().to_uppercase();
    let component = label(record.component());
    let operation = label(record.operation());
    let message = record.message();
    let fields = rendered_fields(record.fields());
    format!("{timestamp} {glyph} {level:<5} {component:<8} {operation:<12} {message}{fields}")
}

/// The glyph one severity prints under. A level outside the five the store
/// records prints under the least severe one.
fn level_glyph(level: &str) -> &'static str {
    match level {
        "error" => "🔴",
        "warn" => "🟡",
        "info" => "🔵",
        "debug" => "⚪",
        _ => "⚫",
    }
}

/// The label a record carried, or `-` when it carried none.
fn label(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

/// The record's remaining fields as ` key=value` pairs, or as the text the
/// store holds when that text is not a JSON object.
fn rendered_fields(fields: &str) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let Ok(named) = serde_json::from_str::<Map<String, Value>>(fields) else {
        return format!(" {fields}");
    };
    let mut rendered = String::new();
    for (key, value) in &named {
        rendered.push(' ');
        rendered.push_str(key);
        rendered.push('=');
        rendered.push_str(&rendered_value(value));
    }
    rendered
}

/// One field value without the quotes JSON puts around a string.
fn rendered_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// One recorded instant as an RFC 3339 timestamp in `time_zone`'s local offset.
///
/// Local offset needs the tz database; jiff owns both parsing the recorded
/// millisecond count and rendering it, with exactly 3 fractional digits and
/// a numeric offset - never `Z`, since the offset is always known here. A
/// millisecond count outside jiff's representable range falls back to the
/// raw count instead of panicking.
fn rendered_timestamp(recorded_at_ms: i64, time_zone: &TimeZone) -> String {
    let Ok(timestamp) = Timestamp::from_millisecond(recorded_at_ms) else {
        return recorded_at_ms.to_string();
    };
    let offset = time_zone.to_offset(timestamp);
    TIMESTAMP_PRINTER.timestamp_with_offset_to_string(&timestamp, offset)
}

/// Milliseconds since the Unix epoch, or zero on a clock before it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
pub(super) fn error_for_test() -> ServerCommandError {
    Error::new(ServerCommandFault::StartTimedOut)
}

#[cfg(test)]
mod tests {
    use std::future::IntoFuture as _;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{
        LogLevel, LogsMode, PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX,
        STOP_POLL_ATTEMPT_COUNT, STOP_WAIT_MAX, ServerCommandFault, ServerOutcome, StaleReason,
        StartMode, TailCount, await_serving, await_stopped, discard_stale_document,
        foreground_refused, holder_evidence, label, level_glyph, logs_mode, logs_query,
        logs_unavailable, now_ms, print_logs, rendered_fields, rendered_line, rendered_timestamp,
        stale_reason_phrase, start_mode, status, stop, stop_log_drain,
    };
    use jiff::tz::{Offset, TimeZone};
    use rift_core::Error;
    use rift_index::{
        LOG_BATCH_RECORDS_MAX, LOG_LEVELS, LOG_PAGE_RECORDS_MAX, LogRecord, LogStore,
    };
    use rift_protocol::lock::{ProductIdentity, ServerLock, ServerLockViolation};

    /// Milliseconds in one hour, for fixture instants only.
    const MILLISECONDS_PER_HOUR: i64 = 3_600_000;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn holder() -> ServerLock {
        ServerLock {
            port: 12_345,
            token: "a".repeat(rift_protocol::lock::SERVER_TOKEN_LENGTH),
            pid: 4_242,
            identity: ProductIdentity {
                version: "0.0.11".to_owned(),
                executable_digest: "a".repeat(64),
                schema_digest: "b".repeat(64),
            },
        }
    }

    /// A holder document naming `port`, for tests that answer on it.
    fn holder_on(port: u16) -> ServerLock {
        ServerLock { port, ..holder() }
    }

    /// A reqwest failure built without any request leaving the process.
    fn request_error() -> reqwest::Error {
        reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("an invalid url must fail the request build")
    }

    /// A loopback port nothing listens on: bound to learn the number, then
    /// released.
    fn dead_port() -> TestResult<u16> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        Ok(listener.local_addr()?.port())
    }

    #[test]
    fn poll_attempt_counts_derive_from_their_windows() {
        assert_eq!(
            PRESENCE_POLL_INTERVAL * START_POLL_ATTEMPT_COUNT,
            START_WAIT_MAX
        );
        assert_eq!(
            PRESENCE_POLL_INTERVAL * STOP_POLL_ATTEMPT_COUNT,
            STOP_WAIT_MAX
        );
    }

    #[test]
    fn foreground_flag_selects_the_mode() {
        assert!(matches!(start_mode(true), StartMode::Foreground));
        assert!(matches!(start_mode(false), StartMode::Detached));
    }

    #[tokio::test]
    async fn a_failed_log_drain_is_joined() {
        let drain = tokio::spawn(async { panic!("injected log drain failure") });

        stop_log_drain(Some(drain)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_log_drain_is_aborted_at_its_deadline() {
        struct Stopped(Arc<AtomicBool>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let stopped = Arc::new(AtomicBool::new(false));
        let task_stopped = Arc::clone(&stopped);
        let drain = tokio::spawn(async move {
            let _stopped = Stopped(task_stopped);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        stop_log_drain(Some(drain)).await;

        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn outcomes_print_the_operator_lines() {
        assert_eq!(
            ServerOutcome::Listening {
                port: 12_345,
                pid: 42
            }
            .to_string(),
            "🚀 rift server listening on 127.0.0.1:12345 (pid 42)"
        );
        assert_eq!(
            ServerOutcome::AlreadyListening {
                port: 12_345,
                pid: 42
            }
            .to_string(),
            "✅ rift server already listening on 127.0.0.1:12345 (pid 42)"
        );
        assert_eq!(ServerOutcome::Stopped.to_string(), "🛑 rift server stopped");
        assert_eq!(
            ServerOutcome::NotRunning.to_string(),
            "💤 no rift server is running for this workspace"
        );
        assert_eq!(
            ServerOutcome::Serving {
                port: 12_345,
                pid: 42,
                version: "0.0.11".to_owned(),
            }
            .to_string(),
            "✅ rift server listening on 127.0.0.1:12345 (pid 42, v0.0.11)"
        );
        assert_eq!(
            ServerOutcome::Stale {
                reason: "no process holds the election lock"
            }
            .to_string(),
            "🧹 found a stale .rift/server.json (no process holds the election lock); \
             the next rift mcp or rift server start replaces it"
        );
    }

    #[test]
    fn stale_reason_phrases_name_each_classification() {
        let cases = [
            (
                StaleReason::DocumentUnreadable,
                "the document could not be read",
            ),
            (StaleReason::DocumentMalformed, "the document is malformed"),
            (
                StaleReason::DocumentInvalid(ServerLockViolation::ProcessIdZero),
                "the document breaks the lock contract",
            ),
            (
                StaleReason::ElectionUnheld,
                "no process holds the election lock",
            ),
            (
                StaleReason::ElectionUnobservable,
                "the election lock state could not be observed",
            ),
        ];
        for (reason, phrase) in cases {
            assert_eq!(stale_reason_phrase(&reason), phrase, "{reason:?}");
        }
    }

    #[test]
    fn status_reports_a_serving_holder() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        guard.publish(&holder())?;
        assert_eq!(
            status(directory.path()),
            ServerOutcome::Serving {
                port: 12_345,
                pid: 4_242,
                version: "0.0.11".to_owned(),
            }
        );
        Ok(())
    }

    #[test]
    fn status_reports_a_stale_document_and_keeps_it() -> TestResult {
        let directory = tempfile::tempdir()?;
        let state_directory = directory
            .path()
            .join(rift_core::constants::RIFT_STATE_DIRECTORY);
        std::fs::create_dir_all(&state_directory)?;
        let document_path = state_directory.join(rift_protocol::lock::SERVER_LOCK_FILE_NAME);
        std::fs::write(&document_path, serde_json::to_vec(&holder())?)?;
        assert_eq!(
            status(directory.path()),
            ServerOutcome::Stale {
                reason: "no process holds the election lock"
            }
        );
        assert!(
            document_path.exists(),
            "status never discards the stale document"
        );
        Ok(())
    }

    #[test]
    fn status_reports_an_empty_workspace_as_not_running() {
        let directory = tempfile::tempdir().expect("workspace fixture must build");
        assert_eq!(status(directory.path()), ServerOutcome::NotRunning);
    }

    #[test]
    fn server_faults_carry_registry_codes() {
        let cases: Vec<(super::ServerCommandError, &str)> = vec![
            (
                Error::new(ServerCommandFault::AlreadyServing {
                    holder: Some(Box::new(holder())),
                }),
                "server_already_serving",
            ),
            (
                Error::new(ServerCommandFault::SpawnFailed {
                    source: std::io::Error::other("fixture"),
                }),
                "server_start_failed",
            ),
            (
                Error::new(ServerCommandFault::StartTimedOut),
                "server_start_timed_out",
            ),
            (
                Error::new(ServerCommandFault::StopRefused {
                    status: reqwest::StatusCode::UNAUTHORIZED,
                }),
                "server_stop_failed",
            ),
            (
                Error::new(ServerCommandFault::StopTimedOut {
                    holder: Box::new(holder()),
                }),
                "server_stop_failed",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.descriptor().code(), code, "{error}");
        }
    }

    #[test]
    fn failure_text_names_evidence_and_next_steps() {
        let already = Error::new(ServerCommandFault::AlreadyServing {
            holder: Some(Box::new(holder())),
        })
        .to_string();
        assert!(already.contains("127.0.0.1:12345"), "{already}");
        assert!(already.contains("pid 4242"), "{already}");
        assert!(already.contains("rift server stop"), "{already}");

        let unpublished =
            Error::new(ServerCommandFault::AlreadyServing { holder: None }).to_string();
        assert!(unpublished.contains("has not published"), "{unpublished}");

        let timed_out = Error::new(ServerCommandFault::StartTimedOut).to_string();
        assert!(
            timed_out.contains(&format!("{START_WAIT_MAX:?}")),
            "{timed_out}"
        );
        assert!(timed_out.contains("--foreground"), "{timed_out}");

        let unauthorized = Error::new(ServerCommandFault::StopRefused {
            status: reqwest::StatusCode::UNAUTHORIZED,
        })
        .to_string();
        assert!(unauthorized.contains("status 401"), "{unauthorized}");
        assert!(unauthorized.contains("stale"), "{unauthorized}");

        let refused = Error::new(ServerCommandFault::StopRefused {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        })
        .to_string();
        assert!(refused.contains("status 500"), "{refused}");
        assert!(!refused.contains("stale"), "{refused}");

        let stop_timed_out = Error::new(ServerCommandFault::StopTimedOut {
            holder: Box::new(holder()),
        })
        .to_string();
        assert!(stop_timed_out.contains("10s"), "{stop_timed_out}");
        assert!(stop_timed_out.contains("pid 4242"), "{stop_timed_out}");
    }

    #[test]
    fn spawn_and_request_failures_keep_their_sources() {
        let spawn = Error::new(ServerCommandFault::SpawnFailed {
            source: std::io::Error::other("fixture"),
        });
        assert!(std::error::Error::source(&spawn).is_some());
        assert!(
            std::error::Error::source(&Error::new(ServerCommandFault::StartTimedOut)).is_none()
        );
    }

    #[test]
    fn holder_evidence_names_the_address_or_its_absence() {
        let known = holder_evidence(Some(&holder()));
        assert_eq!(known.len(), 2);
        assert_eq!(known[0].value(), "127.0.0.1:12345");
        assert_eq!(known[1].value(), "4242");
        let unknown = holder_evidence(None);
        assert_eq!(unknown.len(), 1);
    }

    #[test]
    fn election_fault_forwards_identity_evidence_and_source() {
        let election = Error::new(rift_mcp::ElectionFault::AlreadyServing);
        let expected_code = election.descriptor().code();
        let expected_context = election.context();
        let error = Error::new(ServerCommandFault::Election(Box::new(election)));
        assert_eq!(error.descriptor().code(), expected_code);
        assert_eq!(error.context(), expected_context);
        assert!(
            std::error::Error::source(&error).is_some(),
            "the wrapped election failure must stay on the source chain"
        );
    }

    #[test]
    fn spawn_failure_names_its_operation() {
        let rendered = Error::new(ServerCommandFault::SpawnFailed {
            source: std::io::Error::other("fixture"),
        })
        .to_string();
        assert!(rendered.contains("spawn detached server"), "{rendered}");
    }

    #[test]
    fn stop_request_failure_names_its_operation_and_keeps_its_source() {
        let error = Error::new(ServerCommandFault::StopRequestFailed {
            source: request_error(),
        });
        assert_eq!(error.descriptor().code(), "server_stop_failed");
        let rendered = error.to_string();
        assert!(rendered.contains("stop request"), "{rendered}");
        assert!(
            std::error::Error::source(&error).is_some(),
            "the request failure must stay on the source chain"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn await_serving_times_out_against_an_empty_workspace() -> TestResult {
        let directory = tempfile::tempdir()?;
        let error = await_serving(directory.path(), 1, None)
            .await
            .expect_err("a workspace nobody serves must time the wait out");
        assert!(matches!(error.fault(), ServerCommandFault::StartTimedOut));
        Ok(())
    }

    /// The pre-spawn leftover document paired with a held election is not an
    /// answer: the wait holds out for the fresh document the holder
    /// publishes, and returns its facts, not the leftover's.
    #[tokio::test(start_paused = true)]
    async fn await_serving_holds_out_for_the_fresh_document_over_a_leftover() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let leftover = serde_json::to_vec(&holder())?;
        std::fs::write(rift_mcp::document_path(directory.path()), &leftover)?;
        let fresh = ServerLock {
            pid: 9_999,
            ..holder()
        };
        let outcome = {
            let publish = async {
                tokio::time::sleep(PRESENCE_POLL_INTERVAL * 2).await;
                guard.publish(&fresh).expect("the holder must publish");
            };
            let wait = await_serving(directory.path(), START_POLL_ATTEMPT_COUNT, Some(&leftover));
            let (outcome, ()) = tokio::join!(wait, publish);
            outcome?
        };
        assert!(
            matches!(outcome, ServerOutcome::Listening { pid: 9_999, .. }),
            "the wait must answer with the published document: {outcome:?}"
        );
        Ok(())
    }

    /// A leftover that never scrubs keeps the wait unanswered to its bound.
    #[tokio::test(start_paused = true)]
    async fn await_serving_times_out_while_the_leftover_stands() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        let leftover = serde_json::to_vec(&holder())?;
        std::fs::write(rift_mcp::document_path(directory.path()), &leftover)?;
        let error = await_serving(directory.path(), 2, Some(&leftover))
            .await
            .expect_err("an unscrubbed leftover is never an answer");
        assert!(matches!(error.fault(), ServerCommandFault::StartTimedOut));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn await_stopped_times_out_while_the_holder_keeps_serving() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let document = holder();
        guard.publish(&document)?;
        let error = await_stopped(directory.path(), document, 1)
            .await
            .expect_err("a still-serving holder must time the wait out");
        assert!(
            matches!(error.fault(), ServerCommandFault::StopTimedOut { .. }),
            "the timeout must carry the holder: {error:?}"
        );
        Ok(())
    }

    #[test]
    fn foreground_refusal_maps_election_failures() {
        let directory = tempfile::tempdir().expect("workspace fixture must build");
        let already = foreground_refused(
            directory.path(),
            Error::new(rift_mcp::ElectionFault::AlreadyServing),
        );
        assert!(matches!(
            already.fault(),
            ServerCommandFault::AlreadyServing { holder: None }
        ));
        let storage = Error::new(rift_mcp::ElectionFault::Storage {
            operation: "open election file",
            path: directory.path().join(".rift").join("server.lock"),
            source: std::io::Error::other("disk gone"),
        });
        let passed = foreground_refused(directory.path(), storage);
        assert!(matches!(passed.fault(), ServerCommandFault::Election(_)));
        assert_eq!(passed.descriptor().code(), "storage_failure");
    }

    #[tokio::test]
    async fn stop_treats_a_dead_recorded_port_as_stopped() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        guard.publish(&holder_on(dead_port()?))?;
        let outcome = stop(directory.path()).await?;
        assert_eq!(outcome, ServerOutcome::Stopped);
        Ok(())
    }

    #[tokio::test]
    async fn stop_reports_a_refusing_server() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let refuser = axum::Router::new().route(
            "/api/stop",
            axum::routing::post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let serving = tokio::spawn(axum::serve(listener, refuser).into_future());
        guard.publish(&holder_on(port))?;
        let error = stop(directory.path())
            .await
            .expect_err("a refusing server must fail the stop");
        serving.abort();
        assert!(
            matches!(
                error.fault(),
                ServerCommandFault::StopRefused { status }
                    if *status == reqwest::StatusCode::INTERNAL_SERVER_ERROR
            ),
            "the refusal must carry the answered status: {error:?}"
        );
        Ok(())
    }

    #[test]
    fn discard_stale_document_tolerates_a_missing_document() {
        let directory = tempfile::tempdir().expect("workspace fixture must build");
        discard_stale_document(directory.path());
    }

    #[test]
    fn discard_stale_document_reports_an_unremovable_document() -> TestResult {
        let directory = tempfile::tempdir()?;
        let document_path = directory
            .path()
            .join(rift_core::constants::RIFT_STATE_DIRECTORY)
            .join(rift_protocol::lock::SERVER_LOCK_FILE_NAME);
        std::fs::create_dir_all(&document_path)?;
        discard_stale_document(directory.path());
        assert!(
            document_path.exists(),
            "a directory survives the best-effort removal"
        );
        Ok(())
    }

    /// A log store on a temporary database, for the reads these cases drive.
    async fn log_store(directory: &tempfile::TempDir) -> TestResult<LogStore> {
        let database = rift_index::WorkspaceDatabase::open(
            &directory.path().join("db"),
            rift_index::DatabasePool::new(2, 1_000),
        )
        .await?;
        Ok(LogStore::attached(database))
    }

    #[test]
    fn tail_counts_parse_all_and_positive_integers() {
        assert_eq!("all".parse::<TailCount>(), Ok(TailCount::All));
        assert_eq!("5".parse::<TailCount>(), Ok(TailCount::Newest(5)));
        for refused in ["0", "-1", "twenty", "", "5 ", "all "] {
            let refusal = refused
                .parse::<TailCount>()
                .expect_err("only `all` and a positive integer are accepted");
            assert!(refusal.contains("positive integer"), "{refusal}");
            assert!(refusal.contains(refused), "{refusal}");
        }
    }

    #[test]
    fn every_level_label_is_a_spelling_the_store_holds() {
        let variants = <LogLevel as clap::ValueEnum>::value_variants();
        let labels: Vec<&str> = variants.iter().map(|level| level.label()).collect();

        assert_eq!(labels, LOG_LEVELS);
        for level in variants {
            let value = clap::ValueEnum::to_possible_value(level)
                .expect("every level is a selectable value");
            assert_eq!(value.get_name(), level.label());
        }
    }

    #[test]
    fn the_follow_flag_selects_the_mode() {
        assert!(matches!(logs_mode(true), LogsMode::Following));
        assert!(matches!(logs_mode(false), LogsMode::Once));
    }

    #[test]
    fn a_logs_query_carries_its_tail_level_and_component() {
        let query = logs_query(
            TailCount::Newest(20),
            None,
            Some(LogLevel::Warn),
            Some("index"),
        );

        assert_eq!(query.limit(), 20);
        assert_eq!(query.level(), Some("warn"));
        assert_eq!(query.component(), Some("index"));
        assert_eq!(
            logs_query(TailCount::All, None, None, None).limit(),
            LOG_PAGE_RECORDS_MAX
        );
    }

    #[tokio::test]
    async fn a_since_read_selects_only_records_inside_its_window() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let now = now_ms();
        let older = LogRecord::new(
            now - MILLISECONDS_PER_HOUR,
            "info",
            "rift_mcp::server",
            "index",
            "index.build",
            "old",
            "{}",
        );
        let fresh = LogRecord::new(
            now,
            "info",
            "rift_mcp::server",
            "index",
            "index.build",
            "fresh",
            "{}",
        );
        store.append(&[older, fresh], 1_000).await?;
        let since = rift_protocol::configuration::Duration::parse("10m")?;
        assert_eq!(since.milliseconds(), 600_000);
        assert!(rift_protocol::configuration::Duration::parse("10").is_err());

        let read = store
            .following(&logs_query(TailCount::All, Some(since), None, None))
            .await?;

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].record().message(), "fresh");
        Ok(())
    }

    #[tokio::test]
    async fn a_workspace_without_a_database_prints_nothing_and_creates_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let query = logs_query(TailCount::All, None, None, None);

        print_logs(
            directory.path(),
            &query,
            TailCount::All,
            &LogsMode::Once,
            &TimeZone::UTC,
        )
        .await?;

        assert!(
            !directory.path().join(".rift").exists(),
            "a logs read never creates the state directory"
        );
        Ok(())
    }

    #[test]
    fn an_unopened_database_names_itself_in_the_refusal() {
        let error = Error::new(ServerCommandFault::LogsUnavailable { source: None });

        assert_eq!(error.descriptor().code(), "server_logs_unavailable");
        let rendered = error.to_string();
        assert!(rendered.contains("did not open"), "{rendered}");
        assert!(rendered.contains(".rift/db"), "{rendered}");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[tokio::test]
    async fn a_refused_read_keeps_the_store_failure_on_its_source_chain() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let oversized: Vec<LogRecord> = (0..=LOG_BATCH_RECORDS_MAX)
            .map(|_| LogRecord::new(0, "info", "rift", "index", "index.build", "x", "{}"))
            .collect();
        let refused = store
            .append(&oversized, 1_000)
            .await
            .expect_err("an oversized batch must be refused");

        let error = logs_unavailable(refused);

        assert_eq!(error.descriptor().code(), "server_logs_unavailable");
        let rendered = error.to_string();
        assert!(rendered.contains("read recorded logs"), "{rendered}");
        assert!(
            std::error::Error::source(&error).is_some(),
            "the store failure must stay on the source chain"
        );
        Ok(())
    }

    #[test]
    fn a_rendered_line_carries_every_column() {
        let record = LogRecord::new(
            1_756_552_944_123,
            "info",
            "rift_mcp::server",
            "index",
            "rebuild",
            "published 412 units",
            "{\"unit_count\":412}",
        );

        assert_eq!(
            rendered_line(&record, &TimeZone::UTC),
            "2025-08-30T11:22:24.123+00:00 🔵 INFO  index    rebuild      \
             published 412 units unit_count=412"
        );
    }

    #[test]
    fn a_record_without_labels_prints_a_dash_in_each_column() {
        let record = LogRecord::new(0, "warn", "rift", "", "", "late", "{}");

        assert_eq!(
            rendered_line(&record, &TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00 🟡 WARN  -        -            late"
        );
        assert_eq!(label(""), "-");
        assert_eq!(label("index"), "index");
    }

    #[test]
    fn every_level_prints_its_own_glyph() {
        for (level, glyph) in [
            ("error", "🔴"),
            ("warn", "🟡"),
            ("info", "🔵"),
            ("debug", "⚪"),
            ("trace", "⚫"),
            ("loud", "⚫"),
        ] {
            assert_eq!(level_glyph(level), glyph, "{level}");
        }
    }

    #[test]
    fn fields_print_as_pairs_or_as_the_text_the_store_holds() {
        assert_eq!(rendered_fields("{}"), "");
        assert_eq!(rendered_fields(""), "");
        assert_eq!(
            rendered_fields("{\"epoch\":\"4\",\"count\":7}"),
            " count=7 epoch=4"
        );
        assert_eq!(rendered_fields("not json"), " not json");
        assert_eq!(rendered_fields("[1]"), " [1]");
    }

    #[test]
    fn rendered_timestamp_uses_the_given_time_zones_offset() {
        assert_eq!(
            rendered_timestamp(0, &TimeZone::UTC),
            "1970-01-01T00:00:00.000+00:00"
        );
        assert_eq!(
            rendered_timestamp(-1, &TimeZone::UTC),
            "1969-12-31T23:59:59.999+00:00"
        );

        let positive = TimeZone::fixed(Offset::from_hours(2).expect("+2h must be a valid offset"));
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &positive),
            "2025-08-30T13:22:24.123+02:00"
        );

        let negative = TimeZone::fixed(Offset::from_hours(-5).expect("-5h must be a valid offset"));
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &negative),
            "2025-08-30T06:22:24.123-05:00"
        );

        let berlin =
            TimeZone::get("Europe/Berlin").expect("the tz database must carry Europe/Berlin");
        assert_eq!(
            rendered_timestamp(1_756_552_944_123, &berlin),
            "2025-08-30T13:22:24.123+02:00",
            "August is daylight saving time in Berlin, CEST"
        );
        assert_eq!(
            rendered_timestamp(1_736_940_144_123, &berlin),
            "2025-01-15T12:22:24.123+01:00",
            "January is standard time in Berlin, CET"
        );
    }

    #[test]
    fn an_out_of_range_millisecond_count_falls_back_to_the_raw_count() {
        assert_eq!(
            rendered_timestamp(i64::MAX, &TimeZone::UTC),
            i64::MAX.to_string()
        );
        assert_eq!(
            rendered_timestamp(i64::MIN, &TimeZone::UTC),
            i64::MIN.to_string()
        );
    }
}
