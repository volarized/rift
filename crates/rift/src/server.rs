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
use std::time::Duration;

use rift_core::{CliCode, Error, ErrorContext, ErrorName, Fault};
use rift_mcp::{
    ElectionError, ElectionFault, PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX,
    ServerPresence, StaleReason, probe, read_serving, serve_elected, spawn_detached_server,
};
use rift_protocol::lock::ServerLock;
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

/// Failure while running one `rift server` command.
pub(super) type ServerCommandError = Error<ServerCommandFault>;

/// One server-command failure: what kept the workspace's server from
/// reaching the asked state.
#[derive(Debug)]
pub(super) enum ServerCommandFault {
    /// A rift server already serves this workspace, refusing a foreground
    /// start. Carries the holder's document when the probe could read one.
    AlreadyServing { holder: Option<ServerLock> },
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
    StopTimedOut { holder: ServerLock },
    /// The election refused or failed while serving in the foreground.
    Election(ElectionError),
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
            Self::Election(source) => source.name(),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::AlreadyServing { holder } => holder_evidence(holder.as_ref()),
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
                evidence.extend(holder_evidence(Some(holder)));
                evidence
            }
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
                    "rift server listening on 127.0.0.1:{port} (pid {pid})"
                )
            }
            Self::AlreadyListening { port, pid } => write!(
                formatter,
                "rift server already listening on 127.0.0.1:{port} (pid {pid})"
            ),
            Self::Stopped => formatter.write_str("rift server stopped"),
            Self::NotRunning => formatter.write_str("no rift server is running for this workspace"),
            Self::Serving { port, pid, version } => write!(
                formatter,
                "rift server listening on 127.0.0.1:{port} (pid {pid}, v{version})"
            ),
            Self::Stale { reason } => write!(
                formatter,
                "found a stale .rift/server.json ({reason}); the next rift mcp or rift server start replaces it"
            ),
        }
    }
}

/// Runs one `rift server` command against the current directory's
/// workspace.
///
/// A foreground start prints its listening line itself before blocking and
/// completes with nothing left to print; every other command completes
/// with its outcome line.
///
/// # Errors
///
/// Returns [`ServerCommandError`] when the asked state was not reached.
pub(super) async fn run(
    command: ServerCommand,
) -> Result<Option<ServerOutcome>, ServerCommandError> {
    let root = Path::new(".");
    match command {
        ServerCommand::Start { foreground } => match start_mode(foreground) {
            StartMode::Detached => start_detached(root).await.map(Some),
            StartMode::Foreground => serve_foreground(root).await.map(|()| None),
        },
        ServerCommand::Stop => stop(root).await.map(Some),
        ServerCommand::Restart => restart(root).await.map(Some),
        ServerCommand::Status => Ok(Some(status(root))),
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
            version: lock.version,
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
    spawn_detached_server(root)
        .map_err(|source| Error::new(ServerCommandFault::SpawnFailed { source }))?;
    await_serving(root, START_POLL_ATTEMPT_COUNT).await
}

/// Polls until the workspace serves, bounded by `attempt_count` probes.
///
/// The caller passes [`START_POLL_ATTEMPT_COUNT`], which derives from
/// [`START_WAIT_MAX`] over the poll interval.
async fn await_serving(
    root: &Path,
    attempt_count: u32,
) -> Result<ServerOutcome, ServerCommandError> {
    for _ in 0..attempt_count {
        if let Some(lock) = read_serving(root) {
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
/// same way.
async fn serve_foreground(root: &Path) -> Result<(), ServerCommandError> {
    let shutdown = CancellationToken::new();
    let server = match serve_elected(root, shutdown.clone()).await {
        Ok(server) => server,
        Err(error) => return Err(foreground_refused(root, error)),
    };
    println!(
        "{}",
        ServerOutcome::Listening {
            port: server.port(),
            pid: std::process::id(),
        }
    );
    tokio::spawn(cancel_on_interrupt(shutdown));
    server
        .stopped()
        .await
        .map_err(|error| Error::new(ServerCommandFault::Election(error)))
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
            holder: read_serving(root),
        });
    }
    Error::new(ServerCommandFault::Election(error))
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
    Err(Error::new(ServerCommandFault::StopTimedOut { holder }))
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

#[cfg(test)]
pub(super) fn error_for_test() -> ServerCommandError {
    Error::new(ServerCommandFault::StartTimedOut)
}

#[cfg(test)]
mod tests {
    use std::future::IntoFuture as _;
    use std::net::Ipv4Addr;

    use super::{
        PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, STOP_POLL_ATTEMPT_COUNT,
        STOP_WAIT_MAX, ServerCommandFault, ServerOutcome, StaleReason, StartMode, await_serving,
        await_stopped, discard_stale_document, foreground_refused, holder_evidence,
        stale_reason_phrase, start_mode, status, stop,
    };
    use rift_core::Error;
    use rift_protocol::lock::{ServerLock, ServerLockViolation};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn holder() -> ServerLock {
        ServerLock {
            port: 12_345,
            token: "a".repeat(rift_protocol::lock::SERVER_TOKEN_LENGTH),
            pid: 4_242,
            version: "0.0.11".to_owned(),
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

    #[test]
    fn outcomes_print_the_operator_lines() {
        assert_eq!(
            ServerOutcome::Listening {
                port: 12_345,
                pid: 42
            }
            .to_string(),
            "rift server listening on 127.0.0.1:12345 (pid 42)"
        );
        assert_eq!(
            ServerOutcome::AlreadyListening {
                port: 12_345,
                pid: 42
            }
            .to_string(),
            "rift server already listening on 127.0.0.1:12345 (pid 42)"
        );
        assert_eq!(ServerOutcome::Stopped.to_string(), "rift server stopped");
        assert_eq!(
            ServerOutcome::NotRunning.to_string(),
            "no rift server is running for this workspace"
        );
        assert_eq!(
            ServerOutcome::Serving {
                port: 12_345,
                pid: 42,
                version: "0.0.11".to_owned(),
            }
            .to_string(),
            "rift server listening on 127.0.0.1:12345 (pid 42, v0.0.11)"
        );
        assert_eq!(
            ServerOutcome::Stale {
                reason: "no process holds the election lock"
            }
            .to_string(),
            "found a stale .rift/server.json (no process holds the election lock); \
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
                    holder: Some(holder()),
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
                Error::new(ServerCommandFault::StopTimedOut { holder: holder() }),
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
            holder: Some(holder()),
        })
        .to_string();
        assert!(already.contains("127.0.0.1:12345"), "{already}");
        assert!(already.contains("pid 4242"), "{already}");
        assert!(already.contains("rift server stop"), "{already}");

        let unpublished =
            Error::new(ServerCommandFault::AlreadyServing { holder: None }).to_string();
        assert!(unpublished.contains("has not published"), "{unpublished}");

        let timed_out = Error::new(ServerCommandFault::StartTimedOut).to_string();
        assert!(timed_out.contains("15s"), "{timed_out}");
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

        let stop_timed_out =
            Error::new(ServerCommandFault::StopTimedOut { holder: holder() }).to_string();
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
        let error = Error::new(ServerCommandFault::Election(election));
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
        let error = await_serving(directory.path(), 1)
            .await
            .expect_err("a workspace nobody serves must time the wait out");
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
}
