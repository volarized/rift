//! `rift server` lifecycle commands over the workspace election.
//!
//! `start` spawns a detached `rift server start --foreground` and waits for
//! its published lock document, unless a server is already serving; a server
//! still building gets the same wait with nothing spawned; `stop` asks the
//! recorded server to shut down over its own stop route; `restart` chains
//! the two; `status` prints one probe's
//! classification and changes nothing. Every wait is a bounded poll over
//! [`rift_mcp::probe`] and, when stopping, the original process handle.
//! The election module itself never polls.

use std::fmt;
use std::io;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use rift_error::{ErrorContext, RiftError, errors};
use rift_mcp::{
    PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, ServerPresence,
    SpawnPollOutcome, SpawnedServer, StaleReason, StartSpawns, StartedServer, StopRequestFailure,
    TokenCheck, WorkspaceStorage, probe, read_serving, serve_elected_with_storage,
    spawn_detached_server,
};
use rift_protocol::lock::ServerLock;
use rift_tracing::{
    LOG_PAGE_RECORDS_MAX, LogDrain, LogLines, LogQuery, LogReader, LogReads, RunningLogDrain,
    StoredLogRecord, install_panic_hook,
};
use tokio_util::sync::CancellationToken;
use waitpid_any::WaitHandle;

/// Longest wait for an asked server to leave the serving state.
///
/// The stop poll runs at most `STOP_WAIT_MAX / PRESENCE_POLL_INTERVAL` = 100
/// iterations, and stops at this span of elapsed time when its probes are
/// slower than the interval.
const STOP_WAIT_MAX: Duration = Duration::from_secs(10);
/// Probe attempts one stop waits: `STOP_WAIT_MAX` over the interval.
const STOP_POLL_ATTEMPT_COUNT: u32 = 100;
/// How long the whole server-side stop has, shared by every stage under it.
///
/// The stop derives its deadline where the stop begins, never where the
/// server started listening. Four seconds leave time for command startup and
/// process observation within the five-second stop bound. The span
/// also sits under [`STOP_WAIT_MAX`], so the CLI can observe the process leaving.
/// The serving task drains the requests still in flight, the engines shut
/// down in parallel, the index supervisor joins, and the log drain's final
/// flush runs, each taking only what the stage before it left of that
/// deadline.
const SERVER_STOP_DEADLINE: Duration = Duration::from_secs(4);
/// Time the stop keeps for the metrics database close before the final OTLP export stage.
const SERVER_DATABASE_STOP_RESERVE: Duration = Duration::from_millis(500);
/// Time the stop keeps for the log drain's final flush before metrics database close and
/// OTLP shutdown.
const SERVER_LOG_FLUSH_RESERVE: Duration = Duration::from_millis(500);
/// Time the stop keeps for the final OTLP flush and shutdown after every stop record.
/// A collector that accepts and never answers costs the stop this much and no more,
/// whatever the `OTEL_METRIC_EXPORT_TIMEOUT` and `OTEL_BSP_EXPORT_TIMEOUT` variables are set to.
const SERVER_EXPORT_STOP_RESERVE: Duration = Duration::from_millis(500);
/// Time the stop keeps inside the OTLP reserve to send the final export result through logs.
const SERVER_LOG_EXPORT_STOP_RESERVE: Duration = Duration::from_millis(50);
/// Time the stop keeps inside the OTLP reserve for the trace flush before database close.
const SERVER_TRACE_FLUSH_RESERVE: Duration = Duration::from_millis(50);
/// Time the stop keeps inside the OTLP reserve to shut down spans and metrics after database
/// close and before the final log flush.
const SERVER_TRACE_METRIC_STOP_RESERVE: Duration = SERVER_EXPORT_STOP_RESERVE
    .saturating_sub(SERVER_LOG_EXPORT_STOP_RESERVE)
    .saturating_sub(SERVER_TRACE_FLUSH_RESERVE);
/// Time a workspace server's stop keeps for the stages after serving and the index and
/// vectors close: the log drain's final flush, the metrics database's close, and the OTLP
/// export.
const SERVER_LATER_STAGES_RESERVE: Duration = SERVER_DATABASE_STOP_RESERVE
    .saturating_add(SERVER_LOG_FLUSH_RESERVE)
    .saturating_add(SERVER_EXPORT_STOP_RESERVE);
// The reserves leave the serving stages and the index and vectors close a share of the
// stop's deadline: the deadline less every reserve still lands after the instant the stop
// began, so no subtraction of a reserve from the deadline underflows.
const _: () = assert!(
    SERVER_DATABASE_STOP_RESERVE.as_millis()
        + SERVER_LOG_FLUSH_RESERVE.as_millis()
        + SERVER_EXPORT_STOP_RESERVE.as_millis()
        < SERVER_STOP_DEADLINE.as_millis()
        && SERVER_LOG_EXPORT_STOP_RESERVE.as_millis() > 0
        && SERVER_TRACE_METRIC_STOP_RESERVE.as_millis() > 0
        && SERVER_LOG_EXPORT_STOP_RESERVE.as_millis()
            + SERVER_TRACE_METRIC_STOP_RESERVE.as_millis()
            + SERVER_TRACE_FLUSH_RESERVE.as_millis()
            == SERVER_EXPORT_STOP_RESERVE.as_millis()
);
// Each mode's reserve is a share of the stop's deadline, and a repository server, which runs
// the export and the log drain's final flush and closes no metrics database of its own, keeps
// those two reserves and less than a workspace server.
const _: () = assert!(
    later_stages_reserve(false).as_millis() == SERVER_LATER_STAGES_RESERVE.as_millis()
        && later_stages_reserve(true).as_millis()
            == SERVER_EXPORT_STOP_RESERVE.as_millis() + SERVER_LOG_FLUSH_RESERVE.as_millis()
        && later_stages_reserve(true).as_millis() < later_stages_reserve(false).as_millis()
        && later_stages_reserve(false).as_millis() < SERVER_STOP_DEADLINE.as_millis()
);
// The final log drain, aborted by the stop's deadline at the latest, leaves a five-second
// stop request time to observe the process exit.
const _: () =
    assert!(SERVER_STOP_DEADLINE.as_millis() < rift_mcp::STOP_REQUEST_TIMEOUT.as_millis());
/// Wall-clock span between two polls of the store while following.
const LOG_FOLLOW_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The form `--tail` accepts, named in every refusal.
const TAIL_COUNT_EXPECTED: &str = "`all`, or a positive integer such as `20`";
/// What a workspace holding no recorded diagnostics prints on stderr.
const NO_RECORDED_LOGS: &str = "💤 no server diagnostics recorded for this workspace yet; \
                                start one with `rift server start`";
fn stop_timeout(process: ProcessExit, holder: &ServerLock) -> Result<(), RiftError> {
    let mut builder = errors::cli::server_stop_timed_out()
        .waited(STOP_WAIT_MAX)
        .listening(format!("127.0.0.1:{}", holder.port))
        .pid(holder.pid);
    if let Some(detail) = process.refusal_detail() {
        builder = builder.with(ErrorContext::new("detail", detail));
    }
    builder.fail()
}

fn election_unreleased(process: ProcessExit, pid: u32) -> Result<(), RiftError> {
    let mut builder = errors::cli::server_election_unreleased()
        .pid(pid)
        .waited(STOP_WAIT_MAX);
    if let Some(detail) = process.refusal_detail() {
        builder = builder.with(ErrorContext::new("detail", detail));
    }
    builder.fail()
}

/// `rift server` subcommands.
#[derive(Debug, clap::Subcommand)]
pub(super) enum ServerCommand {
    /// Start this workspace's server, detached unless --foreground.
    Start {
        /// Serve in this process instead of spawning a detached one.
        #[arg(long)]
        foreground: bool,
        /// Serve repository workspaces through one process. Requires `--foreground`.
        #[arg(long, hide = true, requires = "foreground")]
        repository: bool,
        /// How served requests authenticate; `skip` needs --foreground.
        #[arg(
            long,
            value_enum,
            default_value_t = AuthMode::Token,
            value_name = "MODE",
            requires_if("skip", "foreground")
        )]
        auth: AuthMode,
    },
    /// Stop this workspace's server.
    Stop {
        /// Stop repository server selected by this workspace.
        #[arg(long, hide = true)]
        repository: bool,
    },
    /// Stop this workspace's server, then start a fresh detached one.
    Restart,
    /// Report whether this workspace's server is serving.
    Status {
        /// Report repository server selected by this workspace.
        #[arg(long, hide = true)]
        repository: bool,
    },
    /// Print this workspace's recorded server diagnostics, oldest first.
    Logs {
        /// Keep printing records as the server writes them.
        #[arg(short, long)]
        follow: bool,
        /// Print only the newest COUNT records; `all` prints every kept record.
        #[arg(short = 'n', long, default_value = "all", value_name = "COUNT")]
        tail: TailCount,
        /// Print only records recorded at or after WHEN: an age such as `10m` or `2h`,
        /// or an RFC 3339 timestamp such as `2026-10-04T20:42:58Z`.
        #[arg(long, value_name = "WHEN", value_parser = LogsBound::parse)]
        since: Option<LogsBound>,
        /// Print only records recorded before WHEN, in the forms `--since` takes.
        #[arg(long, value_name = "WHEN", value_parser = LogsBound::parse)]
        until: Option<LogsBound>,
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

/// One bound of the window a logs read selects: an age before the read starts, or one
/// instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LogsBound {
    /// This long before the read starts.
    Age(rift_protocol::configuration::Duration),
    /// One instant, given in RFC 3339.
    At(jiff::Timestamp),
}

impl LogsBound {
    /// Reads an age in the configuration's duration spelling, or else an RFC 3339
    /// timestamp; the refusal names both forms.
    fn parse(text: &str) -> Result<Self, String> {
        if let Ok(age) = rift_protocol::configuration::Duration::parse(text) {
            return Ok(Self::Age(age));
        }
        text.parse::<jiff::Timestamp>().map(Self::At).map_err(|_| {
            format!(
                "expected an age such as `10m` or an RFC 3339 timestamp such as \
                 `2026-10-04T20:42:58Z`, not {text:?}"
            )
        })
    }

    /// `query` restricted to records recorded at or after this bound: an age through
    /// [`LogQuery::since_age`], on the tracing clock.
    fn since(self, query: LogQuery) -> LogQuery {
        match self {
            Self::Age(age) => query.since_age(Duration::from_millis(age.milliseconds())),
            Self::At(instant) => query.since_ms(instant.as_millisecond()),
        }
    }

    /// `query` restricted to records recorded before this bound: an age through
    /// [`LogQuery::until_age`], on the tracing clock.
    fn until(self, query: LogQuery) -> LogQuery {
        match self {
            Self::Age(age) => query.until_age(Duration::from_millis(age.milliseconds())),
            Self::At(instant) => query.until_ms(instant.as_millisecond()),
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

/// How a started server authenticates the requests it serves.
///
/// `Skip` exists for the MCP conformance runner, which addresses a server
/// by URL alone and sends no `Authorization` header. It is accepted only
/// beside `--foreground`, so a detached server always checks its token.
/// The variants carry no documentation of their own: clap renders a value's
/// doc comment as per-value help, which turns the whole command's help into
/// its long form.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum AuthMode {
    #[default]
    Token,
    Skip,
}

/// The clap `--auth` value as the policy the transport applies.
fn token_check(auth: AuthMode) -> TokenCheck {
    match auth {
        AuthMode::Token => TokenCheck::Required,
        AuthMode::Skip => TokenCheck::Skipped,
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
    /// A server holds the election and is still building its index. Carries
    /// the pid when this command spawned it; a server another command
    /// started has not published its pid yet.
    Starting { pid: Option<u32> },
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
    Stale { reason: String },
}

/// Result and elapsed time for one provider shutdown phase.
#[derive(Debug)]
struct ExportPhaseReport {
    result: Result<(), rift_tracing::ExportShutdownError>,
    elapsed: Duration,
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
            Self::Starting { pid: Some(pid) } => write!(
                formatter,
                "⏳ rift server starting (pid {pid}): it is indexing the workspace, and \
                 `rift server status` shows it listening once the index is built"
            ),
            Self::Starting { pid: None } => formatter.write_str(
                "⏳ rift server starting: it is indexing the workspace, and `rift server status` \
                 shows it listening once the index is built",
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
/// Returns [`RiftError`] when the asked state was not reached.
pub(super) async fn run(
    command: ServerCommand,
    drain: Option<LogDrain>,
    retention_records: u64,
    export: rift_tracing::OtlpExport,
) -> Result<Option<ServerOutcome>, RiftError> {
    let root = Path::new(".");
    match command {
        ServerCommand::Start {
            foreground,
            repository,
            auth,
        } => match start_mode(foreground) {
            StartMode::Detached => start_detached(root, STOP_POLL_ATTEMPT_COUNT)
                .await
                .map(Some),
            StartMode::Foreground => serve_foreground(
                root,
                drain,
                retention_records,
                token_check(auth),
                repository,
                export,
            )
            .await
            .map(|()| None),
        },
        ServerCommand::Stop { repository } => {
            if repository {
                stop_repository(root, STOP_POLL_ATTEMPT_COUNT)
                    .await
                    .map(Some)
            } else {
                stop(root, STOP_POLL_ATTEMPT_COUNT).await.map(Some)
            }
        }
        ServerCommand::Restart => restart(root).await.map(Some),
        ServerCommand::Status { repository } => Ok(Some(if repository {
            repository_status(root)
        } else {
            status(root)
        })),
        ServerCommand::Logs {
            follow,
            tail,
            since,
            until,
            level,
            component,
        } => {
            let window = LogsWindow { since, until };
            let query = logs_query(tail, window, level, component.as_deref());
            print_logs(root, &query, tail, &logs_mode(follow))
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
        ServerPresence::Starting => ServerOutcome::Starting { pid: None },
        ServerPresence::Stale(reason) => ServerOutcome::Stale {
            reason: stale_reason_phrase(&reason),
        },
        ServerPresence::Absent => ServerOutcome::NotRunning,
    }
}

fn repository_state_directory(root: &Path) -> Option<std::path::PathBuf> {
    let common_directory = rift_mcp::repository::discover_common_directory(root)?;
    let executable = std::env::current_exe().ok()?;
    let identity = rift_mcp::product_identity_of(crate::BUILD_CHECKOUT, &executable).ok()?;
    rift_mcp::repository::repository_election_directory(&common_directory, &identity).ok()
}

fn repository_status(root: &Path) -> ServerOutcome {
    let Some(state_directory) = repository_state_directory(root) else {
        return ServerOutcome::NotRunning;
    };
    match rift_mcp::probe_state_directory(&state_directory) {
        ServerPresence::Serving(lock) => ServerOutcome::Serving {
            port: lock.port,
            pid: lock.pid,
            version: lock.identity.version,
        },
        ServerPresence::Starting => ServerOutcome::Starting { pid: None },
        ServerPresence::Stale(reason) => ServerOutcome::Stale {
            reason: stale_reason_phrase(&reason),
        },
        ServerPresence::Absent => ServerOutcome::NotRunning,
    }
}

async fn stop_repository(root: &Path, attempt_count: u32) -> Result<ServerOutcome, RiftError> {
    let Some(state_directory) = repository_state_directory(root) else {
        return Ok(ServerOutcome::NotRunning);
    };
    let lock = match rift_mcp::probe_state_directory(&state_directory) {
        ServerPresence::Serving(lock) => lock,
        ServerPresence::Starting => return Ok(ServerOutcome::Starting { pid: None }),
        ServerPresence::Stale(StaleReason::PortUnreachable { pid }) => {
            await_repository_election_released(&state_directory, pid, attempt_count).await?;
            return Ok(ServerOutcome::Stopped);
        }
        ServerPresence::Stale(_) | ServerPresence::Absent => return Ok(ServerOutcome::NotRunning),
    };
    let process = ProcessExit::open(lock.pid);
    request_stop(&lock).await?;
    await_repository_stopped(&state_directory, lock, process, attempt_count).await?;
    Ok(ServerOutcome::Stopped)
}

async fn await_repository_election_released(
    state_directory: &Path,
    pid: u32,
    attempt_count: u32,
) -> Result<(), RiftError> {
    let deadline = tokio::time::Instant::now() + poll_window(attempt_count);
    for _ in 0..attempt_count {
        if !rift_mcp::probe_state_directory(state_directory).election_held() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    errors::cli::server_election_unreleased()
        .pid(pid)
        .waited(STOP_WAIT_MAX)
        .fail()
}

async fn await_repository_stopped(
    _state_directory: &Path,
    holder: ServerLock,
    mut process: ProcessExit,
    attempt_count: u32,
) -> Result<(), RiftError> {
    let deadline = tokio::time::Instant::now() + poll_window(attempt_count);
    for _ in 0..attempt_count {
        if process.exited() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    stop_timeout(process, &holder)
}

/// The probe's stale classification as one operator-facing phrase.
fn stale_reason_phrase(reason: &StaleReason) -> String {
    match reason {
        StaleReason::DocumentUnreadable => "the document could not be read".to_owned(),
        StaleReason::DocumentMalformed => "the document is malformed".to_owned(),
        StaleReason::DocumentInvalid(_) => "the document breaks the lock contract".to_owned(),
        StaleReason::ElectionUnheld => "no process holds the election lock".to_owned(),
        StaleReason::ElectionUnobservable => {
            "the election lock state could not be observed".to_owned()
        }
        StaleReason::PortUnreachable { pid } => {
            format!("the server at pid {pid} no longer answers its port and is shutting down")
        }
    }
}

/// Starts a detached server unless one already serves or is starting, and
/// waits for it.
///
/// Repeats are idempotent: an already-serving workspace answers with the
/// running server's address and starts nothing. A workspace whose server
/// another start elected and is still building spawns nothing either: the
/// start waits for that server's document within the window a spawning start
/// waits, answers with its address once it serves, and answers that it is
/// starting when the window runs out first. Concurrent starts therefore name
/// the same server, whichever of them spawned it. A server that
/// stopped answering its port is on its way out: the start waits for it to
/// release the election, bounded by `stop_attempt_count` probes and the
/// [`poll_window`] they span, before electing a fresh one. The commands pass
/// [`STOP_POLL_ATTEMPT_COUNT`], the probes [`STOP_WAIT_MAX`] spans.
async fn start_detached(root: &Path, stop_attempt_count: u32) -> Result<ServerOutcome, RiftError> {
    let holder_building = match probe(root) {
        ServerPresence::Serving(lock) => {
            return Ok(ServerOutcome::AlreadyListening {
                port: lock.port,
                pid: lock.pid,
            });
        }
        // A spawn under a held election only loses it.
        ServerPresence::Starting => true,
        ServerPresence::Stale(StaleReason::PortUnreachable { pid }) => {
            await_election_released(root, pid, stop_attempt_count).await?;
            false
        }
        ServerPresence::Stale(_) | ServerPresence::Absent => false,
    };
    // A stale document keeps its bytes until the elected child scrubs it.
    // Remember them so the wait below never answers with the old document
    // read in the instant the child already holds the election.
    let stale_bytes = std::fs::read(rift_mcp::document_path(root)).ok();
    let mut spawns = StartSpawns::default();
    if !holder_building {
        spawns
            .spawn(|| spawn_detached_server(root))
            .map_err(|source| {
                errors::cli::server_spawn_failed()
                    .operation("spawn detached server")
                    .source(source)
                    .error()
            })?;
    }
    await_serving(
        root,
        START_POLL_ATTEMPT_COUNT,
        stale_bytes.as_deref(),
        &mut spawns,
        || spawn_detached_server(root),
    )
    .await
}

/// The refusal for a detached server that could not be spawned.
/// The child a start watches beside the published document.
///
/// [`SpawnedServer`] is the one implementation the CLI runs; a test double
/// stands in for it so the wait's decisions are provable without a process.
trait ChildWatch {
    /// The child's process id.
    fn pid(&self) -> u32;
    /// Whether the child has not exited yet.
    fn is_running(&mut self) -> bool;
}

impl ChildWatch for SpawnedServer {
    fn pid(&self) -> u32 {
        Self::pid(self)
    }

    fn is_running(&mut self) -> bool {
        Self::is_running(self)
    }
}

/// Polls until the workspace serves, bounded by `attempt_count` probes and by
/// the [`poll_window`] they span.
///
/// The caller passes [`START_POLL_ATTEMPT_COUNT`], which derives from
/// [`START_WAIT_MAX`] over the poll interval. A poll that finds the document
/// still byte-equal to `stale_bytes` - the pre-spawn leftover - keeps
/// waiting: the started server always publishes a fresh document (its own
/// pid, token, and port), so the leftover can only mean the child has not
/// published yet.
///
/// `spawns` holds the server this command spawned, when it spawned one, and
/// [`StartSpawns::poll`] weighs its exit against the election each probe
/// finds. A child that lost the election while no process holds it is
/// replaced through `launch`, bounded by
/// [`START_SPAWN_COUNT_MAX`](rift_mcp::START_SPAWN_COUNT_MAX); a child
/// that exited for another reason while no process holds the election ends
/// the wait at once, since nothing is left to publish. A wait that runs out
/// while the child is still running, or while another process holds the
/// election, is not a failure: the server is indexing, and the outcome says
/// so.
async fn await_serving<Spawned>(
    root: &Path,
    attempt_count: u32,
    stale_bytes: Option<&[u8]>,
    spawns: &mut StartSpawns<Spawned>,
    launch: impl FnMut() -> io::Result<Spawned>,
) -> Result<ServerOutcome, RiftError>
where
    Spawned: ChildWatch + StartedServer<Failure = u32>,
{
    await_serving_with_probe(root, attempt_count, stale_bytes, spawns, launch, |root| {
        std::future::ready(probe(root))
    })
    .await
}

async fn await_serving_with_probe<Spawned, Observation>(
    root: &Path,
    attempt_count: u32,
    stale_bytes: Option<&[u8]>,
    spawns: &mut StartSpawns<Spawned>,
    mut launch: impl FnMut() -> io::Result<Spawned>,
    mut observe: impl FnMut(&Path) -> Observation,
) -> Result<ServerOutcome, RiftError>
where
    Spawned: ChildWatch + StartedServer<Failure = u32>,
    Observation: std::future::Future<Output = ServerPresence>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + poll_window(attempt_count);
    let mut probe_count = 0;
    for _ in 0..attempt_count {
        probe_count += 1;
        let presence = observe(root).await;
        let election_held = presence.election_held();
        let serving = match presence {
            ServerPresence::Serving(lock) if !leftover_unscrubbed(root, stale_bytes) => Some(lock),
            _ => None,
        };
        match spawns.poll(serving, election_held) {
            SpawnPollOutcome::Ready(lock) => {
                return Ok(ServerOutcome::Listening {
                    port: lock.port,
                    pid: lock.pid,
                });
            }
            SpawnPollOutcome::Failed(pid) => {
                return errors::cli::server_start_exited().pid(pid).fail();
            }
            SpawnPollOutcome::ElectionUnheld => spawns.spawn(&mut launch).map_err(|source| {
                errors::cli::server_spawn_failed()
                    .operation("spawn detached server")
                    .source(source)
                    .error()
            })?,
            SpawnPollOutcome::Waiting => {}
        }
        let observed = tokio::time::Instant::now();
        let deadline_reached = observed >= deadline;
        rift_tracing::debug!(
            component = "cli",
            operation = "server.start",
            probe_count,
            attempt_count,
            waited = ?(observed - started),
            window = ?poll_window(attempt_count),
            ?observed,
            ?deadline,
            deadline_reached,
            "server wait probe completed"
        );
        if deadline_reached {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    if let Some(child) = spawns.running()
        && child.is_running()
    {
        return Ok(ServerOutcome::Starting {
            pid: Some(child.pid()),
        });
    }
    // A holder that has not published is starting, whether the document is
    // absent or still the pre-spawn leftover it has yet to scrub.
    rift_tracing::debug!(
        component = "cli",
        operation = "server.start",
        probe_count = probe_count + 1,
        waited = ?started.elapsed(),
        ?deadline,
        closing = true,
        "server start closing probe"
    );
    let presence = observe(root).await;
    let holder_unpublished = matches!(presence, ServerPresence::Starting)
        || (presence.election_held() && leftover_unscrubbed(root, stale_bytes));
    if holder_unpublished {
        return Ok(ServerOutcome::Starting { pid: None });
    }
    errors::cli::server_start_timed_out()
        .waited(START_WAIT_MAX)
        .fail()
}

/// The elapsed time a poll of `attempt_count` probes at [`PRESENCE_POLL_INTERVAL`]
/// may take: [`STOP_WAIT_MAX`] for [`STOP_POLL_ATTEMPT_COUNT`].
///
/// A poll counts its probes and also stops once this span has passed since it
/// began. A probe is not instant: one that meets a port nothing accepts on
/// spends its whole connect timeout, which on Windows is every refused port,
/// because Winsock retries a refused connect instead of failing it. Counting
/// alone, 100 such probes would run for about a minute against a ten-second
/// bound; the span keeps the bound, and slow probes shorten the count.
fn poll_window(attempt_count: u32) -> Duration {
    PRESENCE_POLL_INTERVAL.saturating_mul(attempt_count)
}

/// Whether the document on disk is still byte-equal to the pre-spawn
/// leftover, so a read of it would answer with the previous holder's facts.
fn leftover_unscrubbed(root: &Path, stale_bytes: Option<&[u8]>) -> bool {
    match (stale_bytes, std::fs::read(rift_mcp::document_path(root))) {
        (Some(stale), Ok(current)) => stale == current.as_slice(),
        _ => false,
    }
}

/// Polls until the named process exits or releases the election, bounded by
/// `attempt_count` probes and by the [`poll_window`] they span. A replacement
/// still building must not extend this wait.
///
/// The caller passes [`STOP_POLL_ATTEMPT_COUNT`], which derives from
/// [`STOP_WAIT_MAX`] over the poll interval; `pid` names the holder in the
/// refusal when it keeps the election past that.
async fn await_election_released(
    root: &Path,
    pid: u32,
    attempt_count: u32,
) -> Result<(), RiftError> {
    await_election_released_with_probe(root, pid, attempt_count, |root| {
        std::future::ready(probe(root))
    })
    .await
}

async fn await_election_released_with_probe<Observation>(
    root: &Path,
    pid: u32,
    attempt_count: u32,
    mut observe: impl FnMut(&Path) -> Observation,
) -> Result<(), RiftError>
where
    Observation: std::future::Future<Output = ServerPresence>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + poll_window(attempt_count);
    let mut process = ProcessExit::open(pid);
    for probe_index in 0..attempt_count {
        if process.exited() || !observe(root).await.election_held() {
            return Ok(());
        }
        let observed = tokio::time::Instant::now();
        let deadline_reached = observed >= deadline;
        rift_tracing::debug!(
            component = "cli",
            operation = "server.election",
            probe_count = probe_index + 1,
            attempt_count,
            waited = ?(observed - started),
            window = ?poll_window(attempt_count),
            ?observed,
            ?deadline,
            deadline_reached,
            "server wait probe completed"
        );
        if deadline_reached {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    election_unreleased(process, pid)
}

/// Exit of the process named before the stop request. The handle stays bound to that
/// process when another server takes the election or the OS reuses its process id.
#[derive(Debug)]
enum ProcessExit {
    Waiting {
        handle: WaitHandle,
        last_error: Option<io::Error>,
    },
    Exited,
    Unknown(io::Error),
}

impl ProcessExit {
    fn open(pid: u32) -> Self {
        let Ok(pid) = i32::try_from(pid) else {
            return Self::Unknown(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id exceeds the process handle bound",
            ));
        };
        if pid == 0 {
            return Self::Unknown(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process id must be positive",
            ));
        }
        Self::observed(WaitHandle::open(pid))
    }

    fn observed(result: io::Result<WaitHandle>) -> Self {
        match result {
            Ok(handle) => Self::Waiting {
                handle,
                last_error: None,
            },
            Err(error) if process_absent(&error) => Self::Exited,
            Err(error) => Self::Unknown(error),
        }
    }

    fn exited(&mut self) -> bool {
        match self {
            Self::Exited => true,
            Self::Unknown(_) => false,
            Self::Waiting { handle, last_error } => match handle.wait_timeout(Duration::ZERO) {
                Ok(Some(())) => {
                    *self = Self::Exited;
                    true
                }
                Ok(None) => {
                    *last_error = None;
                    false
                }
                Err(error) => {
                    *last_error = Some(error);
                    false
                }
            },
        }
    }

    fn refusal_detail(self) -> Option<String> {
        match self {
            Self::Unknown(source)
            | Self::Waiting {
                last_error: Some(source),
                ..
            } => Some(format!("process exit could not be observed: {source}")),
            Self::Exited
            | Self::Waiting {
                last_error: None, ..
            } => None,
        }
    }
}

#[cfg(unix)]
fn process_absent(error: &io::Error) -> bool {
    error.raw_os_error().map(nix::errno::Errno::from_raw) == Some(nix::errno::Errno::ESRCH)
}

#[cfg(windows)]
fn process_absent(error: &io::Error) -> bool {
    // OpenProcess uses ERROR_INVALID_PARAMETER for a process id that no longer exists.
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        == Some(windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER)
}

/// Serves the workspace in this process until interrupted or stopped,
/// under `check`.
///
/// The listening line prints before blocking. Ctrl-C, and SIGTERM on unix,
/// cancel the shutdown token; an authorized stop request and the idle timeout
/// end serving the same way. This is the process that records: the drain
/// writes what the tracing layer queued into the metrics database until the
/// same token stops it.
///
/// The stop runs in one order under [`SERVER_STOP_DEADLINE`]: serving ends and the engines
/// and index supervisor shut down by the later-stage reserves, leaving those reserves when
/// a stage reaches its bound. The index and vectors databases close before the log drain's
/// final flush. The metrics database closes before OTLP shutdown. The export stage flushes
/// traces, closes its stage span, shuts down traces and metrics, records each result through
/// logs, then shuts down logs. Export failures do not fail the stop. The server releases
/// the election only after shutdown, by
/// dropping the guard right before the process exits - so a stop the CLI
/// reports as success means the process is leaving. The deadline starts where
/// the stop begins, and each stage takes only what the one before it left of
/// it. Each stage records its name, what it left of the deadline, and its
/// error, and a failed stop leaves with the rendered error on stderr.
///
/// A repository server's drain routes each record to the consumer of the workspace it
/// names. Each workspace stops inside the serving stage `repository workspaces shutdown`:
/// its index and vectors databases close, its consumer flushes, and its metrics database
/// closes. After serving, the server stops the routing drain and OTLP export. It has no
/// metrics database of its own, so its later stages keep [`SERVER_EXPORT_STOP_RESERVE`]
/// and [`SERVER_LOG_FLUSH_RESERVE`] before the deadline ([`later_stages_reserve`]).
///
/// A database close runs no checkpoint and syncs no file, so no pending flush
/// holds the process exit; the next open's checkpoint moves the write-ahead log.
/// A close whose thread outlasts its bound ends its stage with the outcome
/// `timeout` and fails nothing: the thread keeps running until it finishes or
/// the process exits. A metrics close ends `timeout` whatever stage the bound
/// passed in, a close still queued behind an earlier command included. An index
/// supervisor still running at its bound is aborted the same way: its stage
/// ends `timeout` with the table of operations in flight and fails nothing, and
/// blocking work it started ends with the process.
async fn serve_foreground(
    root: &Path,
    drain: Option<LogDrain>,
    retention_records: u64,
    check: TokenCheck,
    repository: bool,
    export: rift_tracing::OtlpExport,
) -> Result<(), RiftError> {
    // A detached server's panic reaches its stderr file at best; the hook
    // records it through the same lane every other diagnostic takes.
    install_panic_hook();
    let shutdown = CancellationToken::new();
    let selection = foreground_selection(root, repository)?;
    let (storage, guard) = if repository {
        (None, None)
    } else {
        let guard = std::sync::Arc::new(
            rift_mcp::claim(root).map_err(|error| foreground_refused(root, error))?,
        );
        let storage = WorkspaceStorage::open_elected(root, std::sync::Arc::clone(&guard))
            .await
            .map_err(|error| foreground_refused(root, error))?;
        (Some(storage), Some(guard))
    };
    let log_drain = start_log_drain(drain, storage.as_ref(), retention_records);
    let serving = if let Some(rift_mcp::repository::ServerConfigurationSelection::Repository {
        authority_root,
        common_directory,
        server,
        ..
    }) = selection
    {
        rift_mcp::serve_repository_elected(
            &authority_root,
            &common_directory,
            server,
            shutdown.clone(),
            rift_index::WorkspaceIndexLimits::default(),
            check,
            crate::BUILD_CHECKOUT,
        )
        .await
    } else {
        let storage = storage.ok_or_else(|| {
            errors::mcp::election_storage_failed()
                .operation("select repository server")
                .path(root)
                .source(io::Error::other("workspace storage is unavailable"))
                .error()
        })?;
        let guard = guard.ok_or_else(|| {
            errors::mcp::election_storage_failed()
                .operation("select repository server")
                .path(root)
                .source(io::Error::other("workspace election is unavailable"))
                .error()
        })?;
        serve_elected_with_storage(
            root,
            guard,
            shutdown.clone(),
            storage,
            check,
            crate::BUILD_CHECKOUT,
        )
        .await
    };
    let server = match serving {
        Ok(server) => server,
        Err(error) => {
            shutdown.cancel();
            stop_log_drain(
                log_drain,
                tokio::time::Instant::now() + SERVER_STOP_DEADLINE,
            )
            .await;
            return foreground_refused(root, error).fail();
        }
    };
    let stop_signals = cancel_on_stop_signal(shutdown.clone());
    println!(
        "{}",
        ServerOutcome::Listening {
            port: server.port(),
            pid: std::process::id(),
        }
    );
    // Serving ends by the reserves of the later stages this mode runs, so they keep theirs.
    let (guard, deadline, stopped, mut database) = server
        .stopped_before_database(SERVER_STOP_DEADLINE, later_stages_reserve(repository))
        .await;
    shutdown.cancel();
    stop_signals.abort();
    let _ = stop_signals.await;
    let flush_deadline = deadline - log_flush_end_reserve(repository);
    let search = database
        .close_search(deadline - later_stages_reserve(repository))
        .await;
    stop_log_drain(log_drain, flush_deadline).await;
    let logs = database
        .close_logs(deadline - SERVER_EXPORT_STOP_RESERVE)
        .await;
    // Export failures do not fail a stop. The phase outcomes are recorded before the log
    // provider closes; that provider cannot export its own shutdown result.
    let _ = stop_export(&export, deadline).await;
    retire_before_exit(guard);
    stopped.and(search).and(logs)
}

/// Starts the log drain of a foreground server: writing into the metrics database of the
/// workspace `storage` opened, or, for a repository server, which opens no `storage`,
/// routing each record to the consumer of the workspace it names.
///
/// A workspace whose metrics database did not open records nothing, and neither does a
/// process that built no `drain`.
fn start_log_drain(
    drain: Option<LogDrain>,
    storage: Option<&WorkspaceStorage>,
    retention_records: u64,
) -> Option<RunningLogDrain> {
    let drain = drain?;
    match storage {
        Some(storage) => storage
            .logs()
            .map(|store| RunningLogDrain::spawn(drain, store, retention_records)),
        None => Some(RunningLogDrain::spawn_routed(drain, retention_records)),
    }
}

/// Retires `server.json` and drops this stop's election guard, immediately before the
/// process exits.
///
/// Each database thread holds a clone of the guard until it exits, so a close that
/// ended `timeout` leaves a thread still holding it: the election stays held until the
/// process exits, and the operating system releases the lock then. The document is
/// retired here either way, because the process no longer serves; dropping the last
/// clone retires it again, which finds it gone.
fn retire_before_exit(guard: std::sync::Arc<rift_mcp::ElectionGuard>) {
    guard.retire();
    if std::sync::Arc::strong_count(&guard) > 1 {
        rift_tracing::warn!(
            component = "mcp",
            operation = "server.stop",
            "a database thread still holds the workspace election; it is released when the \
             process exits"
        );
    }
    drop(guard);
}

fn foreground_selection(
    root: &Path,
    repository: bool,
) -> Result<Option<rift_mcp::repository::ServerConfigurationSelection>, RiftError> {
    if !repository {
        return Ok(None);
    }
    let selected =
        rift_mcp::repository::select_server_configuration(root, None).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("select repository server")
                .path(root)
                .source(source)
                .error()
        })?;
    if !matches!(
        selected,
        rift_mcp::repository::ServerConfigurationSelection::Repository { .. }
    ) {
        return errors::mcp::election_storage_failed()
            .operation("select repository server")
            .path(root)
            .source(io::Error::other("repository settings are unavailable"))
            .fail();
    }
    Ok(Some(selected))
}

/// Time the stop keeps after serving for the later stages a server in this mode runs.
///
/// A workspace server (`repository` false) runs the log drain's final flush, the metrics
/// database's close, and the OTLP export, and keeps [`SERVER_LATER_STAGES_RESERVE`]. A
/// repository server opens no workspace storage of its own: its index, vectors, and
/// metrics databases and its workspace log consumers belong to its workspaces and stop
/// inside the serving stage `repository workspaces shutdown`, so its later stages are the
/// stop of its routing log drain and the OTLP export, and it keeps
/// [`SERVER_EXPORT_STOP_RESERVE`] and [`SERVER_LOG_FLUSH_RESERVE`].
const fn later_stages_reserve(repository: bool) -> Duration {
    if repository {
        SERVER_EXPORT_STOP_RESERVE.saturating_add(SERVER_LOG_FLUSH_RESERVE)
    } else {
        SERVER_LATER_STAGES_RESERVE
    }
}

/// How long before the stop's deadline the `log drain` stage ends in this mode: metrics
/// database close and OTLP shutdown on a workspace server, or OTLP shutdown on a
/// repository server.
const fn log_flush_end_reserve(repository: bool) -> Duration {
    SERVER_EXPORT_STOP_RESERVE.saturating_add(if repository {
        Duration::ZERO
    } else {
        SERVER_DATABASE_STOP_RESERVE
    })
}

/// Sends the OTLP export's final spans and metric points and shuts it down by `deadline`,
/// or [`SERVER_EXPORT_STOP_RESERVE`] after the stage starts when that comes first, as the
/// `otlp export` stop stage: a collector that is down or stalled costs the stop that
/// reserve at most, even when the stages before left more.
///
/// The stage records `outcome="ok"` when the export shut down inside `deadline`,
/// `outcome="timeout"` at `warn` when the deadline passed first, as with a collector that
/// accepts and never answers, and `outcome="error"` at `warn` with the SDK's words when the
/// final export failed, as with a refused connection. None fails the stop: the export
/// carries diagnostics only. A process that exports nothing records `ok` at once.
async fn stop_export(
    export: &rift_tracing::OtlpExport,
    deadline: tokio::time::Instant,
) -> Result<(), rift_tracing::ExportShutdownError> {
    let bound = deadline.min(tokio::time::Instant::now() + SERVER_EXPORT_STOP_RESERVE);
    let provider_bound = bound - SERVER_LOG_EXPORT_STOP_RESERVE;
    let flush_started = tokio::time::Instant::now();
    let trace_flush_bound = provider_bound.min(flush_started + SERVER_TRACE_FLUSH_RESERVE);
    let mut flush = None;
    let stage_result =
        rift_mcp::stop_stage_within("otlp export", deadline, trace_flush_bound, async {
            let result = export.flush_traces(trace_flush_bound).await;
            let stage_result = match &result {
                Ok(()) | Err(rift_tracing::ExportShutdownError::TimedOut) => Ok(()),
                Err(failed @ rift_tracing::ExportShutdownError::Failed(_)) => {
                    errors::mcp::http_serve_failed()
                        .operation("otlp export")
                        .source(io::Error::other(failed.to_string()))
                        .fail()
                }
            };
            flush = Some(result);
            stage_result
        })
        .await;
    let flush = flush.unwrap_or_else(|| match stage_result {
        Err(error) => Err(rift_tracing::ExportShutdownError::Failed(error.to_string())),
        Ok(()) => Err(rift_tracing::ExportShutdownError::Failed(
            "the trace flush returned no result".to_owned(),
        )),
    });
    let flush_report = ExportPhaseReport {
        result: flush,
        elapsed: flush_started.elapsed(),
    };
    record_export_result("traces", &flush_report, trace_flush_bound);

    let started = tokio::time::Instant::now();
    let traces_and_metrics = export.shutdown_traces_and_metrics(provider_bound).await;
    let traces_and_metrics_report = ExportPhaseReport {
        result: traces_and_metrics,
        elapsed: started.elapsed(),
    };
    record_export_result(
        "traces and metrics",
        &traces_and_metrics_report,
        provider_bound,
    );

    export.shutdown_logs(bound).await
}

fn record_export_result(
    phase: &'static str,
    report: &ExportPhaseReport,
    bound: tokio::time::Instant,
) {
    let elapsed_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX);
    let remaining = bound.saturating_duration_since(tokio::time::Instant::now());
    match &report.result {
        Ok(()) => rift_tracing::info!(
            component = "mcp",
            operation = "server.stop",
            stage = "otlp export",
            phase,
            outcome = "ok",
            elapsed_ms,
            ?remaining,
            "stop stage ended"
        ),
        Err(rift_tracing::ExportShutdownError::TimedOut) => rift_tracing::warn!(
            component = "mcp",
            operation = "server.stop",
            stage = "otlp export",
            phase,
            outcome = "timeout",
            elapsed_ms,
            ?remaining,
            "stop stage ended"
        ),
        Err(failed @ rift_tracing::ExportShutdownError::Failed(_)) => rift_tracing::warn!(
            component = "mcp",
            operation = "server.stop",
            stage = "otlp export",
            phase,
            outcome = "error",
            elapsed_ms,
            ?remaining,
            %failed,
            "stop stage ended"
        ),
    }
}

/// Stops the diagnostics drain and joins it by `deadline`, as the `log drain` stop stage;
/// answers the records left unwritten when the drain had to be aborted.
///
/// [`RunningLogDrain::stop`] flushes what the drain holds within what the stages before
/// it left of `deadline`, and aborts a drain the metrics database holds past it. The stage
/// records its elapsed time beside the other stages.
async fn stop_log_drain(
    drain: Option<RunningLogDrain>,
    deadline: tokio::time::Instant,
) -> Option<u64> {
    let drain = drain?;
    rift_mcp::stop_stage("log drain", deadline, async {
        Ok(drain.stop(deadline).await)
    })
    .await
    .ok()
    .flatten()
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
        Err(error) => rift_tracing::warn!(component = "cli", %error, "interrupt listener failed"),
    }
}

/// Cancels `shutdown` when the foreground server receives Ctrl-C or SIGTERM.
///
/// `kill` sends SIGTERM by default, and the signal's default action ends the process at
/// once: no request drains, the log drain never flushes, `server.json` stays behind, and
/// the OTLP export never flushes. Handled like Ctrl-C, it runs the same stop under
/// [`SERVER_STOP_DEADLINE`]. Both handlers install when this is called, not when the
/// returned task first runs, so a signal sent once the listening line appears always
/// reaches them. Tokio keeps an installed handler for the rest of the process, so a second
/// signal during the stop does not cut it short; the stop's own deadline bounds it.
///
/// # Cancel safety
///
/// Aborting the returned task stops listening; the handlers stay installed.
#[cfg(unix)]
fn cancel_on_stop_signal(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let installed = signal(SignalKind::interrupt()).and_then(|interrupt| {
        signal(SignalKind::terminate()).map(|terminate| (interrupt, terminate))
    });
    tokio::spawn(async move {
        let (mut interrupt, mut terminate) = match installed {
            Ok(signals) => signals,
            Err(error) => {
                rift_tracing::warn!(component = "cli", %error, "interrupt listener failed");
                return;
            }
        };
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        shutdown.cancel();
    })
}

/// Cancels `shutdown` when the foreground server receives Ctrl-C.
///
/// Windows has no SIGTERM, so Ctrl-C alone ends serving there.
///
/// # Cancel safety
///
/// Aborting the returned task stops listening for the interrupt.
#[cfg(not(unix))]
fn cancel_on_stop_signal(shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(cancel_on_interrupt(shutdown))
}

/// Attaches the holder's address facts to a foreground refusal.
fn foreground_refused(root: &Path, error: RiftError) -> RiftError {
    if error.slug() == errors::mcp::election_already_serving::SLUG {
        let (listening, pid, detail) = match read_serving(root) {
            Some(lock) => (
                Some(format!("127.0.0.1:{}", lock.port)),
                Some(lock.pid),
                None,
            ),
            None => (
                None,
                None,
                Some("the holding server has not published its lock document yet"),
            ),
        };
        return errors::cli::server_already_serving()
            .maybe_listening(listening)
            .maybe_pid(pid)
            .maybe_detail(detail)
            .error();
    }
    error
}

/// Stops the serving server, treating a workspace without one as done.
///
/// A server still building has no port to ask: the outcome says it is
/// starting, and a later stop reaches it once it serves. Every path that
/// reports a stop first waits for the original process to exit or release its
/// election, so a reported stop means the process is leaving: the port closes where the stop begins and
/// the election releases where it ends, so a server that stopped answering
/// its port is one step of that wait, never its answer. `attempt_count`
/// bounds the wait; the commands pass [`STOP_POLL_ATTEMPT_COUNT`], which
/// derives from [`STOP_WAIT_MAX`] over the poll interval.
async fn stop(root: &Path, attempt_count: u32) -> Result<ServerOutcome, RiftError> {
    let lock = match probe(root) {
        ServerPresence::Serving(lock) => lock,
        ServerPresence::Starting => return Ok(ServerOutcome::Starting { pid: None }),
        ServerPresence::Stale(StaleReason::PortUnreachable { pid }) => {
            await_election_released(root, pid, attempt_count).await?;
            return Ok(ServerOutcome::Stopped);
        }
        ServerPresence::Stale(_) => {
            discard_stale_document(root);
            return Ok(ServerOutcome::NotRunning);
        }
        ServerPresence::Absent => return Ok(ServerOutcome::NotRunning),
    };
    let process = ProcessExit::open(lock.pid);
    request_stop(&lock).await?;
    await_stopped(root, lock, process, attempt_count).await?;
    Ok(ServerOutcome::Stopped)
}

/// Delivers the authorized stop request, bounded by
/// [`rift_mcp::STOP_REQUEST_TIMEOUT`].
///
/// Two answers mean the stop was delivered: `202`, the server accepting it,
/// and a refused connect, nothing listening on the recorded port because
/// serving has already ended. Neither says the process is gone, so the
/// caller waits for the election either way.
async fn request_stop(lock: &ServerLock) -> Result<(), RiftError> {
    rift_mcp::request_stop(lock)
        .await
        .map_err(|failure| match failure {
            StopRequestFailure::Failed(source) => errors::cli::server_stop_request_failed()
                .operation("stop request")
                .source(source)
                .error(),
            StopRequestFailure::Refused(status) => {
                let detail = (status == reqwest::StatusCode::UNAUTHORIZED).then_some(
                    "the recorded bearer token was refused; the lock document may be stale",
                );
                errors::cli::server_stop_refused()
                    .status(status.as_u16())
                    .maybe_detail(detail)
                    .error()
            }
        })
}

/// Polls until the stopped server exits or releases the election, bounded by
/// `attempt_count` probes and by the [`poll_window`] they span.
///
/// The caller passes [`STOP_POLL_ATTEMPT_COUNT`], which derives from
/// [`STOP_WAIT_MAX`] over the poll interval. The port closes before the
/// election releases, so a wait on the port alone would let a restart's
/// spawn lose the election to the process it has only just asked to stop. The process
/// handle also proves completion when a replacement takes the election between probes.
async fn await_stopped(
    root: &Path,
    holder: ServerLock,
    process: ProcessExit,
    attempt_count: u32,
) -> Result<(), RiftError> {
    await_stopped_with_probe(root, holder, process, attempt_count, |root| {
        std::future::ready(probe(root))
    })
    .await
}

async fn await_stopped_with_probe<Observation>(
    root: &Path,
    holder: ServerLock,
    mut process: ProcessExit,
    attempt_count: u32,
    mut observe: impl FnMut(&Path) -> Observation,
) -> Result<(), RiftError>
where
    Observation: std::future::Future<Output = ServerPresence>,
{
    let started = tokio::time::Instant::now();
    let deadline = started + poll_window(attempt_count);
    for probe_index in 0..attempt_count {
        if process.exited() || !observe(root).await.election_held() {
            return Ok(());
        }
        let observed = tokio::time::Instant::now();
        let deadline_reached = observed >= deadline;
        rift_tracing::debug!(
            component = "cli",
            operation = "server.stop",
            probe_count = probe_index + 1,
            attempt_count,
            waited = ?(observed - started),
            window = ?poll_window(attempt_count),
            ?observed,
            ?deadline,
            deadline_reached,
            "server wait probe completed"
        );
        if deadline_reached {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    stop_timeout(process, &holder)
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
        Err(error) => rift_tracing::warn!(
            component = "cli",
            path = %document_path.display(),
            %error,
            "stale server lock document could not be removed"
        ),
    }
}

/// Stops the serving server, then starts a fresh detached one.
async fn restart(root: &Path) -> Result<ServerOutcome, RiftError> {
    stop(root, STOP_POLL_ATTEMPT_COUNT).await?;
    start_detached(root, STOP_POLL_ATTEMPT_COUNT).await
}

/// The window one `rift server logs` run selects.
#[derive(Clone, Copy, Debug, Default)]
struct LogsWindow {
    since: Option<LogsBound>,
    until: Option<LogsBound>,
}

/// The store read one `rift server logs` run issues.
///
/// The page is the `--tail` count, bounded by [`LOG_PAGE_RECORDS_MAX`]; `all`
/// reads a whole page at a time. `--since` and `--until` become absolute bounds here: an
/// age counts back from the one tracing clock reading the query keeps, so every page of one
/// run selects the same window.
fn logs_query(
    tail: TailCount,
    window: LogsWindow,
    level: Option<LogLevel>,
    component: Option<&str>,
) -> LogQuery {
    let limit = match tail {
        TailCount::All => LOG_PAGE_RECORDS_MAX,
        TailCount::Newest(count) => usize::try_from(count).unwrap_or(LOG_PAGE_RECORDS_MAX),
    };
    restricted_query(LogQuery::newest(limit), window, level, component)
}

/// `query` restricted to `level`, `component`, and `window`; an age bound counts back from
/// the clock reading `query` already holds, or else reads the tracing clock.
fn restricted_query(
    mut query: LogQuery,
    window: LogsWindow,
    level: Option<LogLevel>,
    component: Option<&str>,
) -> LogQuery {
    if let Some(level) = level {
        query = query.at_level(level.label());
    }
    if let Some(component) = component {
        query = query.for_component(component);
    }
    if let Some(since) = window.since {
        query = since.since(query);
    }
    if let Some(until) = window.until {
        query = until.until(query);
    }
    query
}

/// Prints this workspace's recorded diagnostics, oldest first.
///
/// The metrics database is read directly, so a workspace whose server has stopped still
/// answers, with no server, no index database, and no valid `rift.toml`. A workspace
/// holding no `.rift/metrics` prints nothing, says so on stderr, and creates no state
/// directory.
///
/// The records print through `rift-tracing`'s [`LogLines`], in UTC: as a stored page, each
/// group padded to its own widths, or, when following, as the live stream stderr prints.
async fn print_logs(
    root: &Path,
    query: &LogQuery,
    tail: TailCount,
    mode: &LogsMode,
) -> Result<(), RiftError> {
    let Some(reader) = WorkspaceStorage::open_logs(root) else {
        eprintln!("{NO_RECORDED_LOGS}");
        return Ok(());
    };
    let mut lines = match mode {
        LogsMode::Once => LogLines::stored_page(),
        LogsMode::Following => LogLines::live_stream(),
    };
    let printed = match tail {
        TailCount::All => print_records_after(&reader, query, 0, &mut lines).await?,
        TailCount::Newest(_) => print_newest_records(&reader, query, &mut lines).await?,
    };
    match mode {
        LogsMode::Once => Ok(()),
        LogsMode::Following => follow_records(&reader, query, printed, &mut lines).await,
    }
}

/// One read of the metrics database on a blocking thread, through a connection of its own.
async fn read_records(
    reader: &LogReader,
    query: LogQuery,
    read: fn(&LogReads, &LogQuery) -> Result<Vec<StoredLogRecord>, RiftError>,
) -> Result<Vec<StoredLogRecord>, RiftError> {
    let reader = reader.clone();
    let records = tokio::task::spawn_blocking(move || read(&reader.connect()?, &query))
        .await
        .map_err(|error| {
            errors::cli::server_logs_unavailable()
                .operation("read recorded logs")
                .detail(error.to_string())
                .error()
        })?;
    records.map_err(|source| {
        errors::cli::server_logs_unavailable()
            .operation("read recorded logs")
            .source(source)
            .error()
    })
}

/// Prints every record after `after`, oldest first, and returns the newest
/// identity it printed.
///
/// One read is bounded by the query's own page, itself bounded by
/// [`LOG_PAGE_RECORDS_MAX`]. The loop repeats only while a page comes back
/// full, and the store's retention bounds how many full pages there can be.
async fn print_records_after(
    reader: &LogReader,
    query: &LogQuery,
    after: i64,
    lines: &mut LogLines,
) -> Result<i64, RiftError> {
    let mut newest = after;
    loop {
        let page = read_records(reader, query.clone().after(newest), LogReads::following).await?;
        print_page(&page, lines);
        if let Some(last) = page.last() {
            newest = last.identity();
        }
        if page.len() < query.limit() {
            return Ok(newest);
        }
    }
}

/// Prints the newest records the query selects, oldest first, and returns the
/// newest identity it printed. The read is bounded by the query's own page.
async fn print_newest_records(
    reader: &LogReader,
    query: &LogQuery,
    lines: &mut LogLines,
) -> Result<i64, RiftError> {
    let mut records = read_records(reader, query.clone(), LogReads::recent).await?;
    records.reverse();
    print_page(&records, lines);
    Ok(records.last().map_or(0, StoredLogRecord::identity))
}

/// Prints `page` on stdout as `lines` lays it out.
fn print_page(page: &[StoredLogRecord], lines: &mut LogLines) {
    print!("{}", lines.lines(page.iter().map(StoredLogRecord::record)));
}

/// Prints records as the server writes them, until the operator interrupts.
async fn follow_records(
    reader: &LogReader,
    query: &LogQuery,
    printed: i64,
    lines: &mut LogLines,
) -> Result<(), RiftError> {
    let interrupted = CancellationToken::new();
    let interrupt = tokio::spawn(cancel_on_interrupt(interrupted.clone()));
    let followed = follow_until_interrupt(reader, query, printed, &interrupted, lines).await;
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
    reader: &LogReader,
    query: &LogQuery,
    printed: i64,
    interrupted: &CancellationToken,
    lines: &mut LogLines,
) -> Result<(), RiftError> {
    let mut newest = printed;
    while !interrupted.is_cancelled() {
        newest = print_records_after(reader, query, newest, lines).await?;
        tokio::select! {
            () = interrupted.cancelled() => {}
            () = tokio::time::sleep(LOG_FOLLOW_POLL_INTERVAL) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::IntoFuture as _;
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use super::{
        AuthMode, ChildWatch, LogLevel, LogQuery, LogsBound, LogsMode, LogsWindow,
        PRESENCE_POLL_INTERVAL, ProcessExit, RunningLogDrain, SERVER_DATABASE_STOP_RESERVE,
        SERVER_EXPORT_STOP_RESERVE, SERVER_LOG_FLUSH_RESERVE, SERVER_STOP_DEADLINE,
        START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, STOP_POLL_ATTEMPT_COUNT, STOP_WAIT_MAX,
        ServerOutcome, StaleReason, StartMode, StartSpawns, StartedServer, TailCount, TokenCheck,
        await_election_released, await_election_released_with_probe, await_serving,
        await_serving_with_probe, await_stopped, await_stopped_with_probe, discard_stale_document,
        foreground_refused, later_stages_reserve, log_flush_end_reserve, logs_mode, logs_query,
        print_logs, request_stop, restricted_query, stale_reason_phrase, start_detached,
        start_mode, status, stop, stop_log_drain, token_check,
    };
    use rift_error::errors;
    use rift_mcp::{START_SPAWN_COUNT_MAX, StartExit};
    use rift_protocol::lock::{ProductIdentity, ServerLock, ServerLockViolation};
    use rift_tracing::{
        LOG_BATCH_RECORDS_MAX, LOG_LEVELS, LOG_PAGE_RECORDS_MAX, LogRecord, LogStore,
    };
    use std::path::Path;
    use std::time::Duration;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn holder() -> ServerLock {
        ServerLock {
            port: 12_345,
            token: "a".repeat(rift_protocol::lock::SERVER_TOKEN_LENGTH),
            pid: 4_242,
            identity: ProductIdentity {
                version: "0.0.11".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server: None,
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
        let (listener, port) = answering_port()?;
        drop(listener);
        Ok(port)
    }

    /// A loopback listener a probe's connect reaches, and the port it holds.
    fn answering_port() -> TestResult<(std::net::TcpListener, u16)> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok((listener, port))
    }

    /// Probes the slow-probe cases allow: a one-second window at the poll interval.
    const SLOW_PROBE_ATTEMPT_COUNT: u32 = 10;
    /// Duration each test observation advances before returning a held election.
    const SLOW_PROBE_COST: std::time::Duration = std::time::Duration::from_millis(300);

    async fn slow_port_probe(
        pid: u32,
        observations: &std::cell::RefCell<Vec<std::time::Duration>>,
        started: tokio::time::Instant,
        wall_started: std::time::Instant,
    ) -> rift_mcp::ServerPresence {
        let logical_start = started.elapsed();
        let wall_start = wall_started.elapsed();
        observations.borrow_mut().push(logical_start);
        let probe_count = observations.borrow().len();
        eprintln!(
            "probe started: probe_count={probe_count}, clock=paused, \
             logical_start={logical_start:?}, wall_start={wall_start:?}, requested={SLOW_PROBE_COST:?}"
        );
        // Advancing the paused clock proves the polling window independently of
        // how long an OS sleep takes (issue #536).
        tokio::time::advance(SLOW_PROBE_COST).await;
        let logical_end = started.elapsed();
        let wall_end = wall_started.elapsed();
        eprintln!(
            "probe completed: probe_count={probe_count}, logical_end={logical_end:?}, \
             logical_cost={:?}, wall_end={wall_end:?}, wall_cost={:?}",
            logical_end.checked_sub(logical_start),
            wall_end.checked_sub(wall_start),
        );
        rift_mcp::ServerPresence::Stale(StaleReason::PortUnreachable { pid })
    }

    /// A stand-in for the spawned server: running until told otherwise, and
    /// then exited the way `exit` names.
    #[derive(Debug)]
    struct FakeChild {
        pid: u32,
        running: bool,
        exit: FakeExit,
    }

    /// How a [`FakeChild`] that is not running ended.
    #[derive(Debug, Clone, Copy)]
    enum FakeExit {
        LostElection,
        Failed,
    }

    impl FakeChild {
        fn running(pid: u32) -> Self {
            Self {
                pid,
                running: true,
                exit: FakeExit::Failed,
            }
        }

        fn exited(pid: u32, exit: FakeExit) -> Self {
            Self {
                pid,
                running: false,
                exit,
            }
        }
    }

    impl ChildWatch for FakeChild {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn is_running(&mut self) -> bool {
            self.running
        }
    }

    impl StartedServer for FakeChild {
        type Failure = u32;

        fn observed_exit(&mut self) -> Option<StartExit<u32>> {
            if self.running {
                return None;
            }
            Some(match self.exit {
                FakeExit::LostElection => StartExit::LostElection {
                    stderr: String::new(),
                },
                FakeExit::Failed => StartExit::Failed(self.pid),
            })
        }
    }

    /// The spawns of a start that spawned `child` first.
    fn spawned(child: FakeChild) -> StartSpawns<FakeChild> {
        let mut spawns = StartSpawns::default();
        spawns
            .spawn(|| Ok(child))
            .expect("a fake child always launches");
        spawns
    }

    /// A launch for a wait that must not spawn again.
    fn no_launch() -> std::io::Result<FakeChild> {
        Err(std::io::Error::other("this wait must not spawn again"))
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
    fn each_mode_reserves_only_the_later_stop_stages_it_runs() {
        // A workspace server keeps time for the log flush, metrics close, and export.
        let workspace = later_stages_reserve(false);
        assert_eq!(
            workspace,
            SERVER_EXPORT_STOP_RESERVE + SERVER_LOG_FLUSH_RESERVE + SERVER_DATABASE_STOP_RESERVE
        );
        assert_eq!(
            log_flush_end_reserve(false),
            SERVER_DATABASE_STOP_RESERVE + SERVER_EXPORT_STOP_RESERVE
        );
        // A repository server runs its routing drain's stop, then the export. Its workspaces
        // close their metrics databases while serving stops.
        let repository = later_stages_reserve(true);
        assert_eq!(
            repository,
            SERVER_EXPORT_STOP_RESERVE + SERVER_LOG_FLUSH_RESERVE
        );
        assert_eq!(log_flush_end_reserve(true), SERVER_EXPORT_STOP_RESERVE);
        assert_eq!(
            workspace.checked_sub(repository),
            Some(SERVER_DATABASE_STOP_RESERVE),
            "a repository server's serving stages gain the close reserve"
        );
        assert_eq!(
            SERVER_STOP_DEADLINE.checked_sub(repository),
            Some(Duration::from_millis(3_000)),
            "a repository server's serving stages keep 3 s of the stop's 4 s"
        );
    }

    #[test]
    fn the_server_stop_deadline_leaves_before_the_cli_gives_up() {
        assert!(
            SERVER_STOP_DEADLINE < STOP_WAIT_MAX,
            "the server-side stop must finish before the CLI's wait: \
             SERVER_STOP_DEADLINE={SERVER_STOP_DEADLINE:?}, STOP_WAIT_MAX={STOP_WAIT_MAX:?}"
        );
    }

    #[test]
    fn foreground_flag_selects_the_mode() {
        assert!(matches!(start_mode(true), StartMode::Foreground));
        assert!(matches!(start_mode(false), StartMode::Detached));
    }

    #[test]
    fn the_auth_flag_selects_the_token_policy() {
        assert_eq!(token_check(AuthMode::Token), TokenCheck::Required);
        assert_eq!(token_check(AuthMode::Skip), TokenCheck::Skipped);
        assert_eq!(AuthMode::default(), AuthMode::Token);
    }

    /// The `log drain` stop stage flushes what the drain holds into the metrics database
    /// and leaves nothing unwritten when it joins by its deadline.
    #[tokio::test]
    async fn a_joined_log_drain_leaves_its_records_written() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = Arc::new(
            rift_tracing::LogStore::open(&directory.path().join("metrics"), None)
                .await
                .expect("the metrics database opens"),
        );
        let (recorder, drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        rift_tracing::info!(component = "test", "written by the final flush");
        drop(recorder);
        let running = RunningLogDrain::spawn(drain, Arc::clone(&store), 100);

        let unwritten = stop_log_drain(
            Some(running),
            tokio::time::Instant::now() + SERVER_STOP_DEADLINE,
        )
        .await;

        assert_eq!(unwritten, None, "a joined drain leaves nothing to count");
        let stored = store
            .reader()
            .connect()
            .and_then(|reads| reads.count())
            .expect("the count reads");
        assert_eq!(stored, 1, "the final flush wrote the record");
    }

    /// A repository server's `log drain` stage stops its routing drain after each workspace
    /// consumer flushed what named its workspace: the store holds the record, nothing is
    /// left unwritten, and both stops end well inside the stop's deadline.
    #[tokio::test]
    async fn a_routing_log_drain_stops_after_its_workspace_consumer_flushed() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let store = Arc::new(
            rift_tracing::LogStore::open(&directory.path().join("metrics"), None)
                .await
                .expect("the metrics database opens"),
        );
        let (_recorder, drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let routing = RunningLogDrain::spawn_routed(drain, 100);
        let consumer = RunningLogDrain::for_workspace("/served", Arc::clone(&store))
            .expect("the routing drain starts a consumer");
        rift_tracing::info!(
            component = "test",
            workspace = "/served",
            "written by the consumer's final flush"
        );

        let started = tokio::time::Instant::now();
        let deadline = started + SERVER_STOP_DEADLINE;
        let consumer_unwritten = consumer.stop(deadline - SERVER_LOG_FLUSH_RESERVE).await;
        let routing_unwritten = stop_log_drain(Some(routing), deadline).await;

        assert_eq!(consumer_unwritten, None, "the consumer joined");
        assert_eq!(routing_unwritten, None, "the routing drain joined");
        assert!(
            started.elapsed() < SERVER_LOG_FLUSH_RESERVE,
            "both stops ended inside the flush reserve: {:?}",
            started.elapsed()
        );
        let stored = store
            .reader()
            .connect()
            .and_then(|reads| reads.count())
            .expect("the count reads");
        assert_eq!(stored, 1, "the consumer's final flush wrote the record");
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
        assert_eq!(
            ServerOutcome::Starting { pid: Some(42) }.to_string(),
            "⏳ rift server starting (pid 42): it is indexing the workspace, and `rift server \
             status` shows it listening once the index is built"
        );
        assert_eq!(
            ServerOutcome::Starting { pid: None }.to_string(),
            "⏳ rift server starting: it is indexing the workspace, and `rift server status` \
             shows it listening once the index is built"
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
                reason: "no process holds the election lock".to_owned()
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
            (
                StaleReason::PortUnreachable { pid: 4_242 },
                "the server at pid 4242 no longer answers its port and is shutting down",
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
        let (_listener, port) = answering_port()?;
        guard.publish(&holder_on(port))?;
        assert_eq!(
            status(directory.path()),
            ServerOutcome::Serving {
                port,
                pid: 4_242,
                version: "0.0.11".to_owned(),
            }
        );
        Ok(())
    }

    #[test]
    fn status_reports_a_building_holder_as_starting() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        assert_eq!(
            status(directory.path()),
            ServerOutcome::Starting { pid: None }
        );
        Ok(())
    }

    #[test]
    fn status_reports_a_holder_whose_port_refuses_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        guard.publish(&holder_on(dead_port()?))?;
        assert_eq!(
            status(directory.path()),
            ServerOutcome::Stale {
                reason: "the server at pid 4242 no longer answers its port and is shutting down"
                    .to_owned()
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
                reason: "no process holds the election lock".to_owned()
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
        let lock = holder();
        let cases: Vec<(super::RiftError, &str)> = vec![
            (
                errors::cli::server_already_serving()
                    .listening(format!("127.0.0.1:{}", lock.port))
                    .pid(lock.pid)
                    .error(),
                "rift.cli.server_already_serving",
            ),
            (
                errors::cli::server_spawn_failed()
                    .operation("spawn detached server")
                    .source(std::io::Error::other("fixture"))
                    .error(),
                "rift.cli.server_spawn_failed",
            ),
            (
                errors::cli::server_start_exited().pid(7).error(),
                "rift.cli.server_start_exited",
            ),
            (
                errors::cli::server_start_timed_out()
                    .waited(START_WAIT_MAX)
                    .error(),
                "rift.cli.server_start_timed_out",
            ),
            (
                errors::cli::server_election_unreleased()
                    .pid(7)
                    .waited(STOP_WAIT_MAX)
                    .error(),
                "rift.cli.server_election_unreleased",
            ),
            (
                errors::cli::server_stop_refused()
                    .status(reqwest::StatusCode::UNAUTHORIZED.as_u16())
                    .detail("the recorded bearer token was refused; the lock document may be stale")
                    .error(),
                "rift.cli.server_stop_refused",
            ),
            (
                errors::cli::server_stop_timed_out()
                    .waited(STOP_WAIT_MAX)
                    .listening(format!("127.0.0.1:{}", lock.port))
                    .pid(lock.pid)
                    .error(),
                "rift.cli.server_stop_timed_out",
            ),
        ];
        for (error, code) in cases {
            assert_eq!(error.slug().as_str(), code, "{error}");
        }
    }

    #[test]
    fn failure_text_names_evidence_and_next_steps() {
        let lock = holder();
        let already = errors::cli::server_already_serving()
            .listening(format!("127.0.0.1:{}", lock.port))
            .pid(lock.pid)
            .error()
            .to_string();
        assert!(already.contains("127.0.0.1:12345"), "{already}");
        assert!(already.contains("pid 4242"), "{already}");
        assert!(already.contains("rift server stop"), "{already}");

        let unpublished = errors::cli::server_already_serving()
            .detail("the holding server has not published its lock document yet")
            .error()
            .to_string();
        assert!(unpublished.contains("has not published"), "{unpublished}");

        let timed_out = errors::cli::server_start_timed_out()
            .waited(START_WAIT_MAX)
            .error()
            .to_string();
        assert!(
            timed_out.contains(&format!("{START_WAIT_MAX:?}")),
            "{timed_out}"
        );
        assert!(timed_out.contains("--foreground"), "{timed_out}");

        let exited_error = errors::cli::server_start_exited().pid(7).error();
        assert_eq!(exited_error.slug(), errors::cli::server_start_exited::SLUG);
        assert!(
            exited_error
                .context()
                .any(|(key, value)| key == "pid" && value == "7"),
            "typed process identifier must remain present: {exited_error:?}"
        );
        let exited = exited_error.to_string();
        assert!(exited.contains("process 7"), "{exited}");
        assert!(exited.contains("exited before publishing"), "{exited}");
        assert!(
            exited.contains(&format!(".rift/{}", rift_mcp::SERVER_STDERR_FILE_NAME)),
            "the action names the stderr file by its one spelling: {exited}"
        );
        assert!(
            exited.contains("rift server logs --level error"),
            "{exited}"
        );
        assert!(
            !exited.contains("binary is runnable"),
            "the registry's shared action is replaced: {exited}"
        );

        let unreleased = errors::cli::server_election_unreleased()
            .pid(7)
            .waited(STOP_WAIT_MAX)
            .error()
            .to_string();
        assert!(unreleased.contains("process 7"), "{unreleased}");
        assert!(
            unreleased.contains("still holds the election"),
            "{unreleased}"
        );
        assert!(unreleased.contains("end the reported pid"), "{unreleased}");

        let unauthorized = errors::cli::server_stop_refused()
            .status(reqwest::StatusCode::UNAUTHORIZED.as_u16())
            .detail("the recorded bearer token was refused; the lock document may be stale")
            .error()
            .to_string();
        assert!(unauthorized.contains("status 401"), "{unauthorized}");
        assert!(unauthorized.contains("stale"), "{unauthorized}");

        let refused = errors::cli::server_stop_refused()
            .status(reqwest::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
            .error()
            .to_string();
        assert!(refused.contains("status 500"), "{refused}");
        assert!(!refused.contains("stale"), "{refused}");

        let stop_timed_out = errors::cli::server_stop_timed_out()
            .waited(STOP_WAIT_MAX)
            .listening(format!("127.0.0.1:{}", lock.port))
            .pid(lock.pid)
            .error()
            .to_string();
        assert!(stop_timed_out.contains("10s"), "{stop_timed_out}");
        assert!(stop_timed_out.contains("pid 4242"), "{stop_timed_out}");
    }

    #[test]
    fn spawn_and_request_failures_keep_their_sources() {
        let spawn = errors::cli::server_spawn_failed()
            .operation("spawn detached server")
            .source(std::io::Error::other("fixture"))
            .error();
        assert!(std::error::Error::source(&spawn).is_some());
        let timeout = errors::cli::server_start_timed_out()
            .waited(START_WAIT_MAX)
            .error();
        assert!(std::error::Error::source(&timeout).is_none());
    }

    #[test]
    fn election_fault_forwards_identity_evidence_and_source() {
        let election = errors::mcp::election_storage_failed()
            .operation("open election file")
            .path(Path::new(".rift/server.lock"))
            .source(std::io::Error::other("disk gone"))
            .error();
        let expected_slug = election.slug();
        let expected_context: Vec<_> = election.context().collect();
        let error = foreground_refused(Path::new("."), election);
        assert_eq!(error.slug(), expected_slug);
        assert_eq!(error.context().collect::<Vec<_>>(), expected_context);
        assert!(
            std::error::Error::source(&error).is_some(),
            "the wrapped election failure must stay on the source chain"
        );
    }

    #[test]
    fn spawn_failure_names_its_operation() {
        let rendered = errors::cli::server_spawn_failed()
            .operation("spawn detached server")
            .source(std::io::Error::other("fixture"))
            .error()
            .to_string();
        assert!(rendered.contains("spawn detached server"), "{rendered}");
    }

    #[test]
    fn stop_request_failure_names_its_operation_and_keeps_its_source() {
        let error = errors::cli::server_stop_request_failed()
            .operation("stop request")
            .source(request_error())
            .error();
        assert_eq!(error.slug(), errors::cli::server_stop_request_failed::SLUG);
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
        let mut spawns = StartSpawns::<FakeChild>::default();
        let error = await_serving(directory.path(), 1, None, &mut spawns, no_launch)
            .await
            .expect_err("a workspace nobody serves must time the wait out");
        assert_eq!(error.slug(), errors::cli::server_start_timed_out::SLUG);
        Ok(())
    }

    /// The pre-spawn leftover document paired with a held election is not an
    /// answer: the wait holds out for the fresh document the holder
    /// publishes, and returns its facts, not the leftover's.
    #[tokio::test(start_paused = true)]
    async fn await_serving_holds_out_for_the_fresh_document_over_a_leftover() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let (_listener, port) = answering_port()?;
        let leftover = serde_json::to_vec(&holder_on(port))?;
        std::fs::write(rift_mcp::document_path(directory.path()), &leftover)?;
        let fresh = ServerLock {
            pid: 9_999,
            ..holder_on(port)
        };
        let outcome = {
            let publish = async {
                tokio::time::sleep(PRESENCE_POLL_INTERVAL * 2).await;
                guard.publish(&fresh).expect("the holder must publish");
            };
            let mut spawns = StartSpawns::<FakeChild>::default();
            let wait = await_serving(
                directory.path(),
                START_POLL_ATTEMPT_COUNT,
                Some(&leftover),
                &mut spawns,
                no_launch,
            );
            let (outcome, ()) = tokio::join!(wait, publish);
            outcome?
        };
        assert!(
            matches!(outcome, ServerOutcome::Listening { pid: 9_999, .. }),
            "the wait must answer with the published document: {outcome:?}"
        );
        Ok(())
    }

    /// A leftover that never scrubs keeps the wait unanswered to its bound; the held
    /// election behind it means a server is starting, so the bound is not a failure.
    #[tokio::test(start_paused = true)]
    async fn await_serving_reports_starting_while_the_leftover_stands_under_a_holder() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        let leftover = serde_json::to_vec(&holder())?;
        std::fs::write(rift_mcp::document_path(directory.path()), &leftover)?;
        let mut spawns = StartSpawns::<FakeChild>::default();
        let wait = await_serving(directory.path(), 2, Some(&leftover), &mut spawns, no_launch);
        let outcome = wait.await?;
        assert_eq!(outcome, ServerOutcome::Starting { pid: None });
        Ok(())
    }

    /// The child this command spawned is still running when the bound runs out: it is
    /// indexing, and the outcome names it.
    #[tokio::test(start_paused = true)]
    async fn await_serving_reports_the_running_child_as_starting_at_the_bound() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut spawns = spawned(FakeChild::running(77));
        let outcome = await_serving(directory.path(), 2, None, &mut spawns, no_launch).await?;
        assert_eq!(outcome, ServerOutcome::Starting { pid: Some(77) });
        Ok(())
    }

    /// The child exited and nobody holds the election: nothing is left to publish, so
    /// the wait ends before its bound with the child's pid.
    #[tokio::test(start_paused = true)]
    async fn await_serving_fails_at_once_when_the_child_exited_and_nobody_holds() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut spawns = spawned(FakeChild::exited(77, FakeExit::Failed));
        let wait = await_serving(
            directory.path(),
            START_POLL_ATTEMPT_COUNT,
            None,
            &mut spawns,
            no_launch,
        );
        let error = wait
            .await
            .expect_err("an exited child under a free election must fail the start");
        assert!(
            error.slug() == errors::cli::server_start_exited::SLUG
                && error
                    .context()
                    .any(|(key, value)| key == "pid" && value == "77"),
            "{error:?}"
        );
        Ok(())
    }

    /// The child exited because another starter holds the election: the wait keeps
    /// polling for that holder's document, and reports a starting server at its bound.
    #[tokio::test(start_paused = true)]
    async fn await_serving_waits_for_another_holder_when_the_child_lost() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        let mut spawns = spawned(FakeChild::exited(77, FakeExit::LostElection));
        let outcome = await_serving(directory.path(), 2, None, &mut spawns, no_launch).await?;
        assert_eq!(outcome, ServerOutcome::Starting { pid: None });
        Ok(())
    }

    /// The child lost an election nobody holds - a lock no serving process holds met its
    /// claim - so the wait spawns again, once, and reports the new child starting.
    #[tokio::test(start_paused = true)]
    async fn await_serving_spawns_again_when_the_child_lost_an_election_nobody_holds() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let mut spawns = spawned(FakeChild::exited(77, FakeExit::LostElection));
        let mut launches = 0_u32;
        let launch = || {
            launches += 1;
            Ok(FakeChild::running(78))
        };
        let outcome = await_serving(directory.path(), 5, None, &mut spawns, launch).await?;
        assert_eq!(outcome, ServerOutcome::Starting { pid: Some(78) });
        assert_eq!(launches, 1, "one respawn replaces the lost child");
        Ok(())
    }

    /// Every child loses an election nobody holds: the wait spawns until the count is
    /// spent, then waits out its bound and reports the timeout.
    #[tokio::test(start_paused = true)]
    async fn await_serving_stops_spawning_at_the_spawn_count() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut spawns = spawned(FakeChild::exited(77, FakeExit::LostElection));
        let mut launches = 0_u32;
        let launch = || {
            launches += 1;
            Ok(FakeChild::exited(78, FakeExit::LostElection))
        };
        let wait = await_serving(
            directory.path(),
            START_POLL_ATTEMPT_COUNT,
            None,
            &mut spawns,
            launch,
        );
        let error = wait
            .await
            .expect_err("a start whose every child loses must time the wait out");
        assert_eq!(error.slug(), errors::cli::server_start_timed_out::SLUG);
        assert_eq!(launches, START_SPAWN_COUNT_MAX - 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn await_stopped_times_out_while_the_holder_keeps_the_election() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let document = ServerLock {
            pid: std::process::id(),
            ..holder()
        };
        guard.publish(&document)?;
        let process = ProcessExit::open(document.pid);
        let error = await_stopped(directory.path(), document, process, 1)
            .await
            .expect_err("a holder that keeps the election must time the wait out");
        assert!(
            error.slug() == errors::cli::server_stop_timed_out::SLUG,
            "the timeout must carry the holder: {error:?}"
        );
        Ok(())
    }

    #[test]
    #[ignore = "child process used by the process exit tests"]
    fn stopped_process_probe() -> TestResult {
        use std::io::Read as _;
        let mut byte = [0];
        assert_eq!(
            std::io::stdin().read(&mut byte)?,
            0,
            "parent closes fixture input"
        );
        Ok(())
    }

    fn stopped_process_child() -> TestResult<tokio::process::Child> {
        use std::process::Stdio;
        Ok(tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "server::tests::stopped_process_probe",
                "--ignored",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?)
    }

    #[tokio::test]
    async fn stopped_process_does_not_wait_for_a_replacement_election() -> TestResult {
        for serving in [false, true] {
            let directory = tempfile::tempdir()?;
            let mut child = stopped_process_child()?;
            let original = ServerLock {
                pid: child.id().expect("child must still have a process id"),
                ..holder()
            };
            let process = ProcessExit::open(original.pid);
            assert!(
                matches!(process, ProcessExit::Waiting { .. }),
                "{process:?}"
            );

            // The old election has released. A replacement keeps it for the whole wait,
            // either before publication or after publishing its own serving document.
            let replacement = rift_mcp::claim(directory.path())?;
            let (listener, port) = answering_port()?;
            if serving {
                replacement.publish(&ServerLock {
                    pid: std::process::id(),
                    ..holder_on(port)
                })?;
            }
            let presence = rift_mcp::probe(directory.path());
            assert!(
                if serving {
                    matches!(presence, rift_mcp::ServerPresence::Serving(_))
                } else {
                    matches!(presence, rift_mcp::ServerPresence::Starting)
                },
                "{presence:?}"
            );

            drop(child.stdin.take());
            // No Child::wait or try_wait occurs before this returns. The process handle
            // must observe exit even while the original child remains unreaped on Unix.
            await_stopped(directory.path(), original, process, 20).await?;
            assert!(rift_mcp::probe(directory.path()).election_held());
            assert!(child.wait().await?.success());
            drop((replacement, listener));
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn changed_or_missing_document_does_not_prove_process_exit() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let original = ServerLock {
            pid: std::process::id(),
            ..holder()
        };
        let (listener, port) = answering_port()?;
        for document in [None, Some(holder_on(port))] {
            if let Some(document) = document {
                guard.publish(&document)?;
            }
            let error = await_stopped(
                directory.path(),
                original.clone(),
                ProcessExit::open(original.pid),
                1,
            )
            .await
            .expect_err("a live original process and held election must keep waiting");
            assert!(
                error.slug() == errors::cli::server_stop_timed_out::SLUG,
                "{error:?}"
            );
        }
        drop(listener);
        Ok(())
    }

    #[tokio::test]
    async fn already_exited_process_does_not_wait_for_a_building_holder() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut child = stopped_process_child()?;
        let pid = child.id().expect("child must have a process id");
        drop(child.stdin.take());
        assert!(child.wait().await?.success());
        let _replacement = rift_mcp::claim(directory.path())?;
        let mut process = ProcessExit::open(pid);
        assert!(
            process.exited(),
            "already-exited process must be observed: {process:?}"
        );
        await_election_released(directory.path(), pid, 1).await?;
        assert!(rift_mcp::probe(directory.path()).election_held());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn process_observation_errors_keep_waiting_and_retain_their_cause() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Other,
        ] {
            let process =
                ProcessExit::observed(Err(std::io::Error::new(kind, "process observation failed")));
            let error = await_stopped(directory.path(), holder(), process, 1)
                .await
                .expect_err("observation failure must not prove process exit");
            assert_eq!(error.slug(), errors::cli::server_stop_timed_out::SLUG);
            assert!(
                error.to_string().contains("process observation failed"),
                "{error}"
            );
        }
        for pid in [0, u32::MAX] {
            assert!(
                !ProcessExit::open(pid).exited(),
                "invalid process id must not prove exit"
            );
        }
        for poll_again in [false, true] {
            let mut process = ProcessExit::open(std::process::id());
            let ProcessExit::Waiting { last_error, .. } = &mut process else {
                panic!("the current process handle must open: {process:?}");
            };
            *last_error = Some(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "process wait interrupted",
            ));
            if poll_again {
                assert!(!process.exited(), "the current process must remain alive");
            }
            let detail = process.refusal_detail();
            assert_eq!(
                detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("process wait interrupted")),
                !poll_again,
                "a successful poll must clear the prior wait failure: {detail:?}"
            );
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn await_election_released_returns_once_the_holder_is_gone() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let pid = std::process::id();
        let error = await_election_released(directory.path(), pid, 1)
            .await
            .expect_err("a held election must time the wait out");
        assert!(
            error.slug() == errors::cli::server_election_unreleased::SLUG
                && error
                    .context()
                    .any(|(key, value)| key == "pid" && value == pid.to_string()),
            "{error:?}"
        );
        drop(guard);
        await_election_released(directory.path(), pid, 1).await?;
        Ok(())
    }

    /// A stop wait with slow probes ends inside its polling window.
    #[tokio::test(start_paused = true)]
    async fn a_stop_wait_ends_at_its_window_when_every_probe_is_slow() -> TestResult {
        let (_trace, _drain) = rift_tracing::ScopedRecorder::builder()
            .capture("debug")
            .install()?;
        let directory = tempfile::tempdir()?;
        let pid = std::process::id();
        let process = ProcessExit::open(pid);
        let probe_times = std::cell::RefCell::new(Vec::new());
        let started = tokio::time::Instant::now();
        let wall_started = std::time::Instant::now();
        let result = await_stopped_with_probe(
            directory.path(),
            holder(),
            process,
            SLOW_PROBE_ATTEMPT_COUNT,
            |_| slow_port_probe(pid, &probe_times, started, wall_started),
        )
        .await;
        eprintln!(
            "wait completed: clock=paused, probe_count={}, logical_elapsed={:?}, wall_elapsed={:?}, result={result:?}",
            probe_times.borrow().len(),
            started.elapsed(),
            wall_started.elapsed(),
        );
        let error = result.expect_err("a held election times the stop wait out");
        assert_eq!(
            *probe_times.borrow(),
            [0, 400, 800].map(std::time::Duration::from_millis),
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(1100));
        assert!(
            error.slug() == errors::cli::server_stop_timed_out::SLUG,
            "{error:?}"
        );
        Ok(())
    }

    /// The wait for a holder to release its election ends at its window under slow probes.
    #[tokio::test(start_paused = true)]
    async fn an_election_wait_ends_at_its_window_when_every_probe_is_slow() -> TestResult {
        let (_trace, _drain) = rift_tracing::ScopedRecorder::builder()
            .capture("debug")
            .install()?;
        let directory = tempfile::tempdir()?;
        let pid = std::process::id();
        let probe_times = std::cell::RefCell::new(Vec::new());
        let started = tokio::time::Instant::now();
        let wall_started = std::time::Instant::now();
        let result = await_election_released_with_probe(
            directory.path(),
            pid,
            SLOW_PROBE_ATTEMPT_COUNT,
            |_| slow_port_probe(pid, &probe_times, started, wall_started),
        )
        .await;
        eprintln!(
            "wait completed: clock=paused, probe_count={}, logical_elapsed={:?}, wall_elapsed={:?}, result={result:?}",
            probe_times.borrow().len(),
            started.elapsed(),
            wall_started.elapsed(),
        );
        let error = result.expect_err("a held election times the wait out");
        assert_eq!(
            *probe_times.borrow(),
            [0, 400, 800].map(std::time::Duration::from_millis),
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(1100));
        assert!(
            error.slug() == errors::cli::server_election_unreleased::SLUG,
            "{error:?}"
        );
        Ok(())
    }

    /// The start wait ends at its window under slow probes, and its closing probe is the
    /// one probe it adds past the window.
    #[tokio::test(start_paused = true)]
    async fn a_start_wait_ends_at_its_window_when_every_probe_is_slow() -> TestResult {
        let (_trace, _drain) = rift_tracing::ScopedRecorder::builder()
            .capture("debug")
            .install()?;
        let directory = tempfile::tempdir()?;
        let mut spawns = StartSpawns::<FakeChild>::default();
        let probe_times = std::cell::RefCell::new(Vec::new());
        let started = tokio::time::Instant::now();
        let wall_started = std::time::Instant::now();
        let result = await_serving_with_probe(
            directory.path(),
            SLOW_PROBE_ATTEMPT_COUNT,
            None,
            &mut spawns,
            no_launch,
            |_| slow_port_probe(std::process::id(), &probe_times, started, wall_started),
        )
        .await;
        eprintln!(
            "wait completed: clock=paused, probe_count={}, logical_elapsed={:?}, wall_elapsed={:?}, result={result:?}",
            probe_times.borrow().len(),
            started.elapsed(),
            wall_started.elapsed(),
        );
        let error =
            result.expect_err("a held election whose port does not answer never serves this start");
        assert_eq!(
            *probe_times.borrow(),
            [0, 400, 800, 1100].map(std::time::Duration::from_millis),
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(1400));
        assert!(
            error.slug() == errors::cli::server_start_timed_out::SLUG,
            "{error:?}"
        );
        Ok(())
    }

    /// Records actual blocking probe costs alongside the start wait's deadline decisions.
    #[tokio::test]
    async fn a_start_wait_records_synchronous_probe_costs() -> TestResult {
        let (_trace, _drain) = rift_tracing::ScopedRecorder::builder()
            .capture("debug")
            .install()?;
        let directory = tempfile::tempdir()?;
        let mut spawns = StartSpawns::<FakeChild>::default();
        let mut observations = Vec::new();
        let started = tokio::time::Instant::now();
        let wall_started = std::time::Instant::now();
        let result = await_serving_with_probe(
            directory.path(),
            SLOW_PROBE_ATTEMPT_COUNT,
            None,
            &mut spawns,
            no_launch,
            |_| {
                let logical_start = started.elapsed();
                let wall_start = wall_started.elapsed();
                let probe_count = observations.len() + 1;
                eprintln!(
                    "probe started: probe_count={probe_count}, clock=running, \
                     logical_start={logical_start:?}, wall_start={wall_start:?}, requested={SLOW_PROBE_COST:?}"
                );
                let sleep_started = std::time::Instant::now();
                std::thread::sleep(SLOW_PROBE_COST);
                let sleep_cost = sleep_started.elapsed();
                let logical_end = started.elapsed();
                let wall_end = wall_started.elapsed();
                eprintln!(
                    "probe completed: probe_count={probe_count}, logical_end={logical_end:?}, \
                     logical_cost={:?}, wall_end={wall_end:?}, wall_cost={:?}, sleep_cost={sleep_cost:?}",
                    logical_end.checked_sub(logical_start),
                    wall_end.checked_sub(wall_start),
                );
                observations.push((wall_start, wall_end, sleep_cost));
                std::future::ready(rift_mcp::ServerPresence::Stale(
                    StaleReason::PortUnreachable {
                        pid: std::process::id(),
                    },
                ))
            },
        )
        .await;
        let wall_elapsed = wall_started.elapsed();
        eprintln!(
            "wait completed: clock=running, probe_count={}, logical_elapsed={:?}, \
             wall_elapsed={wall_elapsed:?}, observations={observations:?}, result={result:?}",
            observations.len(),
            started.elapsed(),
        );
        let error =
            result.expect_err("a held election whose port does not answer never serves this start");
        assert_eq!(error.slug(), errors::cli::server_start_timed_out::SLUG);
        assert!((2..=SLOW_PROBE_ATTEMPT_COUNT as usize + 1).contains(&observations.len()));
        // The OS sleep guarantees a minimum cost, not an upper bound. The paused
        // cases prove the deadline; this case retains each real cost and decision.
        let mut previous_end = std::time::Duration::ZERO;
        for (start, end, sleep_cost) in observations {
            assert!(start >= previous_end, "probes must complete in order");
            assert!(
                sleep_cost >= SLOW_PROBE_COST,
                "the sleep must spend its requested cost"
            );
            assert!(end <= wall_elapsed, "the wait must include every probe");
            previous_end = end;
        }
        Ok(())
    }

    /// Probes the start makes of a holder whose port refuses. The clock is paused, but a
    /// probe's connect is not: on Windows each refused connect spends the whole connect
    /// timeout, so the stop window's 100 probes would hold this case for most of a minute.
    const REFUSING_HOLDER_ATTEMPT_COUNT: u32 = 3;

    /// A holder that stopped answering its port is on its way out: the start waits for
    /// it to release the election and refuses with its pid when it keeps the election.
    #[tokio::test(start_paused = true)]
    async fn start_waits_for_a_holder_whose_port_refuses_and_names_it_when_it_stays() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let document = ServerLock {
            pid: std::process::id(),
            ..holder_on(dead_port()?)
        };
        guard.publish(&document)?;
        let error = start_detached(directory.path(), REFUSING_HOLDER_ATTEMPT_COUNT)
            .await
            .expect_err("a holder that keeps the election past the stop window refuses the start");
        let unreleased = error.slug() == errors::cli::server_election_unreleased::SLUG
            && error
                .context()
                .any(|(key, value)| key == "pid" && value == document.pid.to_string());
        assert!(unreleased, "{error:?}");
        drop(guard);
        Ok(())
    }

    /// A holder that has not published yet is a server another start elected, still
    /// building: the start spawns nothing and waits for that server's document, as the
    /// start that spawned it does, then answers with the published address. The start's
    /// first probe runs before the paused clock moves, so it always finds the holder
    /// unpublished.
    #[tokio::test(start_paused = true)]
    async fn start_waits_for_a_building_holder_and_answers_with_its_address() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let (_listener, port) = answering_port()?;
        let published = holder_on(port);
        let publish = async {
            tokio::time::sleep(PRESENCE_POLL_INTERVAL * 2).await;
            guard.publish(&published).expect("the holder must publish");
        };
        let start = start_detached(directory.path(), STOP_POLL_ATTEMPT_COUNT);
        let (outcome, ()) = tokio::join!(start, publish);
        let expected = ServerOutcome::Listening {
            port,
            pid: published.pid,
        };
        assert_eq!(outcome?, expected);
        Ok(())
    }

    /// A holder that publishes nothing within the start window is still building when
    /// the window runs out: the start answers that it is starting, with no pid of its
    /// own, and only once the whole window has passed.
    #[tokio::test(start_paused = true)]
    async fn start_reports_a_building_holder_as_starting_once_the_window_runs_out() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        let began = tokio::time::Instant::now();
        assert_eq!(
            start_detached(directory.path(), STOP_POLL_ATTEMPT_COUNT).await?,
            ServerOutcome::Starting { pid: None }
        );
        let waited = began.elapsed();
        assert!(
            waited >= START_WAIT_MAX,
            "the start must wait the whole start window before answering: waited={waited:?}, \
             start_wait_max={START_WAIT_MAX:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn stop_reports_a_building_holder_as_starting() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = rift_mcp::claim(directory.path())?;
        assert_eq!(
            stop(directory.path(), STOP_POLL_ATTEMPT_COUNT).await?,
            ServerOutcome::Starting { pid: None }
        );
        Ok(())
    }

    #[test]
    fn foreground_refusal_maps_election_failures() {
        let directory = tempfile::tempdir().expect("workspace fixture must build");
        let already = foreground_refused(
            directory.path(),
            errors::mcp::election_already_serving().error(),
        );
        assert_eq!(already.slug(), errors::cli::server_already_serving::SLUG);
        let storage = errors::mcp::election_storage_failed()
            .operation("open election file")
            .path(directory.path().join(".rift").join("server.lock"))
            .source(std::io::Error::other("disk gone"))
            .error();
        let passed = foreground_refused(directory.path(), storage);
        assert_eq!(passed.slug(), errors::mcp::election_storage_failed::SLUG);
    }

    /// A holder whose port refuses is shutting down, and the election it still holds
    /// is what the stop waits on: a port that no longer answers is one step of that
    /// wait, never the answer.
    #[tokio::test]
    async fn stop_refuses_while_a_dead_port_holder_keeps_the_election() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        let holder = ServerLock {
            pid: std::process::id(),
            ..holder_on(dead_port()?)
        };
        guard.publish(&holder)?;
        let error = stop(directory.path(), 1)
            .await
            .expect_err("a held election must refuse the stop");
        assert!(
            error.slug() == errors::cli::server_election_unreleased::SLUG
                && error
                    .context()
                    .any(|(key, value)| key == "pid" && value == holder.pid.to_string()),
            "the refusal must name the holder that kept the election: {error:?}"
        );
        assert!(
            rift_mcp::document_path(directory.path()).exists(),
            "the stop leaves the document to its holder"
        );
        Ok(())
    }

    /// A refused connect delivers the stop: nothing listens on the recorded port, so
    /// serving has already ended and only the election is left to wait on.
    #[tokio::test]
    async fn stop_request_treats_a_refused_connect_as_delivered() -> TestResult {
        request_stop(&holder_on(dead_port()?)).await?;
        Ok(())
    }

    /// The stop reports success once the holder releases the election, even though its
    /// port stopped answering first.
    #[tokio::test]
    async fn stop_reports_stopped_once_the_dead_port_holder_releases_the_election() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = rift_mcp::claim(directory.path())?;
        guard.publish(&ServerLock {
            pid: std::process::id(),
            ..holder_on(dead_port()?)
        })?;
        let released = tokio::spawn(async move {
            tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
            drop(guard);
        });
        assert_eq!(
            stop(directory.path(), STOP_POLL_ATTEMPT_COUNT).await?,
            ServerOutcome::Stopped
        );
        released.await?;
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
        let error = stop(directory.path(), STOP_POLL_ATTEMPT_COUNT)
            .await
            .expect_err("a refusing server must fail the stop");
        serving.abort();
        assert!(
            error.slug() == errors::cli::server_stop_refused::SLUG
                && error
                    .context()
                    .any(|(key, value)| key == "status" && value == "500"),
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

    /// A log store on a temporary metrics database, for the reads these cases drive.
    async fn log_store(directory: &tempfile::TempDir) -> TestResult<LogStore> {
        Ok(LogStore::open(&directory.path().join("metrics"), None).await?)
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
            LogsWindow::default(),
            Some(LogLevel::Warn),
            Some("index"),
        );

        assert_eq!(query.limit(), 20);
        assert_eq!(query.level(), Some("warn"));
        assert_eq!(query.component(), Some("index"));
        assert_eq!(
            logs_query(TailCount::All, LogsWindow::default(), None, None).limit(),
            LOG_PAGE_RECORDS_MAX
        );
    }

    /// The tracing clock's reading, in milliseconds since the Unix epoch, every age cutoff
    /// of the logs tests counts back from.
    const CLOCK_MS: i64 = 10_000_000;

    /// A logs read as `rift server logs` builds it, with the tracing clock fixed at
    /// [`CLOCK_MS`].
    fn logs_query_at_fixed_clock(window: LogsWindow) -> LogQuery {
        restricted_query(
            LogQuery::newest(LOG_PAGE_RECORDS_MAX).at_clock_ms(CLOCK_MS),
            window,
            None,
            None,
        )
    }

    /// An `info` record of `index.build` at `recorded_at_ms` carrying `message`.
    fn build_record(recorded_at_ms: i64, message: &str) -> LogRecord {
        LogRecord::new(
            recorded_at_ms,
            "info",
            "rift_mcp::server",
            "index",
            "index.build",
            message,
            "{}",
        )
    }

    /// The messages of `records`, in the order read.
    fn messages(records: &[rift_tracing::StoredLogRecord]) -> Vec<String> {
        records
            .iter()
            .map(|stored| stored.record().message().to_owned())
            .collect()
    }

    /// `--since 10m` selects `recorded_at >= clock - 600_000`: a record at the cutoff is
    /// answered, one a millisecond before it is not.
    #[tokio::test]
    async fn a_logs_since_age_includes_its_cutoff_and_excludes_one_millisecond_before() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let cutoff = CLOCK_MS - 600_000;
        store
            .append(
                [
                    build_record(cutoff - 1, "outside"),
                    build_record(cutoff, "at cutoff"),
                    build_record(cutoff + 1, "inside"),
                ],
                1_000,
            )
            .await?;
        let since = rift_protocol::configuration::Duration::parse("10m")?;
        assert_eq!(since.milliseconds(), 600_000);
        assert!(rift_protocol::configuration::Duration::parse("10").is_err());

        let window = LogsWindow {
            since: Some(LogsBound::Age(since)),
            ..LogsWindow::default()
        };
        let read = store
            .reader()
            .connect()?
            .following(&logs_query_at_fixed_clock(window))?;

        assert_eq!(messages(&read), ["at cutoff", "inside"]);
        Ok(())
    }

    /// `--until 30m` selects `recorded_at < clock - 1_800_000`: a record at the cutoff is
    /// not answered, one a millisecond before it is.
    #[tokio::test]
    async fn a_logs_until_age_excludes_its_cutoff_and_includes_one_millisecond_before() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let cutoff = CLOCK_MS - 1_800_000;
        store
            .append(
                [
                    build_record(cutoff - 1, "inside"),
                    build_record(cutoff, "at cutoff"),
                    build_record(cutoff + 1, "outside"),
                ],
                1_000,
            )
            .await?;
        let window = LogsWindow {
            until: Some(LogsBound::parse("30m")?),
            ..LogsWindow::default()
        };
        let read = store
            .reader()
            .connect()?
            .following(&logs_query_at_fixed_clock(window))?;

        assert_eq!(messages(&read), ["inside"]);
        Ok(())
    }

    /// `--since 2h --until 30m` counts both ages back from one clock reading: the window is
    /// `clock - 7_200_000 <= recorded_at < clock - 1_800_000`, exact at both ends.
    #[tokio::test]
    async fn a_logs_since_and_until_age_select_the_window_between_them() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let since = CLOCK_MS - 7_200_000;
        let until = CLOCK_MS - 1_800_000;
        store
            .append(
                [
                    build_record(since - 1, "before since"),
                    build_record(since, "at since"),
                    build_record(until - 1, "before until"),
                    build_record(until, "at until"),
                ],
                1_000,
            )
            .await?;
        let window = LogsWindow {
            since: Some(LogsBound::parse("2h")?),
            until: Some(LogsBound::parse("30m")?),
        };

        let read = store
            .reader()
            .connect()?
            .following(&logs_query_at_fixed_clock(window))?;

        assert_eq!(messages(&read), ["at since", "before until"]);
        Ok(())
    }

    /// A follow read takes each later page with `after`, and the age cutoff the query
    /// resolved when it was built still holds: a record appended after the first page, one
    /// millisecond before the cutoff, stays out; one at the cutoff prints.
    #[tokio::test]
    async fn a_logs_follow_page_keeps_the_age_cutoff_of_its_query() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let cutoff = CLOCK_MS - 600_000;
        store
            .append([build_record(cutoff + 1, "first")], 1_000)
            .await?;
        let window = LogsWindow {
            since: Some(LogsBound::parse("10m")?),
            ..LogsWindow::default()
        };
        let query = logs_query_at_fixed_clock(window);
        let reads = store.reader().connect()?;
        let first = reads.following(&query)?;
        assert_eq!(messages(&first), ["first"]);
        let newest = first
            .last()
            .map_or(0, rift_tracing::StoredLogRecord::identity);

        store
            .append(
                [
                    build_record(cutoff - 1, "late but old"),
                    build_record(cutoff, "late"),
                ],
                1_000,
            )
            .await?;
        let next = reads.following(&query.clone().after(newest))?;

        assert_eq!(messages(&next), ["late"]);
        Ok(())
    }

    #[tokio::test]
    async fn a_logs_until_instant_selects_records_before_its_bound() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let started = 1_759_600_000_000;
        store
            .append(
                [
                    build_record(started - 1, "before"),
                    build_record(started, "first"),
                    build_record(started + 999, "last"),
                    build_record(started + 1_000, "after"),
                ],
                1_000,
            )
            .await?;
        let window = LogsWindow {
            since: Some(LogsBound::parse("2025-10-04T17:46:40Z")?),
            until: Some(LogsBound::parse("2025-10-04T17:46:41Z")?),
        };
        let read =
            store
                .reader()
                .connect()?
                .following(&logs_query(TailCount::All, window, None, None))?;

        assert_eq!(messages(&read), ["first", "last"]);
        Ok(())
    }

    /// An instant bound becomes its own milliseconds; an age bound is resolved by the query.
    #[test]
    fn a_logs_window_bound_reads_an_age_or_an_rfc_3339_timestamp() {
        assert_eq!(
            LogsBound::parse("10m"),
            Ok(LogsBound::Age(
                rift_protocol::configuration::Duration::from_millis(600_000)
            ))
        );
        let instant = LogsBound::parse("1970-01-01T00:00:01.5Z").expect("an RFC 3339 instant");
        assert_eq!(
            instant.since(LogQuery::newest(1)),
            LogQuery::newest(1).since_ms(1_500)
        );
        assert_eq!(
            instant.until(LogQuery::newest(1)),
            LogQuery::newest(1).until_ms(1_500)
        );
        let refused = LogsBound::parse("yesterday").expect_err("a word is no bound");
        assert!(refused.contains("RFC 3339"), "{refused}");
    }

    #[tokio::test]
    async fn a_workspace_without_a_database_prints_nothing_and_creates_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);

        print_logs(directory.path(), &query, TailCount::All, &LogsMode::Once).await?;

        assert!(
            !directory.path().join(".rift").exists(),
            "a logs read never creates the state directory"
        );
        Ok(())
    }

    /// `rift server logs` needs no server and no index database: with only
    /// `.rift/metrics` present it reads the records and creates nothing else.
    #[tokio::test]
    async fn logs_print_from_the_metrics_database_alone() -> TestResult {
        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        std::fs::create_dir(&state_directory)?;
        let store = LogStore::open(&state_directory.join("metrics"), None).await?;
        store
            .append(
                [LogRecord::new(
                    0,
                    "info",
                    "rift",
                    "index",
                    "index.build",
                    "kept",
                    "{}",
                )],
                1_000,
            )
            .await?;
        store
            .close(tokio::time::Instant::now() + SERVER_STOP_DEADLINE)
            .await?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);

        print_logs(directory.path(), &query, TailCount::All, &LogsMode::Once).await?;
        print_logs(
            directory.path(),
            &query,
            TailCount::Newest(5),
            &LogsMode::Once,
        )
        .await?;

        let mut names: Vec<String> = std::fs::read_dir(&state_directory)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<_, _>>()?;
        names.retain(|name| !name.starts_with("metrics"));
        assert!(
            names.is_empty(),
            "a logs read creates no other state: {names:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_refused_read_keeps_the_store_failure_on_its_source_chain() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let oversized: Vec<LogRecord> = (0..=LOG_BATCH_RECORDS_MAX)
            .map(|_| LogRecord::new(0, "info", "rift", "index", "index.build", "x", "{}"))
            .collect();
        let refused = store
            .append(oversized, 1_000)
            .await
            .expect_err("an oversized batch must be refused");

        let error = errors::cli::server_logs_unavailable()
            .operation("read recorded logs")
            .source(refused)
            .error();

        assert_eq!(error.slug(), errors::cli::server_logs_unavailable::SLUG);
        let rendered = error.to_string();
        assert!(rendered.contains("read recorded logs"), "{rendered}");
        assert!(
            std::error::Error::source(&error).is_some(),
            "the store failure must stay on the source chain"
        );
        Ok(())
    }

    /// A metrics file that is not a database fails each read with the read's operation, and
    /// the store failure stays on the source chain.
    #[tokio::test]
    async fn a_logs_read_the_reader_refuses_names_the_read_and_keeps_its_source() -> TestResult {
        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        std::fs::create_dir(&state_directory)?;
        std::fs::write(state_directory.join("metrics"), b"not a sqlite database")?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);

        for tail in [TailCount::All, TailCount::Newest(5)] {
            let refused = print_logs(directory.path(), &query, tail, &LogsMode::Once)
                .await
                .expect_err("a file that is not a database cannot be read");

            assert_eq!(refused.slug(), errors::cli::server_logs_unavailable::SLUG);
            let rendered = refused.to_string();
            assert!(rendered.contains("read recorded logs"), "{rendered}");
            assert!(
                std::error::Error::source(&refused).is_some(),
                "the store failure must stay on the source chain"
            );
        }
        Ok(())
    }

    /// A read that panics on its blocking thread is reported as an unavailable read.
    #[tokio::test]
    async fn a_read_that_panics_is_reported_as_an_unavailable_read() -> TestResult {
        use super::{LogQuery, LogReads, StoredLogRecord, read_records};

        fn panicking(
            _reads: &LogReads,
            _query: &LogQuery,
        ) -> Result<Vec<StoredLogRecord>, rift_error::RiftError> {
            panic!("injected read failure")
        }

        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);

        let refused = read_records(&store.reader(), query, panicking)
            .await
            .expect_err("a panicking read cannot answer");

        assert_eq!(refused.slug(), errors::cli::server_logs_unavailable::SLUG);
        let rendered = refused.to_string();
        assert!(rendered.contains("read recorded logs"), "{rendered}");
        Ok(())
    }

    /// A follow whose read is refused fails at once rather than waiting to poll again.
    #[tokio::test]
    async fn a_follow_whose_read_is_refused_fails_before_it_waits() -> TestResult {
        use super::{LogLines, LogReader, follow_until_interrupt};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("metrics");
        std::fs::write(&path, b"not a sqlite database")?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);
        let interrupted = tokio_util::sync::CancellationToken::new();

        let refused = follow_until_interrupt(
            &LogReader::new(&path),
            &query,
            0,
            &interrupted,
            &mut LogLines::live_stream(),
        )
        .await
        .expect_err("a file that is not a database cannot be followed");

        assert_eq!(refused.slug(), errors::cli::server_logs_unavailable::SLUG);
        Ok(())
    }

    /// An interrupt that arrives inside the follow loop ends the follow once the read in
    /// flight answers. The first poll enters the loop and returns pending, on the read or on
    /// the poll interval, so the interrupt always lands inside the loop.
    #[tokio::test]
    async fn a_follow_ends_after_the_read_in_flight_when_interrupted() -> TestResult {
        use super::{LogLines, follow_until_interrupt};
        use std::task::{Context, Waker};

        let directory = tempfile::tempdir()?;
        let store = log_store(&directory).await?;
        let followed = LogRecord::new(0, "info", "rift", "index", "index.build", "followed", "{}");
        store.append([followed], 1_000).await?;
        let reader = store.reader();
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);
        let interrupted = tokio_util::sync::CancellationToken::new();
        let mut lines = LogLines::live_stream();
        let mut follow = Box::pin(follow_until_interrupt(
            &reader,
            &query,
            0,
            &interrupted,
            &mut lines,
        ));
        let mut context = Context::from_waker(Waker::noop());
        let first_poll = std::future::Future::poll(follow.as_mut(), &mut context);
        assert!(
            first_poll.is_pending(),
            "the loop waits on a read or on its interval"
        );

        interrupted.cancel();

        follow.await?;
        Ok(())
    }

    /// `--follow` prints until the operator interrupts, so a followed read has not ended
    /// when a bound on it elapses. The clock is paused: it advances only while no read is
    /// on a blocking thread, so the bound elapses inside the poll interval of the follow
    /// loop, after the first page printed, on every run.
    #[tokio::test(start_paused = true)]
    async fn a_followed_read_ends_only_on_the_interrupt() -> TestResult {
        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        std::fs::create_dir(&state_directory)?;
        let store = LogStore::open(&state_directory.join("metrics"), None).await?;
        let kept = LogRecord::new(0, "info", "rift", "index", "index.build", "kept", "{}");
        store.append([kept], 1_000).await?;
        let query = logs_query(TailCount::All, LogsWindow::default(), None, None);
        let following = print_logs(
            directory.path(),
            &query,
            TailCount::Newest(5),
            &LogsMode::Following,
        );

        let followed = tokio::time::timeout(std::time::Duration::from_secs(2), following).await;

        assert!(followed.is_err(), "a follow ends only on the interrupt");
        Ok(())
    }
}
