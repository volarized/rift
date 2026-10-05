//! Streamable-HTTP transport: loopback serving behind a minted bearer token.

use std::future::IntoFuture as _;
use std::net::Ipv4Addr;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use data_encoding::BASE64URL_NOPAD;
use rift_error::{RiftError, errors};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::ServerConfiguration;
use rift_protocol::lock::{ProductIdentity, ServerLock};
use rift_search::SearchIndex;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::RiftMcp;
use crate::identity::BuildCheckout;
use crate::repository_http::RepositoryWorkspaceRegistry;
pub(crate) use crate::repository_http::serve_repository_http;
use crate::server::EngineHold;
use crate::storage::WorkspaceStorage;
use crate::validation::IndexSupervisor;

/// A route under the server's one API base path, spelled here once.
macro_rules! api_path {
    ($segment:literal) => {
        concat!("/api", $segment)
    };
}

/// Path the MCP Streamable-HTTP service is mounted at.
pub(crate) const MCP_PATH: &str = api_path!("/mcp");
/// Path an authorized `POST` stops the server through.
pub(crate) const STOP_PATH: &str = api_path!("/stop");
/// Header selecting one workspace behind a repository server.
pub(crate) const WORKSPACE_ROOT_HEADER: &str = "x-rift-workspace-root";
/// Bytes of entropy behind one minted bearer token.
const TOKEN_ENTROPY_BYTES: usize = 32;
/// The authentication scheme: the `WWW-Authenticate` refusal names it, and
/// an accepted `Authorization` value carries it before the single space.
const BEARER_SCHEME: &str = "Bearer";
/// The `Host` spellings that name the loopback the server binds.
const LOOPBACK_HOSTS: &[&str] = &["127.0.0.1", "localhost", "[::1]"];

/// Whether the served routes check the minted bearer token.
///
/// The token separates OS users sharing the machine, and the loopback bind
/// is the network boundary under either value. `Skipped` drops the token
/// check for one run, which the MCP conformance runner needs: it addresses
/// a server by URL alone and sends no `Authorization` header. The CLI
/// accepts `--auth skip` only beside `--foreground`, so no detached server
/// ever serves unchecked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TokenCheck {
    /// Every request presents `Bearer` followed by the minted token.
    #[default]
    Required,
    /// Every request that clears the loopback boundary is served, whatever
    /// it presents.
    Skipped,
}

impl TokenCheck {
    /// Whether one request's `Authorization` value passes this policy.
    fn accepts(self, authorization: Option<&str>, token: &str) -> bool {
        match self {
            Self::Required => bearer_authorized(authorization, token),
            Self::Skipped => true,
        }
    }
}

/// Serves the workspace at `root` over loopback Streamable HTTP, behind
/// `check`.
///
/// The workspace builds exactly as stdio serving does - an invalid
/// `rift.toml` still serves, refusing each request as
/// `configuration_invalid` under default policies - then a bearer token is
/// minted and the first free port of the accepted `[server]` selection is
/// bound: the pinned `port`, the configured `port_range`, or the default
/// serving range. The token is minted and published under either `check`,
/// so `rift mcp` reaches the server the same way.
/// The returned handle's listener is already accepting. Serving ends when
/// `shutdown` cancels, an authorized `POST /api/stop` arrives, or the
/// accepted `server.idle_timeout` passes after the last authorized request
/// completes while no request remains active.
///
/// # Errors
///
/// Returns [`RiftError`] when the workspace cannot be indexed, entropy
/// for the token is unavailable, every port in the serving range is bound,
/// or the listener cannot register with the runtime.
///
/// # Cancel safety
///
/// Dropping this future discards construction; an accepted initial index
/// scan still finishes in the bounded blocking executor. A returned
/// [`HttpServer`] owns the serving tasks and is driven through
/// [`HttpServer::stopped`].
///
/// The server names itself as [`BuildCheckout::Unversioned`], as
/// [`RiftMcp::build`] does.
pub async fn serve_http(
    root: &Path,
    shutdown: CancellationToken,
    check: TokenCheck,
) -> Result<HttpServer, RiftError> {
    let storage = WorkspaceStorage::open(root).await;
    serve_http_with_storage(
        root,
        shutdown,
        storage,
        WorkspaceIndexLimits::default(),
        check,
        BuildCheckout::Unversioned,
    )
    .await
}

/// Serves HTTP through storage already opened by the serving process, under
/// explicit index bounds and one token policy, naming the build `checkout`
/// describes.
pub(crate) async fn serve_http_with_storage(
    root: &Path,
    shutdown: CancellationToken,
    storage: WorkspaceStorage,
    limits: WorkspaceIndexLimits,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<HttpServer, RiftError> {
    rift_tracing::info!(
        component = "mcp",
        transport = "http",
        phase = "start",
        "MCP server starting"
    );
    let logs = storage.logs();
    let server = RiftMcp::build_with_storage(root, limits, storage, checkout).await?;
    let identity = server.product_identity().clone();
    let search_index = server.search_index_handle();
    let server_table = server.server_configuration().await;
    let idle_timeout = Duration::from_millis(server_table.idle_timeout.milliseconds());
    let supervisor = server.index_supervisor();
    let engines = server.engine_hold();
    let token = mint_token()?;
    let (port, listener) = bind_loopback_listener(server_table.serving_ports())?;
    let stop = shutdown.child_token();
    let idle = server.request_activity();
    if matches!(check, TokenCheck::Skipped) {
        rift_tracing::warn!(
            component = "mcp",
            transport = "http",
            "the bearer token check is off for this run: every loopback request is answered"
        );
    }
    let router = authenticated_router(server, &token, check, &stop, &idle);
    let serving = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .into_future(),
    );
    let idle_watch = tokio::spawn(watch_idle(idle, idle_timeout, stop.clone()));
    rift_tracing::info!(
        component = "mcp",
        transport = "http",
        port,
        outcome = "ok",
        "MCP server ready"
    );
    Ok(HttpServer {
        port,
        token,
        identity,
        server_configuration: server_table,
        stop,
        serving,
        idle_watch,
        repository_idle_watch: None,
        supervisor: Some(supervisor),
        engines: Some(engines),
        search_index,
        logs,
        repository_workspaces: None,
    })
}

/// One serving HTTP MCP server: its address facts and its serving tasks.
#[derive(Debug)]
pub struct HttpServer {
    pub(crate) port: u16,
    pub(crate) token: String,
    pub(crate) identity: ProductIdentity,
    pub(crate) server_configuration: ServerConfiguration,
    pub(crate) stop: CancellationToken,
    pub(crate) serving: JoinHandle<Result<(), std::io::Error>>,
    pub(crate) idle_watch: JoinHandle<()>,
    pub(crate) repository_idle_watch: Option<JoinHandle<()>>,
    pub(crate) supervisor: Option<IndexSupervisor>,
    pub(crate) engines: Option<Arc<EngineHold>>,
    pub(crate) search_index: Option<Arc<rift_search::SearchIndex>>,
    pub(crate) logs: Option<Arc<rift_tracing::LogStore>>,
    pub(crate) repository_workspaces: Option<Arc<RepositoryWorkspaceRegistry>>,
}

/// The index and vectors databases and the metrics database of a stopped server, closed
/// after its serving stopped.
///
/// A caller with a log drain closes them in two steps around the drain's final flush:
/// [`Self::close_search`] first, so a close failure is recorded and the flush writes it,
/// then [`Self::close_logs`] last, so the metrics database outlives every record.
#[doc(hidden)]
pub struct DeferredDatabaseShutdown(
    Option<Arc<SearchIndex>>,
    Option<Arc<rift_tracing::LogStore>>,
);

impl DeferredDatabaseShutdown {
    /// Closes the index database, and the vectors database when it opened, then the
    /// metrics database, all by the shared stop deadline.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] if a SQLite worker or the metrics writer thread fails or
    /// cannot stop before the deadline.
    pub async fn shutdown(mut self, deadline: Instant) -> Result<(), RiftError> {
        let search = self.close_search(deadline).await;
        let logs = self.close_logs(deadline).await;
        search.and(logs)
    }

    /// Closes the index database, and the vectors database when it opened, by `deadline`.
    ///
    /// A failure is recorded as a `warn` record of the `storage` component, operation
    /// `database.close`, so a log drain still running writes it to the metrics database.
    /// A second call closes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] if a SQLite worker fails or cannot stop before `deadline`.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future leaves the workers to stop on their own; the databases are not
    /// closed again.
    pub async fn close_search(&mut self, deadline: Instant) -> Result<(), RiftError> {
        let Some(search_index) = self.0.take() else {
            return Ok(());
        };
        stop_stage("SQLite worker shutdown", deadline, async {
            search_index.shutdown(deadline).await.map_err(|error| {
                rift_tracing::warn!(
                    component = "storage",
                    operation = "database.close",
                    %error,
                    "the index or vectors database did not close"
                );
                errors::mcp::http_serve_failed()
                    .operation("SQLite worker shutdown")
                    .cause(error)
                    .error()
            })
        })
        .await
    }

    /// Closes the metrics database by `deadline`, when it opened.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] if the metrics writer thread fails or outlasts `deadline`.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the close is queued leaves the writer thread to close.
    pub async fn close_logs(self, deadline: Instant) -> Result<(), RiftError> {
        close_logs(self.1.as_deref(), deadline).await
    }
}

/// Runs one stage of a server stop inside a `server.stop` span and records how it ended.
///
/// The span's close carries the stage's elapsed time. The `stop stage ended` record
/// carries the stage's name, what it left of the stop's shared `deadline`, its outcome,
/// and, for a failure, the error and its causes, so a stop that leaves with a failure
/// names the stage that returned it. The outcome is `ok` for a stage that succeeded inside
/// `deadline`, `timeout` at `warn` for one that succeeded with nothing of it left, such as
/// a database close whose checkpoint outlasted it, and `error` at `warn` for a failure;
/// only a failure reaches the caller as an error. The span records its opening too, so a
/// stage the process never finishes still names itself, and the stage the deadline
/// expired in publishes the table of operations in flight with the reason `stop deadline`.
///
/// # Errors
///
/// Returns the error `work` returned, unchanged.
///
/// # Cancel safety
///
/// Dropping the future drops `work` and records nothing.
#[doc(hidden)]
pub async fn stop_stage<Value>(
    stage: &'static str,
    deadline: Instant,
    work: impl std::future::Future<Output = Result<Value, RiftError>>,
) -> Result<Value, RiftError> {
    rift_tracing::traced!(
        component = "mcp",
        operation = "server.stop",
        open = true,
        stage = stage,
        async move {
            let deadline_ahead = Instant::now() < deadline;
            let outcome = work.await;
            let remaining = deadline.saturating_duration_since(Instant::now());
            // The stage the shared deadline expired in publishes the operations still open:
            // what it, and every stage after it, met unfinished. A stage that starts past
            // the deadline publishes nothing, so one stop publishes once.
            if deadline_ahead && remaining.is_zero() {
                rift_tracing::warn_in_flight("stop deadline");
            }
            match &outcome {
                Ok(_) if remaining.is_zero() => rift_tracing::warn!(
                    component = "mcp",
                    operation = "server.stop",
                    stage,
                    ?remaining,
                    outcome = "timeout",
                    "stop stage ended"
                ),
                Ok(_) => rift_tracing::info!(
                    component = "mcp",
                    operation = "server.stop",
                    stage,
                    ?remaining,
                    outcome = "ok",
                    "stop stage ended"
                ),
                Err(error) => {
                    let causes = rift_error::causes(error).join(": ");
                    rift_tracing::warn!(
                        component = "mcp",
                        operation = "server.stop",
                        stage,
                        ?remaining,
                        outcome = "error",
                        %error,
                        causes,
                        "stop stage ended"
                    );
                }
            }
            outcome
        }
    )
    .await
}

/// Closes the metrics database by `deadline`, when it opened.
///
/// The close runs as a `database.close` operation with `open = true`, so its opening and
/// its end are both recorded. A deadline that passes in any close stage does not fail
/// the stage: it is recorded as a `warn` `database.close` record naming the close stage and
/// its time, and the next open recovers the log.
pub(crate) async fn close_logs(
    logs: Option<&rift_tracing::LogStore>,
    deadline: Instant,
) -> Result<(), RiftError> {
    match logs {
        Some(logs) => {
            stop_stage("metrics database close", deadline, async {
                // The close runs as a `database.close` operation: it records its opening,
                // sits in the table of operations in flight while the writer thread runs
                // it, and ends with its outcome.
                rift_tracing::traced!(
                    component = "storage",
                    operation = "database.close",
                    database = "metrics",
                    open = true,
                    async { logs.close(deadline).await }
                )
                .await
                .map(|closed| match closed {
                    rift_tracing::StoreClose::Closed(checkpoint) => rift_tracing::info!(
                        component = "storage",
                        operation = "database.close",
                        database = "metrics",
                        busy = checkpoint.is_busy(),
                        log = checkpoint.log(),
                        checkpointed = checkpoint.checkpointed(),
                        elapsed_ms = elapsed_ms(checkpoint.elapsed()),
                        "database checkpointed its write-ahead log"
                    ),
                    rift_tracing::StoreClose::Timeout { stage, elapsed } => {
                        rift_tracing::warn!(
                            component = "storage",
                            operation = "database.close",
                            database = "metrics",
                            stage,
                            elapsed_ms = elapsed_ms(elapsed),
                            "database checkpoint outlasted the shutdown deadline; the \
                                 write-ahead log stays for the next open"
                        );
                    }
                })
                .map_err(|error| {
                    errors::mcp::http_serve_failed()
                        .operation("metrics database close")
                        .cause(error)
                        .error()
                })
            })
            .await
        }
        None => Ok(()),
    }
}

impl HttpServer {
    /// The loopback port the server accepts requests on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The bearer token every request must present.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Exact identity advertised by this server.
    #[must_use]
    pub(crate) fn product_identity(&self) -> &ProductIdentity {
        &self.identity
    }

    /// Accepted server settings this process keeps until restart.
    #[must_use]
    pub(crate) fn server_configuration(&self) -> &ServerConfiguration {
        &self.server_configuration
    }

    /// Waits until the server stopped and its index supervisor shut down,
    /// bounded by `budget` from the moment the stop began.
    ///
    /// Resolves after the stop began - the external shutdown token, an
    /// authorized `POST /api/stop`, or the idle timeout cancels this server's
    /// token - or after the serve loop ended on its own I/O failure, which
    /// cancels nothing. The deadline is derived there, so the span the server
    /// spent listening never spends it. The serving task's drain of the
    /// requests still in flight, the engines' shutdown, and the index
    /// supervisor's join all run under that one deadline: each stage takes
    /// only what the one before it left, and a stage that outlasts it is not
    /// waited on. The drain takes its share of the same budget because the
    /// process leaves either way; reported unfinished, it tells the caller
    /// that requests were still running when the budget ran out and that
    /// those callers get no answer, while the election releases and the
    /// process exits on time. The returned deadline is that same instant, so
    /// the caller's later stop stages share it.
    ///
    /// The retired supervisor's published index is released on the blocking
    /// pool after shutdown. Its data can take longer to free than serving took
    /// to stop; the runtime owns that release while the caller completes its
    /// later stop stages.
    ///
    /// A supervisor still running at the deadline is aborted and its stage ends
    /// with the outcome `timeout`, which fails nothing.
    ///
    /// # Errors
    ///
    /// The second tuple element carries [`RiftError`] when the drain outlasted
    /// the deadline, or a serving task or the supervisor failed.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future detaches the serving tasks; a shutdown already
    /// triggered still completes in the background.
    pub async fn stopped(self, budget: Duration) -> (Instant, Result<(), RiftError>) {
        let (deadline, stopped, database) =
            self.stopped_before_database(budget, Duration::ZERO).await;
        let database = database.shutdown(deadline).await;
        (deadline, stopped.and(database))
    }

    /// Stops serving and index lanes, leaving SQLite open for final log writes.
    ///
    /// As the stop begins it publishes the table of operations in flight with the reason
    /// `stop`, so the store holds what was still running when the stop arrived.
    ///
    /// Every stage here ends by `reserve` before the returned deadline: the caller's
    /// database close and final log flush keep that share of `budget` however long a stage
    /// here runs. The stages' bound is `budget - reserve` from where the stop began,
    /// saturating at zero, so a `reserve` at or past `budget` leaves these stages nothing
    /// and never moves the bound before the stop began. A supervisor still running at
    /// that bound is aborted, and its stage ends with the outcome `timeout` and the table
    /// of operations in flight, the form a close checkpoint that outlasts its bound uses.
    #[doc(hidden)]
    pub async fn stopped_before_database(
        self,
        budget: Duration,
        reserve: Duration,
    ) -> (Instant, Result<(), RiftError>, DeferredDatabaseShutdown) {
        let mut serving = self.serving;
        let ended_before_the_stop = tokio::select! {
            outcome = &mut serving => Some(outcome),
            () = self.stop.cancelled() => None,
        };
        // The stop's deadline starts where the stop began: one derived where
        // the server began listening is already spent when the stop arrives.
        let stop_began = Instant::now();
        let deadline = stop_began + budget;
        let stages_deadline = stop_began + budget.saturating_sub(reserve);
        rift_tracing::info!(
            component = "mcp",
            operation = "server.stop",
            ?budget,
            "MCP server stopping"
        );
        rift_tracing::publish_in_flight("stop");
        // The serve loop can end on its own I/O error, where nothing has
        // cancelled the token yet; cancelling here unblocks the idle watch
        // on every path.
        self.stop.cancel();
        let serve_result =
            stopped_serving(ended_before_the_stop, &mut serving, stages_deadline).await;
        let idle_outcome = stop_stage("idle watch task", stages_deadline, async {
            self.idle_watch.await.map_err(|error| {
                errors::mcp::http_serve_failed()
                    .operation("idle watch task")
                    .source(error)
                    .error()
            })
        })
        .await;
        let repository_idle_outcome = match self.repository_idle_watch {
            Some(watch) => stopped_repository_idle_watch(watch, stages_deadline).await,
            None => Ok(()),
        };
        let engines_stopped = match self.engines {
            Some(engines) => stop_stage("engines shutdown", stages_deadline, async {
                tokio::time::timeout_at(stages_deadline, engines.shutdown())
                    .await
                    .map_err(|error| {
                        errors::mcp::http_serve_failed()
                            .operation("engines shutdown")
                            .source(error)
                            .error()
                    })
            })
            .await
            .is_ok(),
            None => true,
        };
        let supervisor_outcome = if let Some(supervisor) = self.supervisor.as_ref() {
            // An aborted supervisor fails nothing: its stage ends `timeout` with nothing of
            // `stages_deadline` left, and the stop goes on to the caller's database close
            // and log flush.
            stop_stage("index supervisor shutdown", stages_deadline, async {
                supervisor.joined_by(stages_deadline).await.map(|_join| ())
            })
            .await
        } else {
            Ok(())
        };
        let repository_outcome = match self.repository_workspaces {
            Some(registry) => {
                stop_stage(
                    "repository workspaces shutdown",
                    stages_deadline,
                    registry.shutdown(stages_deadline),
                )
                .await
            }
            None => Ok(()),
        };
        // The last supervisor can own the whole published index. Freeing it here
        // would block this future before the caller drains logs and releases its
        // election. Tokio owns the detached release; an embedded runtime reclaims
        // it, and the foreground CLI already exits without waiting on its pool.
        if let Some(supervisor) = self.supervisor {
            drop(tokio::task::spawn_blocking(move || drop(supervisor)));
        }
        let outcome = stop_outcome_label(
            supervisor_outcome.is_ok()
                && repository_outcome.is_ok()
                && serve_result.is_ok()
                && engines_stopped,
        );
        rift_tracing::info!(
            component = "mcp",
            transport = "http",
            outcome,
            "MCP server stopped"
        );
        let stopped = supervisor_outcome
            .and(repository_outcome)
            .and(repository_idle_outcome)
            .and(serve_result)
            .and(idle_outcome);
        (
            deadline,
            stopped,
            DeferredDatabaseShutdown(self.search_index, self.logs),
        )
    }
}

/// The serving stage of a stop: the serve loop's own outcome when it ended before the
/// stop, or the drain of the requests still in flight, by `deadline`.
async fn stopped_serving(
    ended_before_the_stop: Option<Result<Result<(), std::io::Error>, tokio::task::JoinError>>,
    serving: &mut JoinHandle<Result<(), std::io::Error>>,
    deadline: Instant,
) -> Result<(), RiftError> {
    if let Some(outcome) = ended_before_the_stop {
        return stop_stage("http serve loop", deadline, async {
            classify_serve_outcome(outcome)
        })
        .await;
    }
    stop_stage(
        "http serve drain",
        deadline,
        drained_serve_outcome(serving, deadline),
    )
    .await
}

/// Joins the repository idle watch by `deadline`, aborting a watch that outlasts it.
async fn stopped_repository_idle_watch(
    mut watch: JoinHandle<()>,
    deadline: Instant,
) -> Result<(), RiftError> {
    match tokio::time::timeout_at(deadline, &mut watch).await {
        Ok(outcome) => outcome.map_err(|error| {
            errors::mcp::http_serve_failed()
                .operation("workspace idle watch task")
                .source(error)
                .error()
        }),
        Err(error) => {
            watch.abort();
            let _ = watch.await;
            errors::mcp::http_serve_failed()
                .operation("workspace idle watch task")
                .source(error)
                .fail()
        }
    }
}

/// Maps the joined serve loop outcome onto the transport failure taxonomy:
/// the loop's own I/O failure names the serve loop, a panicked or aborted
/// serving task names the task.
fn classify_serve_outcome(
    outcome: Result<Result<(), std::io::Error>, tokio::task::JoinError>,
) -> Result<(), RiftError> {
    match outcome {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => errors::mcp::http_serve_failed()
            .operation("http serve loop")
            .source(error)
            .fail(),
        Err(error) => errors::mcp::http_serve_failed()
            .operation("http serve task")
            .source(error)
            .fail(),
    }
}

/// Joins the serving task by `deadline`, the stop's shared deadline.
///
/// The drain runs the requests still in flight when the stop began, so it
/// takes only what `deadline` leaves it: the process exits whether or not
/// those requests finished, and every later stage of the stop - the engines,
/// the index supervisor, the log drain, and the election's release - waits
/// behind this one. A drain that outlasts the deadline is reported unfinished
/// and left to the process's own exit.
async fn drained_serve_outcome(
    serving: &mut JoinHandle<Result<(), std::io::Error>>,
    deadline: Instant,
) -> Result<(), RiftError> {
    match tokio::time::timeout_at(deadline, serving).await {
        Ok(joined) => classify_serve_outcome(joined),
        Err(elapsed) => errors::mcp::http_serve_failed()
            .operation("http serve drain")
            .source(elapsed)
            .fail(),
    }
}

/// The stop log's outcome field for whether every part shut down cleanly.
/// `elapsed` in whole milliseconds, as the `elapsed_ms` field of a record.
fn elapsed_ms(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn stop_outcome_label(stopped_cleanly: bool) -> &'static str {
    if stopped_cleanly { "ok" } else { "error" }
}

/// Mints one bearer token: 32 random bytes as unpadded base64url.
///
/// The encoding fixes the length: `TOKEN_ENTROPY_BYTES` entropy bytes
/// spell exactly [`rift_protocol::lock::SERVER_TOKEN_LENGTH`] base64url
/// characters.
pub(crate) fn mint_token() -> Result<String, RiftError> {
    let mut entropy = [0_u8; TOKEN_ENTROPY_BYTES];
    getrandom::fill(&mut entropy).map_err(|error| {
        errors::mcp::http_serve_failed()
            .operation("token mint")
            .source(error)
            .error()
    })?;
    Ok(BASE64URL_NOPAD.encode(&entropy))
}

/// The first port in `ports` that `bind` accepts, with what it bound.
///
/// A port `bind` refuses for any reason is skipped. The walk is bounded by
/// the range itself; the typed failure names the exhausted range when every
/// port refused.
fn bind_first_free<Listener>(
    ports: RangeInclusive<u16>,
    mut bind: impl FnMut(u16) -> std::io::Result<Listener>,
) -> Result<(u16, Listener), RiftError> {
    let (port_min, port_max) = (*ports.start(), *ports.end());
    for port in ports {
        if let Ok(listener) = bind(port) {
            return Ok((port, listener));
        }
    }
    errors::mcp::http_ports_exhausted()
        .port_min(port_min)
        .port_max(port_max)
        .fail()
}

/// Binds the first free loopback port of the accepted selection for the
/// runtime.
pub(crate) fn bind_loopback_listener(
    ports: RangeInclusive<u16>,
) -> Result<(u16, tokio::net::TcpListener), RiftError> {
    let (port, listener) = bind_first_free(ports, |port| {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
    })?;
    listener.set_nonblocking(true).map_err(|error| {
        errors::mcp::http_serve_failed()
            .operation("listener nonblocking mode")
            .source(error)
            .error()
    })?;
    let listener = tokio::net::TcpListener::from_std(listener).map_err(|error| {
        errors::mcp::http_serve_failed()
            .operation("listener runtime registration")
            .source(error)
            .error()
    })?;
    Ok((port, listener))
}

/// Assembles the routes over one served workspace, behind `check` and the
/// loopback boundary.
///
/// Requests run statelessly: the service clones the server per request, and
/// no session is ever created.
fn authenticated_router(
    server: RiftMcp,
    token: &str,
    check: TokenCheck,
    stop: &CancellationToken,
    idle: &Arc<IdleTracker>,
) -> Router {
    let mcp_service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_cancellation_token(stop.child_token()),
    );
    let gate = RequestGate {
        token: Arc::from(token),
        check,
        idle: Arc::clone(idle),
    };
    Router::new()
        .nest_service(MCP_PATH, mcp_service)
        .route(STOP_PATH, post(stop_server))
        .with_state(stop.clone())
        .layer(middleware::from_fn_with_state(gate, authorize_request))
        .layer(middleware::from_fn(guard_loopback_boundary))
}

/// Refuses requests whose `Host` or `Origin` reaches past the loopback.
///
/// A remote page can reach a loopback port through DNS rebinding - its own
/// name resolving here - or issue a request carrying its own origin. The
/// server accepts only a loopback `Host`, and only a loopback `Origin` when
/// one is present; non-browser MCP clients omit `Origin` and pass. The
/// guard runs before authentication on every route, so the boundary holds
/// for the stop route as well as the MCP service.
pub(crate) async fn guard_loopback_boundary(request: Request, next: Next) -> Response {
    let headers = request.headers();
    let host_accepted = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(host_is_loopback);
    if !host_accepted {
        return boundary_refusal("Host");
    }
    let origin_accepted = match headers.get(header::ORIGIN) {
        None => true,
        Some(value) => value.to_str().is_ok_and(origin_is_loopback),
    };
    if !origin_accepted {
        return boundary_refusal("Origin");
    }
    next.run(request).await
}

/// The `400` refusal naming the header that reached past the loopback.
fn boundary_refusal(header_name: &'static str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        format!("{header_name} header does not name the loopback this server binds"),
    )
        .into_response()
}

/// Whether a `Host` value names the loopback, with or without a port.
///
/// A bracketed IPv6 host keeps its brackets; otherwise a trailing
/// all-digit `:port` is cut before the comparison, and a non-numeric tail
/// stays part of the name and fails it.
fn host_is_loopback(host: &str) -> bool {
    let name = match host.find(']') {
        Some(bracket_end) => &host[..=bracket_end],
        None => host
            .rsplit_once(':')
            .filter(|(_, port)| !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()))
            .map_or(host, |(name, _)| name),
    };
    let name = name.to_ascii_lowercase();
    LOOPBACK_HOSTS.contains(&name.as_str())
}

/// Whether an `Origin` value names a loopback `http` or `https` origin.
fn origin_is_loopback(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let scheme_known = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    scheme_known && host_is_loopback(rest)
}

/// Requests the same shutdown an external cancel performs.
///
/// Repeats answer identically: cancellation is idempotent.
async fn stop_server(State(stop): State<CancellationToken>) -> StatusCode {
    stop.cancel();
    StatusCode::ACCEPTED
}

/// Bound on one stop request: connect, send, and read the answer.
pub const STOP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a stop request did not reach a server that accepted it.
#[derive(Debug)]
pub enum StopRequestFailure {
    /// The request could not be built or sent, or its answer could not be read.
    Failed(reqwest::Error),
    /// The server answered with something other than acceptance.
    Refused(reqwest::StatusCode),
}

/// Asks the server `lock` records to stop, through its authorized `POST /api/stop`.
///
/// The server answers `202 Accepted` and starts the same shutdown an interrupt does; a
/// repeated request answers the same way. A refused connect counts as accepted: nothing
/// listens on the recorded port, so the server has already left. The request carries the
/// token `lock` records, so it stops that one server and no other: a server started after
/// it mints another token and refuses this one.
///
/// # Errors
///
/// Returns [`StopRequestFailure::Refused`] for any answer but acceptance, and
/// [`StopRequestFailure::Failed`] when the request outlives [`STOP_REQUEST_TIMEOUT`] or
/// fails for any other reason than a refused connect.
///
/// # Cancel safety
///
/// Dropping this future abandons the request; a request the server already received still
/// stops it.
pub async fn request_stop(lock: &ServerLock) -> Result<(), StopRequestFailure> {
    let client = reqwest::Client::builder()
        .timeout(STOP_REQUEST_TIMEOUT)
        .build()
        .map_err(StopRequestFailure::Failed)?;
    let answer = client
        .post(format!("http://127.0.0.1:{}{STOP_PATH}", lock.port))
        .bearer_auth(&lock.token)
        .send()
        .await;
    match answer {
        Ok(response) if response.status() == reqwest::StatusCode::ACCEPTED => Ok(()),
        Ok(response) => Err(StopRequestFailure::Refused(response.status())),
        Err(error) if error.is_connect() => Ok(()),
        Err(error) => Err(StopRequestFailure::Failed(error)),
    }
}

/// Per-request policy shared by every route: the token requests must
/// present, whether that token is checked at all, and the activity instant
/// served requests refresh.
#[derive(Clone, Debug)]
pub(crate) struct RequestGate {
    pub(crate) token: Arc<str>,
    pub(crate) check: TokenCheck,
    pub(crate) idle: Arc<IdleTracker>,
}

/// Refuses requests the token policy does not accept, tracking every
/// request that passes until its response completes.
///
/// The token separates OS users sharing the machine; the loopback bind is
/// the network boundary.
pub(crate) async fn authorize_request(
    State(gate): State<RequestGate>,
    request: Request,
    next: Next,
) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !gate.check.accepts(authorization, &gate.token) {
        return unauthorized();
    }
    let activity = gate.idle.begin();
    let response = next.run(request).await;
    drop(activity);
    response
}

/// Whether an `Authorization` value presents exactly `Bearer` plus `token`.
///
/// Equal-length comparison touches every byte, so a refusal's timing does
/// not reveal how much of the token matched. The earlier returns reveal
/// only the request's own shape - a missing scheme or a wrong length -
/// never a token byte.
fn bearer_authorized(authorization: Option<&str>, token: &str) -> bool {
    let Some(presented) = authorization
        .and_then(|value| value.strip_prefix(BEARER_SCHEME))
        .and_then(|schemed| schemed.strip_prefix(' '))
    else {
        return false;
    };
    if presented.len() != token.len() {
        return false;
    }
    presented
        .bytes()
        .zip(token.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

/// The `401` refusal naming the scheme a caller must authenticate with.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(BEARER_SCHEME),
        )],
    )
        .into_response()
}

/// Authorized-request state used by the idle watcher.
#[derive(Debug)]
struct IdleState {
    /// Instant when server started or last authorized request completed.
    last_activity: Instant,
    /// Authorized requests that started but have not completed.
    active_requests: usize,
}

/// Tracks active authorized requests and the last completed activity.
#[derive(Debug)]
pub(crate) struct IdleTracker {
    state: Mutex<IdleState>,
    changed: Notify,
}

impl IdleTracker {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(IdleState {
                last_activity: Instant::now(),
                active_requests: 0,
            }),
            changed: Notify::new(),
        }
    }

    /// Starts one authorized request. Dropping returned guard records completion.
    pub(crate) fn begin(&self) -> ActiveRequest<'_> {
        self.start();
        ActiveRequest { idle: self }
    }

    pub(crate) fn start(&self) {
        let mut state = self.lock_state();
        state.active_requests = state
            .active_requests
            .checked_add(1)
            .expect("active authorized request count must fit usize");
        drop(state);
        self.changed.notify_waiters();
    }

    /// Records one authorized request completion.
    pub(crate) fn finish(&self) {
        let mut state = self.lock_state();
        state.active_requests = state
            .active_requests
            .checked_sub(1)
            .expect("an active authorized request must complete once");
        if state.active_requests == 0 {
            state.last_activity = Instant::now();
        }
        drop(state);
        self.changed.notify_waiters();
    }

    /// The instant the server becomes idle, absent while an authorized request remains active.
    pub(crate) fn idle_deadline(&self, idle_timeout: Duration) -> Option<Instant> {
        let state = self.lock_state();
        (state.active_requests == 0).then_some(state.last_activity + idle_timeout)
    }

    /// Waits until no authorized request is active, at most `bound`. Answers
    /// whether the server settled; `false` means the bound passed first.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future ends the wait; it changes no state.
    pub(crate) async fn settled(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.lock_state().active_requests == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return false;
            }
        }
    }

    /// The tracked state, recovered from a poisoned lock: the stored
    /// value is plain data, valid regardless of a panicked writer.
    fn lock_state(&self) -> MutexGuard<'_, IdleState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// One active authorized request. Completion is cancellation-safe.
pub(crate) struct ActiveRequest<'a> {
    idle: &'a IdleTracker,
}

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.idle.finish();
    }
}

/// Cancels `stop` once `idle_timeout` passes after the last authorized request
/// completes while no request remains active.
///
/// Each turn waits for shutdown, activity state change, or current idle deadline.
/// Active requests have no idle deadline; last completion starts one.
///
/// # Cancel safety
///
/// Dropping this future ends the watch without cancelling `stop`.
pub(crate) async fn watch_idle(
    idle: Arc<IdleTracker>,
    idle_timeout: Duration,
    stop: CancellationToken,
) {
    loop {
        let changed = idle.changed.notified();
        tokio::pin!(changed);
        if let Some(deadline) = idle.idle_deadline(idle_timeout) {
            tokio::select! {
                () = stop.cancelled() => return,
                () = &mut changed => continue,
                () = tokio::time::sleep_until(deadline) => {}
            }
        } else {
            tokio::select! {
                () = stop.cancelled() => return,
                () = &mut changed => continue,
            }
        }
        if idle
            .idle_deadline(idle_timeout)
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            stop.cancel();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{StatusCode, header};
    use rift_error::errors;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::ServerConfiguration;
    use rift_protocol::lock::{ProductIdentity, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, ServerLock};
    use tokio_util::sync::CancellationToken;

    use tokio::time::Instant;

    use crate::failure::{McpErrorExt as _, WireFailure as _};
    use crate::server::EngineHold;
    use crate::validation::{IndexSupervisor, IndexValidation};

    use super::{
        HttpServer, IdleTracker, TokenCheck, bearer_authorized, bind_first_free,
        classify_serve_outcome, mint_token, stop_outcome_label, unauthorized, watch_idle,
    };

    #[test]
    fn minted_token_satisfies_the_advertised_lock_contract() {
        let token = mint_token().expect("token must mint");
        assert_eq!(token.len(), SERVER_TOKEN_LENGTH);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "token must stay within the base64url alphabet: {token}"
        );
        let lock = ServerLock {
            port: SERVER_PORT_MIN,
            token,
            pid: 1,
            identity: ProductIdentity {
                version: "0.0.9".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server: None,
        };
        assert_eq!(lock.validate(), Ok(()));
    }

    #[test]
    fn two_minted_tokens_differ() {
        let first = mint_token().expect("first token must mint");
        let second = mint_token().expect("second token must mint");
        assert_ne!(first, second, "two mints must draw fresh entropy");
    }

    #[test]
    fn bind_selects_the_first_free_port() {
        let (port, bound) =
            bind_first_free(4..=6, Ok::<u16, std::io::Error>).expect("first port must bind");
        assert_eq!((port, bound), (4, 4));
    }

    #[test]
    fn bind_skips_busy_ports() {
        let (port, bound) = bind_first_free(4..=6, |port| {
            if port < 6 {
                Err(std::io::Error::from(std::io::ErrorKind::AddrInUse))
            } else {
                Ok(port)
            }
        })
        .expect("the free port must bind");
        assert_eq!((port, bound), (6, 6));
    }

    #[test]
    fn boundary_port_is_accepted() {
        let (port, _) = bind_first_free(6..=6, Ok::<u16, std::io::Error>)
            .expect("a single-port range must bind its boundary");
        assert_eq!(port, 6);
    }

    #[test]
    fn exhausted_range_names_its_bounds_and_classifies_transient() {
        let error = bind_first_free(4..=6, |_| {
            Err::<u16, _>(std::io::Error::from(std::io::ErrorKind::AddrInUse))
        })
        .expect_err("an all-busy range must refuse");
        assert_eq!(error.slug(), errors::mcp::http_ports_exhausted::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "port_min" && value == "4")
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "port_max" && value == "6")
        );
        let wire = error
            .mcp()
            .wire_error(rift_protocol::error::ErrorPhase::Read);
        assert_eq!(
            wire.code,
            rift_protocol::error::ErrorCode::TemporarilyUnavailable
        );
        assert_eq!(
            wire.retry,
            rift_protocol::error::RetryDirective::SameRequest
        );
    }

    #[test]
    fn serve_error_names_its_operation_and_exposes_the_source() {
        let error = errors::mcp::http_serve_failed()
            .operation("http serve loop")
            .source(std::io::Error::other("socket gone"))
            .error();
        assert_eq!(error.slug(), errors::mcp::http_serve_failed::SLUG);
        let rendered = error.to_string();
        assert!(rendered.contains("http serve loop"), "{rendered}");
        let source = std::error::Error::source(&error).expect("source must be exposed");
        assert_eq!(source.to_string(), "socket gone");
    }

    #[test]
    fn ports_exhausted_carries_no_source() {
        let error = bind_first_free(4..=6, |_| {
            Err::<u16, _>(std::io::Error::from(std::io::ErrorKind::AddrInUse))
        })
        .expect_err("an all-busy range must refuse");
        assert!(
            std::error::Error::source(&error).is_none(),
            "an exhausted range has no underlying failure to expose"
        );
    }

    #[test]
    fn clean_serve_outcome_classifies_as_success() {
        assert!(classify_serve_outcome(Ok(Ok(()))).is_ok());
    }

    #[test]
    fn serve_loop_failure_names_the_loop() {
        let error = classify_serve_outcome(Ok(Err(std::io::Error::other("socket gone"))))
            .expect_err("a serve loop failure must classify as an error");
        let rendered = error.to_string();
        assert!(rendered.contains("http serve loop"), "{rendered}");
    }

    #[tokio::test]
    async fn aborted_serve_task_names_the_task() {
        let handle = tokio::spawn(std::future::pending::<Result<(), std::io::Error>>());
        handle.abort();
        let outcome = handle.await;
        let error = classify_serve_outcome(outcome)
            .expect_err("an aborted serving task must classify as an error");
        let rendered = error.to_string();
        assert!(rendered.contains("http serve task"), "{rendered}");
    }

    /// A stop stage passes its outcome through unchanged and records its name, its
    /// outcome, and a failure's error.
    #[test]
    fn a_stop_stage_records_its_name_outcome_and_error() {
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the fixture runtime must start");
        let deadline = Instant::now() + Duration::from_secs(4);
        let (passed, failed) = runtime.block_on(async {
            let passed = super::stop_stage("passing stage", deadline, async {
                Ok::<_, rift_error::RiftError>(7)
            })
            .await;
            let failed = super::stop_stage("failing stage", deadline, async {
                errors::mcp::http_serve_failed()
                    .operation("failing stage")
                    .source(std::io::Error::other("injected stage failure"))
                    .fail::<()>()
            })
            .await;
            (passed, failed)
        });
        drop(recorder);

        assert_eq!(passed.ok(), Some(7));
        assert!(failed.is_err(), "the stage failure passes through");
        let ended = drain
            .queued_records()
            .into_iter()
            .filter(|record| record.message() == "stop stage ended")
            .collect::<Vec<_>>();
        assert_eq!(ended.len(), 2, "one record per stage: {ended:?}");
        assert!(ended[0].fields().contains("passing stage"), "{ended:?}");
        assert!(
            ended[0].fields().contains("\"outcome\":\"ok\""),
            "{ended:?}"
        );
        assert_eq!(ended[1].level(), "warn");
        assert!(ended[1].fields().contains("failing stage"), "{ended:?}");
        assert!(
            ended[1].fields().contains("injected stage failure"),
            "{ended:?}"
        );
    }

    /// A metrics close whose checkpoint outlasts the stop's deadline ends its stage with
    /// the outcome `timeout` and no error, so the stop's exit status stays clean, and
    /// records the close at `warn` with the stage it was in.
    #[tokio::test]
    async fn a_metrics_checkpoint_past_the_stop_deadline_ends_its_stage_without_an_error()
    -> Result<(), Box<dyn std::error::Error>> {
        const BUDGET: Duration = Duration::from_secs(4);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        let store = rift_tracing::LogStore::open(&directory.path().join("metrics"), None).await?;
        let (holding, release) = store.hold_next_checkpoint();
        let deadline = Instant::now() + BUDGET;

        // The held writer answers nothing, so the paused clock reaches the deadline only
        // once the close waits inside the checkpoint.
        let (closed, held) = tokio::join!(super::close_logs(Some(&store), deadline), async {
            let held = holding.await;
            tokio::time::pause();
            tokio::time::advance(BUDGET).await;
            held
        });

        held?;
        closed?;
        tokio::time::resume();
        release.send(())?;
        drop(recorder);
        let records = drain.queued_records();
        let ended = records
            .iter()
            .find(|record| record.message() == "stop stage ended")
            .ok_or("the stage ended with a record")?;
        assert_eq!(ended.level(), "warn");
        let ended: serde_json::Value = serde_json::from_str(ended.fields())?;
        assert_eq!(ended["stage"], "metrics database close", "{ended}");
        assert_eq!(ended["outcome"], "timeout", "{ended}");
        let close = records
            .iter()
            .find(|record| {
                record.operation() == "database.close"
                    && record.message().starts_with("database checkpoint")
            })
            .ok_or("the close was recorded")?;
        assert_eq!(close.level(), "warn");
        assert_eq!(
            close.message(),
            "database checkpoint outlasted the shutdown deadline; the write-ahead log stays \
             for the next open"
        );
        let close: serde_json::Value = serde_json::from_str(close.fields())?;
        assert_eq!(close["database"], "metrics", "{close}");
        assert_eq!(close["stage"], "checkpoint", "{close}");
        assert!(close.get("elapsed_ms").is_some(), "{close}");
        Ok(())
    }

    /// Every stop stage records its opening, and the stage the stop deadline expires in
    /// publishes the operations still open, once: a stage that starts past the deadline
    /// publishes nothing.
    #[tokio::test(start_paused = true)]
    async fn the_stage_the_stop_deadline_expires_in_publishes_the_operations_in_flight()
    -> Result<(), Box<dyn std::error::Error>> {
        const BUDGET: Duration = Duration::from_millis(10);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let deadline = Instant::now() + BUDGET;
        let stages = [
            ("engines shutdown", Duration::ZERO),
            ("index supervisor shutdown", BUDGET * 2),
            ("SQLite worker shutdown", Duration::ZERO),
        ];
        for (stage, spent) in stages {
            super::stop_stage(stage, deadline, async move {
                tokio::time::advance(spent).await;
                Ok::<_, rift_error::RiftError>(())
            })
            .await?;
        }
        drop(recorder);

        let records = drain.queued_records();
        let opened = records
            .iter()
            .filter(|record| record.message() == "operation opened")
            .count();
        assert_eq!(opened, stages.len(), "one opening per stage: {records:?}");
        let tables = records
            .iter()
            .filter(|record| record.message() == "operations in flight")
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 1, "one publication per stop: {tables:?}");
        assert_eq!(
            tables[0].level(),
            "warn",
            "a WARN filter keeps it: {tables:?}"
        );
        let table: serde_json::Value = serde_json::from_str(tables[0].fields())?;
        assert_eq!(table["reason"], "stop deadline", "{table}");
        assert_eq!(
            table["root_span"]["fields"]["stage"], "index supervisor shutdown",
            "{table}"
        );
        assert!(
            table["operations"]
                .as_str()
                .is_some_and(|listed| listed.contains("\"operation\":\"server.stop\"")),
            "{table}"
        );
        Ok(())
    }

    #[test]
    fn stop_outcome_label_names_both_outcomes() {
        assert_eq!(stop_outcome_label(true), "ok");
        assert_eq!(stop_outcome_label(false), "error");
    }

    #[test]
    fn stopping_retires_index_data_without_waiting_for_its_release() {
        const WAIT_MAX: Duration = Duration::from_secs(2);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("the fixture runtime must start");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("the fixture root must be created");
            let (validation, _invalidations) =
                IndexValidation::new(WorkspaceIndexLimits::default().files_max());
            let retired = Arc::downgrade(&validation);
            let server = HttpServer {
                port: SERVER_PORT_MIN,
                token: mint_token().expect("token must mint"),
                identity: ProductIdentity {
                    version: "0.0.9".to_owned(),
                    schema_digest: "b".repeat(64),
                },
                server_configuration: ServerConfiguration::default(),
                stop: CancellationToken::new(),
                serving: tokio::spawn(std::future::ready(Ok::<(), std::io::Error>(()))),
                idle_watch: tokio::spawn(std::future::ready(())),
                repository_idle_watch: None,
                supervisor: Some(IndexSupervisor { validation }),
                engines: Some(Arc::new(EngineHold::new(
                    directory.path().to_path_buf(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                ))),
                search_index: None,
                logs: None,
                repository_workspaces: None,
            };
            let (occupied, ready) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::sync_channel(1);
            let blocking = tokio::task::spawn_blocking(move || {
                occupied
                    .send(())
                    .expect("the fixture must observe the worker");
                // A failed assertion cannot leave Runtime::drop waiting on this worker.
                let _released = released.recv_timeout(WAIT_MAX);
            });
            ready.await.expect("the blocking worker must start");

            let result = tokio::time::timeout(WAIT_MAX, server.stopped(WAIT_MAX)).await;
            let retained_while_queued = retired.upgrade().is_some();
            let _released = release.send(());
            let (_deadline, stopped) = result.expect("stopping must not wait for index release");
            stopped.expect("the serving tasks must stop successfully");
            assert!(
                retained_while_queued,
                "retired index data must wait on the blocking pool, outside the stop"
            );
            blocking.await.expect("the occupied worker must finish");
            tokio::time::timeout(WAIT_MAX, async {
                while retired.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
                // A zero strong count can precede Drop's return. The sole blocking
                // worker cannot run this marker until that earlier Drop completes.
                tokio::task::spawn_blocking(|| ())
                    .await
                    .expect("index release must leave the blocking worker usable");
            })
            .await
            .expect("an embedded runtime must eventually release the retired index");
        });
    }

    #[tokio::test(start_paused = true)]
    async fn the_stop_budget_starts_where_a_serve_loop_ended_on_its_own() {
        /// How long the server serves before its loop ends. It outlasts the
        /// budget, so a deadline taken where serving began is already spent.
        const SERVE_SPAN: Duration = Duration::from_secs(30);
        /// The budget every stage of the stop shares.
        const STOP_BUDGET: Duration = Duration::from_secs(8);

        let directory = tempfile::tempdir().expect("the fixture root must be created");
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        *validation.task.lock().await = Some(tokio::spawn(std::future::pending::<()>()));
        let server = HttpServer {
            port: SERVER_PORT_MIN,
            token: mint_token().expect("token must mint"),
            identity: ProductIdentity {
                version: "0.0.9".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server_configuration: ServerConfiguration::default(),
            stop: CancellationToken::new(),
            serving: tokio::spawn(async {
                tokio::time::sleep(SERVE_SPAN).await;
                Ok::<(), std::io::Error>(())
            }),
            idle_watch: tokio::spawn(std::future::ready(())),
            repository_idle_watch: None,
            supervisor: Some(IndexSupervisor { validation }),
            engines: Some(Arc::new(EngineHold::new(
                directory.path().to_path_buf(),
                BTreeMap::new(),
                BTreeMap::new(),
            ))),
            search_index: None,
            logs: None,
            repository_workspaces: None,
        };

        let started = Instant::now();
        let (deadline, stopped) = server.stopped(STOP_BUDGET).await;
        let served_until = started + SERVE_SPAN;
        assert_eq!(
            deadline,
            served_until + STOP_BUDGET,
            "a serve loop that ended on its own begins the stop: \
             served_until={served_until:?}, budget={STOP_BUDGET:?}, deadline={deadline:?}"
        );
        assert_eq!(
            Instant::now(),
            deadline,
            "the later stages must be given the whole budget"
        );
        stopped.expect("a supervisor aborted at the deadline ends its stage `timeout`, no error");
    }

    /// A supervisor that never joins is aborted `reserve` before the stop's deadline: its
    /// stage ends `timeout` at `warn` with nothing of the stages' bound left, publishes the
    /// operations in flight, and fails nothing, while the returned deadline keeps the
    /// whole budget for the caller's database close and log flush.
    #[tokio::test(start_paused = true)]
    async fn a_supervisor_that_never_joins_ends_its_stage_by_the_reserve()
    -> Result<(), Box<dyn std::error::Error>> {
        const STOP_BUDGET: Duration = Duration::from_secs(4);
        const RESERVE: Duration = Duration::from_secs(1);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        *validation.task.lock().await = Some(tokio::spawn(std::future::pending::<()>()));
        let stop = CancellationToken::new();
        let server = HttpServer {
            port: SERVER_PORT_MIN,
            token: mint_token()?,
            identity: ProductIdentity {
                version: "0.0.9".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server_configuration: ServerConfiguration::default(),
            stop: stop.clone(),
            serving: tokio::spawn(std::future::ready(Ok::<(), std::io::Error>(()))),
            idle_watch: tokio::spawn(std::future::ready(())),
            repository_idle_watch: None,
            supervisor: Some(IndexSupervisor { validation }),
            engines: Some(Arc::new(EngineHold::new(
                directory.path().to_path_buf(),
                BTreeMap::new(),
                BTreeMap::new(),
            ))),
            search_index: None,
            logs: None,
            repository_workspaces: None,
        };

        let started = Instant::now();
        stop.cancel();
        let (deadline, stopped, _database) =
            server.stopped_before_database(STOP_BUDGET, RESERVE).await;
        drop(recorder);

        stopped?;
        assert_eq!(
            deadline,
            started + STOP_BUDGET,
            "the deadline keeps the budget"
        );
        assert_eq!(
            Instant::now(),
            deadline - RESERVE,
            "the stages end by the reserve before the deadline"
        );
        let records = drain.queued_records();
        let ended = records
            .iter()
            .filter(|record| record.message() == "stop stage ended")
            .map(|record| Ok((record.level(), serde_json::from_str(record.fields())?)))
            .collect::<Result<Vec<(_, serde_json::Value)>, serde_json::Error>>()?;
        let (level, supervisor) = ended
            .iter()
            .find(|(_, fields)| fields["stage"] == "index supervisor shutdown")
            .ok_or("the supervisor stage ended with a record")?;
        assert_eq!(*level, "warn", "{supervisor}");
        assert_eq!(supervisor["outcome"], "timeout", "{supervisor}");
        let tables = records
            .iter()
            .filter(|record| record.message() == "operations in flight")
            .map(|record| serde_json::from_str::<serde_json::Value>(record.fields()))
            .collect::<Result<Vec<_>, _>>()?;
        assert!(
            tables.iter().any(|table| table["reason"] == "stop deadline"
                && table["root_span"]["fields"]["stage"] == "index supervisor shutdown"),
            "{tables:?}"
        );
        let stopped_line = records
            .iter()
            .find(|record| record.message() == "MCP server stopped")
            .ok_or("the stop records its end")?;
        assert!(
            stopped_line.fields().contains("\"outcome\":\"ok\""),
            "{}",
            stopped_line.fields()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_serving_drain_that_never_ends_still_returns_by_the_deadline() {
        /// The budget every stage of the stop shares.
        const STOP_BUDGET: Duration = Duration::from_secs(8);

        let directory = tempfile::tempdir().expect("the fixture root must be created");
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let stop = CancellationToken::new();
        let server = HttpServer {
            port: SERVER_PORT_MIN,
            token: mint_token().expect("token must mint"),
            identity: ProductIdentity {
                version: "0.0.9".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server_configuration: ServerConfiguration::default(),
            stop: stop.clone(),
            // A request the stop never finishes draining: axum's graceful
            // shutdown holds the serving task until it completes.
            serving: tokio::spawn(std::future::pending::<Result<(), std::io::Error>>()),
            idle_watch: tokio::spawn(std::future::ready(())),
            repository_idle_watch: None,
            supervisor: Some(IndexSupervisor { validation }),
            engines: Some(Arc::new(EngineHold::new(
                directory.path().to_path_buf(),
                BTreeMap::new(),
                BTreeMap::new(),
            ))),
            search_index: None,
            logs: None,
            repository_workspaces: None,
        };

        let started = Instant::now();
        stop.cancel();
        let (deadline, stopped) = server.stopped(STOP_BUDGET).await;
        assert_eq!(
            deadline,
            started + STOP_BUDGET,
            "the stop's deadline starts where the stop began: started={started:?}, \
             budget={STOP_BUDGET:?}, deadline={deadline:?}"
        );
        assert_eq!(
            Instant::now(),
            deadline,
            "an unfinished drain must give up exactly at the deadline"
        );
        let error = stopped.expect_err("a drain that never ends must report the stop unfinished");
        let rendered = error.to_string();
        assert!(rendered.contains("http serve drain"), "{rendered}");
    }

    #[test]
    fn poisoned_activity_lock_still_tracks_activity() {
        let idle = Arc::new(IdleTracker::new());
        let poisoner = Arc::clone(&idle);
        std::thread::spawn(move || {
            let _guard = poisoner
                .state
                .lock()
                .expect("the first lock of a fresh tracker must succeed");
            panic!("poison the activity lock");
        })
        .join()
        .expect_err("the poisoning thread must end by panic");
        assert!(
            idle.state.lock().is_err(),
            "the lock must be poisoned for the recovery arm to matter"
        );
        let before = Instant::now();
        drop(idle.begin());
        let deadline = idle
            .idle_deadline(Duration::from_secs(5))
            .expect("completed activity must have an idle deadline");
        assert!(
            deadline >= before + Duration::from_secs(5),
            "a recovered tracker must keep tracking completed activity"
        );
    }

    #[test]
    fn exact_bearer_token_is_authorized() {
        assert!(bearer_authorized(Some("Bearer secret"), "secret"));
    }

    #[test]
    fn wrong_token_is_refused() {
        assert!(!bearer_authorized(Some("Bearer secrex"), "secret"));
        assert!(!bearer_authorized(Some("Bearer secre"), "secret"));
        assert!(!bearer_authorized(Some("Bearer secrets"), "secret"));
    }

    #[test]
    fn malformed_authorization_is_refused() {
        for value in [
            "secret",
            "bearer secret",
            "Bearer",
            "Bearer  secret",
            "Basic secret",
            "",
        ] {
            assert!(
                !bearer_authorized(Some(value), "secret"),
                "malformed value must be refused: {value:?}"
            );
        }
    }

    #[test]
    fn missing_authorization_is_refused() {
        assert!(!bearer_authorized(None, "secret"));
    }

    #[test]
    fn the_required_check_is_the_default_and_the_bearer_gate() {
        assert_eq!(TokenCheck::default(), TokenCheck::Required);
        assert!(TokenCheck::Required.accepts(Some("Bearer secret"), "secret"));
        assert!(!TokenCheck::Required.accepts(Some("Bearer wrong!"), "secret"));
        assert!(!TokenCheck::Required.accepts(None, "secret"));
    }

    #[test]
    fn a_skipped_check_accepts_what_the_request_presents() {
        assert!(TokenCheck::Skipped.accepts(None, "secret"));
        assert!(TokenCheck::Skipped.accepts(Some("Bearer wrong!"), "secret"));
        assert!(TokenCheck::Skipped.accepts(Some("Basic secret"), "secret"));
    }

    #[test]
    fn refusal_carries_the_authenticate_scheme() {
        let response = unauthorized();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer")
        );
    }

    #[test]
    fn loopback_hosts_are_accepted_with_and_without_ports() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:12345",
            "localhost",
            "localhost:80",
            "LOCALHOST:13000",
            "[::1]",
            "[::1]:12000",
        ] {
            assert!(
                super::host_is_loopback(host),
                "host {host:?} must be accepted"
            );
        }
    }

    #[test]
    fn foreign_hosts_are_refused() {
        for host in [
            "",
            "rift.example",
            "rift.example:12345",
            "127.0.0.1.rift.example",
            "localhost:port",
            "[::2]:80",
            "127.0.0.1:",
        ] {
            assert!(
                !super::host_is_loopback(host),
                "host {host:?} must be refused"
            );
        }
    }

    #[test]
    fn loopback_origins_are_accepted() {
        for origin in [
            "http://127.0.0.1:5500",
            "https://localhost",
            "HTTP://[::1]:13000",
        ] {
            assert!(
                super::origin_is_loopback(origin),
                "origin {origin:?} must be accepted"
            );
        }
    }

    #[test]
    fn foreign_origins_are_refused() {
        for origin in [
            "",
            "null",
            "http://rift.example",
            "https://rift.example:443",
            "file://127.0.0.1",
            "127.0.0.1",
        ] {
            assert!(
                !super::origin_is_loopback(origin),
                "origin {origin:?} must be refused"
            );
        }
    }

    #[test]
    fn boundary_refusal_names_the_header() {
        let response = super::boundary_refusal("Origin");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_watch_stops_a_quiet_server() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_secs(5),
            stop.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(6)).await;
        watch.await.expect("watch task must join");
        assert!(
            stop.is_cancelled(),
            "a quiet span past the timeout must stop the server"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completed_activity_defers_the_idle_stop() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_secs(5),
            stop.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(idle.begin());
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !stop.is_cancelled(),
            "completed activity inside the span must defer the stop"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        watch.await.expect("watch task must join");
        assert!(
            stop.is_cancelled(),
            "the deferred deadline must still stop the server"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn authorized_request_defers_idle_stop_until_after_completion() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_secs(5),
            stop.clone(),
        ));
        let request = idle.begin();
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(
            !stop.is_cancelled(),
            "idle timeout must not stop an authorized request in progress"
        );
        drop(request);
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            !stop.is_cancelled(),
            "idle time starts after the last authorized request completes"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        watch.await.expect("watch task must join");
        assert!(
            stop.is_cancelled(),
            "the later quiet span must stop the server"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_active_request_must_complete_before_idle_time_starts() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_secs(5),
            stop.clone(),
        ));
        let first = idle.begin();
        let second = idle.begin();
        drop(first);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(
            !stop.is_cancelled(),
            "one remaining authorized request must keep the server active"
        );
        drop(second);
        tokio::time::sleep(Duration::from_secs(6)).await;
        watch.await.expect("watch task must join");
        assert!(
            stop.is_cancelled(),
            "idle time starts after final completion"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn external_cancel_ends_the_idle_watch() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_hours(1),
            stop.clone(),
        ));
        stop.cancel();
        watch
            .await
            .expect("an externally cancelled watch must end promptly");
    }

    /// A metrics close queued behind a held writer at the stop's deadline ends its stage
    /// with the outcome `timeout` and no error, so the stop's exit status stays clean. A
    /// test connection holds the write lock while an append is queued ahead of the close,
    /// so the writer thread cannot answer the close before the lock is released, and the
    /// paused clock carries the deadline past while the writer waits. The writer's busy
    /// timeout, `METRICS_BUSY_TIMEOUT_MS`, only bounds that wait; the append that commits
    /// after the release proves the lock outlasted the deadline.
    #[tokio::test(start_paused = true)]
    async fn a_metrics_close_queued_at_the_stop_deadline_ends_its_stage_without_an_error()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::task::{Context, Waker};

        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("metrics");
        let store = rift_tracing::LogStore::open(&path, None).await?;
        let holder = rusqlite::Connection::open(&path)?;
        holder.execute_batch("BEGIN IMMEDIATE")?;
        let held = rift_tracing::LogRecord::new(0, "info", "rift", "storage", "test", "held", "{}");
        let records = [held];
        let mut append = Box::pin(store.append(&records, 1_000));
        let first_poll =
            std::future::Future::poll(append.as_mut(), &mut Context::from_waker(Waker::noop()));
        assert!(
            first_poll.is_pending(),
            "the held lock keeps the append waiting"
        );

        let deadline = Instant::now() + Duration::from_secs(1);
        super::close_logs(Some(&store), deadline).await?;

        holder.execute_batch("ROLLBACK")?;
        append.await?;
        drop(recorder);
        let records = drain.queued_records();
        let ended = records
            .iter()
            .find(|record| record.message() == "stop stage ended")
            .ok_or("the stage ended with a record")?;
        assert_eq!(ended.level(), "warn");
        let ended: serde_json::Value = serde_json::from_str(ended.fields())?;
        assert_eq!(ended["stage"], "metrics database close", "{ended}");
        assert_eq!(ended["outcome"], "timeout", "{ended}");
        let close = records
            .iter()
            .find(|record| {
                record.operation() == "database.close"
                    && record.message().starts_with("database checkpoint")
            })
            .ok_or("the close was recorded")?;
        assert_eq!(close.level(), "warn");
        let close: serde_json::Value = serde_json::from_str(close.fields())?;
        assert_eq!(close["database"], "metrics", "{close}");
        assert_eq!(close["stage"], "queued", "{close}");
        Ok(())
    }

    /// A server whose serve loop already ended, with no supervisor, no databases, and the
    /// given idle watches and engine hold.
    fn server_with_watches(
        idle_watch: tokio::task::JoinHandle<()>,
        repository_idle_watch: Option<tokio::task::JoinHandle<()>>,
        engines: Option<Arc<EngineHold>>,
    ) -> HttpServer {
        HttpServer {
            port: SERVER_PORT_MIN,
            token: "a".repeat(SERVER_TOKEN_LENGTH),
            identity: ProductIdentity {
                version: "0.0.9".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server_configuration: ServerConfiguration::default(),
            stop: CancellationToken::new(),
            serving: tokio::spawn(std::future::ready(Ok::<(), std::io::Error>(()))),
            idle_watch,
            repository_idle_watch,
            supervisor: None,
            engines,
            search_index: None,
            logs: None,
            repository_workspaces: None,
        }
    }

    /// An idle watch task that panicked fails the stop under its own stage.
    #[tokio::test]
    async fn an_idle_watch_that_panicked_fails_the_stop() {
        let idle_watch = tokio::spawn(async { panic!("the idle watch panicked") });
        let server = server_with_watches(idle_watch, None, None);

        let (_deadline, stopped) = server.stopped(Duration::from_secs(8)).await;

        let error = stopped.expect_err("a panicked idle watch must fail the stop");
        assert_eq!(error.slug(), errors::mcp::http_serve_failed::SLUG);
        let rendered = error.to_string();
        assert!(rendered.contains("idle watch task"), "{rendered}");
    }

    /// A repository idle watch task that panicked fails the stop under its own stage.
    #[tokio::test]
    async fn a_workspace_idle_watch_that_panicked_fails_the_stop() {
        let repository_idle_watch =
            tokio::spawn(async { panic!("the workspace idle watch panicked") });
        let idle_watch = tokio::spawn(std::future::ready(()));
        let server = server_with_watches(idle_watch, Some(repository_idle_watch), None);

        let (_deadline, stopped) = server.stopped(Duration::from_secs(8)).await;

        let error = stopped.expect_err("a panicked workspace idle watch must fail the stop");
        assert_eq!(error.slug(), errors::mcp::http_serve_failed::SLUG);
        let rendered = error.to_string();
        assert!(rendered.contains("workspace idle watch task"), "{rendered}");
    }

    /// A repository idle watch still running at the stop deadline is aborted, and the
    /// stop fails under the watch's stage exactly at the deadline.
    #[tokio::test(start_paused = true)]
    async fn a_workspace_idle_watch_that_outlasts_the_deadline_is_aborted_and_fails_the_stop() {
        const STOP_BUDGET: Duration = Duration::from_secs(8);
        let repository_idle_watch = tokio::spawn(std::future::pending::<()>());
        let watch = repository_idle_watch.abort_handle();
        let idle_watch = tokio::spawn(std::future::ready(()));
        let server = server_with_watches(idle_watch, Some(repository_idle_watch), None);

        let (deadline, stopped) = server.stopped(STOP_BUDGET).await;

        assert_eq!(
            Instant::now(),
            deadline,
            "the watch is given the whole budget"
        );
        let error = stopped.expect_err("a watch that never ends must fail the stop");
        assert_eq!(error.slug(), errors::mcp::http_serve_failed::SLUG);
        let rendered = error.to_string();
        assert!(rendered.contains("workspace idle watch task"), "{rendered}");
        assert!(
            watch.is_finished(),
            "the stop aborts the watch it gave up on"
        );
    }

    /// An engine hold whose shutdown is still running when the stop deadline passes ends
    /// the engines stage as an error. The idle watch spends the whole budget, and the
    /// hold's one engine slot ends on a task the stop's first poll has not run yet, so the
    /// stage starts at the deadline with its shutdown pending on every run.
    #[tokio::test(start_paused = true)]
    async fn an_engines_shutdown_the_deadline_ends_records_its_stage_failed()
    -> Result<(), Box<dyn std::error::Error>> {
        const STOP_BUDGET: Duration = Duration::from_secs(8);
        let directory = tempfile::tempdir()?;
        let key = rift_server::LspProcessKey::named("ty");
        let configuration = serde_json::from_value(serde_json::json!({ "command": "uvx" }))?;
        let engines = EngineHold::new(
            directory.path().to_path_buf(),
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("python".to_owned(), key)]),
        );
        let idle_watch = tokio::spawn(tokio::time::sleep(STOP_BUDGET));
        let server = server_with_watches(idle_watch, None, Some(Arc::new(engines)));
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;

        let (_deadline, stopped) = server.stopped(STOP_BUDGET).await;
        drop(recorder);

        stopped?;
        let records = drain.queued_records();
        let engines_stage = records
            .iter()
            .find(|record| {
                record.message() == "stop stage ended"
                    && record.fields().contains("\"stage\":\"engines shutdown\"")
            })
            .ok_or("the engines stage records how it ended")?;
        let fields: serde_json::Value = serde_json::from_str(engines_stage.fields())?;
        assert_eq!(fields["outcome"], "error", "{fields}");
        let stopped_record = records
            .iter()
            .find(|record| record.message() == "MCP server stopped")
            .ok_or("the stop records its outcome")?;
        let fields: serde_json::Value = serde_json::from_str(stopped_record.fields())?;
        assert_eq!(fields["outcome"], "error", "{fields}");
        Ok(())
    }

    /// A search index whose vectors database is still in its first open at the deadline
    /// fails `close_search` as a SQLite worker shutdown, and records the failure as a
    /// `database.close` warning. The test holds the migration lock the open waits on, so
    /// the open stays in flight past the deadline on every run.
    #[tokio::test]
    async fn a_search_close_that_outlasts_the_vectors_first_open_fails_and_records_it()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::task::{Context, Waker};

        use rift_index::{DatabaseName, DatabasePool, LazyDatabase, WorkspaceDatabase};

        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        std::fs::create_dir_all(&state_directory)?;
        let index_path = DatabaseName::Index.path(&state_directory);
        let pool = DatabasePool::new(4, 60_000);
        let database = WorkspaceDatabase::open(&index_path, DatabaseName::Index, pool).await?;
        let vectors_path = DatabaseName::Vectors.path(&state_directory);
        let vectors = Arc::new(LazyDatabase::new(
            &vectors_path,
            DatabaseName::Vectors,
            None,
        ));
        let migration_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(DatabaseName::Vectors.migration_lock_path(&state_directory))?;
        migration_lock.try_lock()?;
        let mut opening = Box::pin(vectors.resolve(pool));
        let first_poll =
            std::future::Future::poll(opening.as_mut(), &mut Context::from_waker(Waker::noop()));
        assert!(
            first_poll.is_pending(),
            "the held migration lock keeps the open waiting"
        );
        let limits = rift_search::SearchIndexLimits::default();
        let search = rift_search::SearchIndex::attached(database, Arc::clone(&vectors), limits)?;
        let mut shutdown = super::DeferredDatabaseShutdown(Some(Arc::new(search)), None);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;

        let deadline = Instant::now() + Duration::from_millis(200);
        let refusal = shutdown
            .close_search(deadline)
            .await
            .expect_err("an open in flight outlasts the close deadline");
        drop(recorder);

        assert_eq!(refusal.slug(), errors::mcp::http_serve_failed::SLUG);
        let rendered = refusal.to_string();
        assert!(rendered.contains("SQLite worker shutdown"), "{rendered}");
        let records = drain.queued_records();
        let failed = records
            .iter()
            .find(|record| record.message() == "the index or vectors database did not close")
            .ok_or("the failed close is recorded")?;
        assert_eq!(failed.level(), "warn");
        assert_eq!(failed.operation(), "database.close");
        drop(opening);
        drop(migration_lock);
        Ok(())
    }

    /// A stop failure of the index database lands in the metrics database: another
    /// connection holds the write lock on `.rift/index` while a lexical write waits on it
    /// past the index close, so the close's checkpoint and worker stop outlast their
    /// deadline, and the drain's final flush writes their `database.close` records to
    /// `.rift/metrics`.
    ///
    /// The close starts only once the lexical write holds the write turn: its
    /// `lexical.write_turn` span has ended, so its transaction start is waiting on the
    /// held lock, and the checkpoint cannot take the turn before that lock is released.
    #[tokio::test]
    async fn an_index_close_behind_a_held_write_lock_is_recorded_in_the_metrics_database()
    -> Result<(), Box<dyn std::error::Error>> {
        use rift_index::{
            DatabaseName, DatabasePool, LazyDatabase, LexicalIndexLimits, LexicalSearchIndex,
            WorkspaceDatabase,
        };

        /// Failure bound on one wait in this case; never a way to order two events.
        const STEP_MAX: Duration = Duration::from_secs(10);
        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        std::fs::create_dir_all(&state_directory)?;
        let metrics_path = state_directory.join("metrics");
        let store = Arc::new(rift_tracing::LogStore::open(&metrics_path, None).await?);
        let (recorder, drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let running = rift_tracing::RunningLogDrain::spawn(drain, Arc::clone(&store), 1_000);
        let index_path = DatabaseName::Index.path(&state_directory);
        let database = WorkspaceDatabase::open(
            &index_path,
            DatabaseName::Index,
            DatabasePool::new(4, 30_000),
        )
        .await?;
        let holder = rusqlite::Connection::open(&index_path)?;
        holder.execute_batch("BEGIN IMMEDIATE")?;
        let lexical =
            LexicalSearchIndex::attached(Arc::clone(&database), LexicalIndexLimits::default());
        let writing = tokio::spawn(async move { lexical.replace_all(&[], "held").await });
        tokio::time::timeout(STEP_MAX, async {
            while recorder
                .metrics()
                .find(
                    "traces.span.metrics.calls",
                    &[
                        ("span.name", "lexical.write_turn"),
                        ("span.kind", "Internal"),
                        ("status.code", "Ok"),
                    ],
                )
                .is_none()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|_elapsed| "the lexical write never took the write turn")?;
        let vectors = Arc::new(LazyDatabase::new(
            &DatabaseName::Vectors.path(&state_directory),
            DatabaseName::Vectors,
            None,
        ));
        let search = rift_search::SearchIndex::attached(
            database,
            vectors,
            rift_search::SearchIndexLimits::default(),
        )?;
        let mut shutdown = super::DeferredDatabaseShutdown(Some(Arc::new(search)), None);

        shutdown
            .close_search(Instant::now() + Duration::from_millis(200))
            .await?;
        let unwritten = running.stop(Instant::now() + STEP_MAX).await;
        drop(recorder);
        holder.execute_batch("ROLLBACK")?;
        let _refused = tokio::time::timeout(STEP_MAX, writing).await??;
        store.close(Instant::now() + STEP_MAX).await?;

        assert_eq!(unwritten, None, "the final flush writes every record");
        let stored = rift_tracing::LogReader::new(&metrics_path)
            .connect()?
            .recent(&rift_tracing::LogQuery::newest(1_000))?;
        let closes = stored
            .iter()
            .map(rift_tracing::StoredLogRecord::record)
            .filter(|record| record.operation() == "database.close")
            .map(|record| {
                let fields: serde_json::Value = serde_json::from_str(record.fields())?;
                Ok((
                    record.level().to_owned(),
                    record.message().to_owned(),
                    fields,
                ))
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        let outlasted = closes
            .iter()
            .find(|(_, message, _)| {
                message.starts_with("database checkpoint outlasted the shutdown deadline")
            })
            .ok_or_else(|| format!("the metrics database holds the index close: {closes:?}"))?;
        assert_eq!(outlasted.0, "warn");
        assert_eq!(outlasted.2["database"], "index", "{:?}", outlasted.2);
        Ok(())
    }
}
