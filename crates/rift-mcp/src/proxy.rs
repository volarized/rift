//! Stateless stdio proxy to the workspace's elected rift server.
//!
//! `rift mcp` serves agents over stdio while the workspace's state lives in
//! one `rift server` process. Every request forwards to that server: the
//! proxy adopts a recorded server when the lock document names a live one,
//! starts a detached server when the workspace has none, and reconnects -
//! single-flight, one retry per request - when the server it held goes
//! away, so an agent session survives server restarts.

use std::cmp::Ordering;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::http::{HeaderName, HeaderValue};
use rift_core::CapturedStream;
use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_error::{RiftError, errors};
use rift_protocol::configuration::ServerConfiguration;
use rift_protocol::error as wire;
use rift_protocol::lock::{ProductIdentity, ServerLock};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, Extensions,
    Implementation, ListResourceTemplatesRequest, ListResourceTemplatesRequestMethod,
    ListResourceTemplatesResult, ListResourcesRequest, ListResourcesRequestMethod,
    ListResourcesResult, ListToolsRequest, ListToolsRequestMethod, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequest, ReadResourceRequestParams, ReadResourceResponse,
    ServerCapabilities, ServerConfig, ServerPeerInfo, ServerResult,
};
use rmcp::service::{
    Peer, PeerRequestOptions, QuitReason, RequestContext, RoleClient, RoleServer, RunningService,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, ServerHandler, ServiceError, ServiceExt as _};
use semver::Version;
use tracing::Instrument as _;

use crate::election::{ServerPresence, StaleReason, probe, probe_state_directory};
use crate::failure::{McpErrorExt as _, McpErrorFailExt as _, WireFailure as _};
use crate::http::{MCP_PATH, StopRequestFailure, WORKSPACE_ROOT_HEADER, request_stop};
use crate::identity::BuildCheckout;
use crate::repository::{
    ServerConfigurationSelection, repository_election_directory, select_server_configuration,
};
use crate::spawn::{
    PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, SpawnPollOutcome,
    StartSpawns, StartupCapture,
};
use crate::validation::ConfigurationState;

/// The suffix `SQLite` gives a WAL database's write-ahead log, appended to the
/// database file's name.
const WRITE_AHEAD_LOG_SUFFIX: &str = "-wal";

/// Bound on one upstream connect-and-initialize attempt.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Allowance past the server's own request bounds for its answer to be
/// rendered and to travel back to the proxy.
const FORWARD_ANSWER_GRACE: Duration = Duration::from_secs(10);

/// Serves agents over stdio MCP, forwarding every request to the
/// workspace's elected server, as the build `checkout` describes.
///
/// Serving starts immediately: a background warmup attempts the first
/// upstream connect - starting a server when the workspace has none - and
/// each request that arrives earlier waits on the same single-flight
/// connect. Stdout carries protocol frames only; diagnostics go to
/// tracing/stderr, and the upstream bearer token appears in neither.
///
/// # Errors
///
/// Returns a registered error for initialization or service-task failure.
///
/// # Cancel safety
///
/// Dropping this future closes the owned MCP service and the upstream
/// connection; the detached server keeps serving the workspace.
pub async fn serve_proxy(root: &Path, checkout: BuildCheckout) -> Result<(), RiftError> {
    tracing::info!(component = "mcp", transport = "stdio", "MCP proxy starting");
    let identity = crate::identity::product_identity(checkout)
        .await
        .map_err(|error| errors::mcp::proxy_identity_failed().source(error).error())?;
    let proxy = RiftProxy::new(root, identity);
    let warmup = tokio::spawn(warm_up(proxy.clone()));
    let outcome = Box::pin(serve_connection(proxy, crate::transport::guarded_stdio())).await;
    warmup.abort();
    outcome
}

/// Serves one proxy over `transport` through initialization, quit, and
/// shutdown.
///
/// Split from [`serve_proxy`] so initialization failure and quit handling
/// are testable without live stdio I/O.
async fn serve_connection<Transport, TransportError, Adapter>(
    proxy: RiftProxy,
    transport: Transport,
) -> Result<(), RiftError>
where
    Transport: rmcp::transport::IntoTransport<RoleServer, TransportError, Adapter>,
    TransportError: std::error::Error + Send + Sync + 'static,
{
    let service = proxy.serve(transport).await.map_err(|error| {
        errors::mcp::proxy_initialization_failed()
            .source(error)
            .error()
    })?;
    tracing::info!(component = "mcp", transport = "stdio", "MCP proxy ready");
    let reason = service.waiting().await;
    let outcome = reason
        .map_err(|error| errors::mcp::proxy_task_failed().source(error).error())
        .and_then(quit_reason_result);
    tracing::info!(
        component = "mcp",
        transport = "stdio",
        outcome = if outcome.is_ok() { "ok" } else { "error" },
        "MCP proxy stopped"
    );
    outcome
}

/// Maps a service quit reason to its outcome.
///
/// Split from [`serve_connection`] so the mapping is testable without live
/// stdio I/O.
fn quit_reason_result(reason: QuitReason) -> Result<(), RiftError> {
    match reason {
        QuitReason::Closed | QuitReason::Cancelled => Ok(()),
        QuitReason::JoinError(error) => errors::mcp::proxy_task_failed().source(error).fail(),
        _ => errors::mcp::proxy_unexpected_quit().fail(),
    }
}

/// Attempts the first upstream connect before any request needs it.
///
/// Best effort: a workspace that cannot produce a server yet only logs -
/// the agent session still starts, and each request surfaces its own
/// refusal until the server comes up.
///
/// # Cancel safety
///
/// Dropping this future abandons the warmup; the next request connects on
/// demand through the same single-flight slot.
async fn warm_up(proxy: RiftProxy) {
    if let Err(refusal) = proxy.leased_peer(None).await {
        tracing::warn!(
            component = "mcp",
            refusal = %refusal.message,
            "upstream warmup did not connect"
        );
    }
}

/// The stdio-facing proxy: forwards agent traffic to one upstream server.
///
/// Clones share the upstream slot and the advertised info, so the warmup
/// task and the serving handler act on one connection.
#[derive(Clone, Debug)]
struct RiftProxy {
    root: Arc<Path>,
    identity: Arc<ProductIdentity>,
    upstream: Arc<tokio::sync::Mutex<UpstreamSlot>>,
    advertised: Arc<std::sync::Mutex<Option<ServerConfig>>>,
    /// How long one forwarded request may go unanswered: see [`forward_budget`].
    forward_budget: Duration,
}

/// The proxy's one upstream connection and the generation counter that
/// keeps reconnects single-flight.
#[derive(Debug)]
struct UpstreamSlot {
    connected: Option<Upstream>,
    connecting: Option<Arc<ConnectAttempt>>,
    generation_next: u64,
}

/// One connection result shared by callers that arrived while it ran.
#[derive(Debug)]
struct ConnectAttempt {
    result: std::sync::Mutex<Option<Result<(), ErrorData>>>,
    finished: tokio::sync::Notify,
}

impl ConnectAttempt {
    fn pending() -> Self {
        Self {
            result: std::sync::Mutex::new(None),
            finished: tokio::sync::Notify::new(),
        }
    }

    fn complete(&self, result: Result<(), ErrorData>) {
        *self.lock_result() = Some(result);
        self.finished.notify_waiters();
    }

    async fn outcome(&self) -> Result<(), ErrorData> {
        loop {
            let notified = self.finished.notified();
            if let Some(result) = self.lock_result().clone() {
                return result;
            }
            notified.await;
        }
    }

    fn lock_result(&self) -> std::sync::MutexGuard<'_, Option<Result<(), ErrorData>>> {
        match self.result.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl UpstreamSlot {
    fn empty() -> Self {
        Self {
            connected: None,
            connecting: None,
            generation_next: 0,
        }
    }

    fn begin_connection(&mut self) -> (Arc<ConnectAttempt>, bool) {
        if let Some(attempt) = &self.connecting {
            return (Arc::clone(attempt), false);
        }
        let attempt = Arc::new(ConnectAttempt::pending());
        self.connecting = Some(Arc::clone(&attempt));
        (attempt, true)
    }

    fn finish_connection(&mut self, attempt: &Arc<ConnectAttempt>) {
        if self
            .connecting
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(running, attempt))
        {
            self.connecting = None;
        }
    }
}

/// One live upstream connection. Dropping it cancels the connection's
/// transport tasks, so a replaced upstream never leaks its dead connection.
#[derive(Debug)]
struct Upstream {
    running: RunningService<RoleClient, ()>,
    generation: u64,
}

impl RiftProxy {
    /// A proxy for the workspace at `root`, bounding its forwards by the
    /// `[server]` table the workspace accepts when the proxy starts.
    fn new(root: &Path, identity: ProductIdentity) -> Self {
        let server = ConfigurationState::accept(root).server_configuration();
        Self {
            root: Arc::from(root),
            identity: Arc::new(identity),
            upstream: Arc::new(tokio::sync::Mutex::new(UpstreamSlot::empty())),
            advertised: Arc::new(std::sync::Mutex::new(None)),
            forward_budget: forward_budget(&server),
        }
    }

    /// A peer for one forward attempt, plus the generation it belongs to.
    ///
    /// `observed` is the generation whose connection failed the caller's
    /// previous attempt, or `None` on a first attempt. The slot's mutex is
    /// held only to clone the peer or join one shared connection attempt -
    /// never across a connect or forward. Concurrent callers receive the
    /// same terminal connection result, including a refusal.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future leaves its bounded shared connection attempt
    /// running for other callers.
    async fn leased_peer(
        &self,
        observed: Option<u64>,
    ) -> Result<(Peer<RoleClient>, u64), ErrorData> {
        loop {
            let (attempt, starts) = {
                let mut slot = self.upstream.lock().await;
                if let Some(current) = &slot.connected
                    && reuse_current(current.generation, observed)
                {
                    return Ok((current.running.peer().clone(), current.generation));
                }
                // A connection matching `observed` failed the caller. Drop
                // it before joining or starting its replacement.
                drop(slot.connected.take());
                slot.begin_connection()
            };
            if starts {
                self.start_connection(Arc::clone(&attempt));
            }
            attempt.outcome().await?;
        }
    }

    /// Starts one bounded connection attempt and publishes its result to
    /// every caller that joined it.
    fn start_connection(&self, attempt: Arc<ConnectAttempt>) {
        let proxy = self.clone();
        tokio::spawn(async move {
            let connected = connect_upstream(&proxy.root, &proxy.identity).await;
            let result = match connected {
                Ok(running) => {
                    proxy.mirror_advertised(&running);
                    let mut slot = proxy.upstream.lock().await;
                    let generation = slot.generation_next;
                    slot.generation_next += 1;
                    slot.connected = Some(Upstream {
                        running,
                        generation,
                    });
                    slot.finish_connection(&attempt);
                    Ok(())
                }
                Err(refusal) => {
                    proxy.upstream.lock().await.finish_connection(&attempt);
                    Err(refusal)
                }
            };
            attempt.complete(result);
        });
    }

    /// Forwards one request to the upstream with a single reconnect retry,
    /// reading its answer through `answer`.
    ///
    /// A transport-shaped failure marks the held connection dead and
    /// retries exactly once on a freshly leased connection; every other
    /// failure maps straight through [`forwarded_error`], and an answer of
    /// another kind than `answer` reads refuses as an unexpected response.
    /// Each send is bounded by the proxy's forward budget, and a send that
    /// outlives it is cancelled on the server and refuses as
    /// the registered forwarding error.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future abandons the forward; a reconnect another
    /// request started is unaffected.
    async fn forward<Value>(
        &self,
        request: ClientRequest,
        answer: fn(ServerResult) -> Option<Value>,
    ) -> Result<Value, ErrorData> {
        let (peer, generation) = self.leased_peer(None).await?;
        let failure = match self.answered(&peer, request.clone()).await? {
            Ok(result) => return answered_as(result, answer),
            Err(error) if transport_failed(&error) => error,
            Err(error) => return forwarded_error(error).fail(),
        };
        tracing::info!(
            component = "mcp",
            failure = %failure,
            "upstream connection lost; reconnecting"
        );
        let (peer, _generation) = self.leased_peer(Some(generation)).await?;
        let result = self
            .answered(&peer, request)
            .await?
            .map_err(forwarded_error)?;
        answered_as(result, answer)
    }

    /// What one send answered, or the refusal for a server that did not
    /// answer it within the forward budget.
    ///
    /// The request goes out cancellable under the budget. When the budget
    /// ends, rmcp's `RequestHandle::await_response` cancels the request:
    /// over Streamable HTTP that closes the request's response stream, which
    /// the MCP specification makes the cancellation signal, so the server
    /// stops the request instead of answering into a dropped channel.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future drops the send before its budget ends; the
    /// request's response stream closes with it.
    async fn answered(
        &self,
        peer: &Peer<RoleClient>,
        request: ClientRequest,
    ) -> Result<Result<ServerResult, ServiceError>, ErrorData> {
        let budget = self.forward_budget;
        let answer = match peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(budget))
            .await
        {
            Ok(handle) => {
                tracing::debug!(component = "mcp", upstream_request_id = %handle.id, "forwarded request awaiting response");
                let result = handle.await_response().await;
                tracing::debug!(
                    component = "mcp",
                    is_error = result.is_err(),
                    "forwarded request completed"
                );
                result
            }
            Err(error) => Err(error),
        };
        match answer {
            Err(ServiceError::Timeout { .. }) => {
                tracing::warn!(
                    component = "mcp",
                    budget = ?budget,
                    "the workspace server did not answer a forwarded request within its budget; \
                     the request is cancelled"
                );
                errors::mcp::forward_unanswered()
                    .waited(budget)
                    .mcp()
                    .tool_error(wire::ErrorPhase::Read)
                    .fail()
            }
            answered => Ok(answered),
        }
    }

    /// The info advertised downstream: the upstream's mirrored
    /// advertisement once a connect succeeded, and a tools-enabled fallback
    /// naming this binary before then.
    fn advertised_info(&self) -> ServerConfig {
        self.lock_advertised()
            .clone()
            .unwrap_or_else(|| fallback_info(&self.identity))
    }

    /// Records the connected upstream's advertisement for `get_info`.
    fn mirror_advertised(&self, running: &RunningService<RoleClient, ()>) {
        let Some(peer_info) = running.peer_info() else {
            return;
        };
        *self.lock_advertised() = Some(mirrored_info(&peer_info));
    }

    /// The advertised slot, recovered from a poisoned lock: the stored
    /// value is plain data, valid regardless of a panicked writer.
    fn lock_advertised(&self) -> std::sync::MutexGuard<'_, Option<ServerConfig>> {
        match self.advertised.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Bound on one forwarded request: the two waits the server bounds one
/// request by - `[server] readiness_timeout` for the workspace to be ready,
/// `[server] worker_queue_timeout` for a free worker - and
/// `FORWARD_ANSWER_GRACE` for the answer.
///
/// A server that has not answered within it has stopped answering, and the
/// caller gets a refusal it can retry instead of a request that never ends.
/// A caller that bounds the same request from outside the proxy reads the
/// budget here, so its own bound can end after the proxy's.
#[must_use]
pub fn forward_budget(server: &ServerConfiguration) -> Duration {
    Duration::from_millis(server.readiness_timeout.milliseconds())
        .saturating_add(Duration::from_millis(
            server.worker_queue_timeout.milliseconds(),
        ))
        .saturating_add(FORWARD_ANSWER_GRACE)
}

/// The value `answer` reads from one upstream result, or the refusal for a
/// result of another kind than the request asked for.
fn answered_as<Value>(
    result: ServerResult,
    answer: fn(ServerResult) -> Option<Value>,
) -> Result<Value, ErrorData> {
    answer(result).ok_or_else(|| forwarded_error(ServiceError::UnexpectedResponse))
}

/// Whether a request may reuse the slot's current upstream instead of
/// reconnecting.
///
/// `observed` is the generation whose connection failed the request, or
/// `None` when the request has not held one. A current generation that
/// differs from the observed one was connected after the observed failure,
/// so it is fresh; the observed generation itself is the connection that
/// just failed and must be replaced.
fn reuse_current(current_generation: u64, observed: Option<u64>) -> bool {
    observed != Some(current_generation)
}

/// Connects to the workspace's serving server, electing one when needed.
///
/// One adoption attempt against the recorded server first; a workspace
/// without a live server gets a detached spawn, its startup stderr
/// captured, then a poll of at most [`START_POLL_ATTEMPT_COUNT`] probes at
/// [`PRESENCE_POLL_INTERVAL`], each adoption bounded by
/// [`UPSTREAM_CONNECT_TIMEOUT`]. The poll also stops at the
/// [`START_WAIT_MAX`] deadline, so slow connect attempts shorten the
/// attempt count instead of stretching the window. Connect failures inside
/// the window keep polling. A spawned server that exits before the window
/// closes is weighed by [`StartSpawns::poll`]: while another process holds
/// the election the poll waits for that holder; once none does, a lost
/// election spawns again, at most
/// [`START_SPAWN_COUNT_MAX`](crate::spawn::START_SPAWN_COUNT_MAX) spawns in
/// all, and any other exit refuses with the server's captured stderr.
/// When the window closes on a holder of the election that is still building
/// its first index and wrote to the workspace database during the window,
/// the refusal is one the caller resends; any other exhaustion refuses with
/// the operator's next step.
///
/// A recorded server of another identity is weighed by [`ServerStanding`].
/// One this process replaces is asked to stop first, and the spawn waits
/// until that server has released the election, so the new server never
/// opens the workspace beside the one leaving it. A stop request that failed
/// in transport decides nothing: the poll reads the election as it does after
/// an accepted one, and refuses with the identity refusal only when the window
/// closes on that same server still serving.
///
/// # Cancel safety
///
/// Dropping this future abandons the connect; a spawned server keeps
/// serving and the next attempt adopts it. A server already asked to stop
/// stops.
async fn connect_upstream(
    root: &Path,
    identity: &ProductIdentity,
) -> Result<RunningService<RoleClient, ()>, ErrorData> {
    if let Some(running) = connect_repository_server(root, identity).await? {
        return Ok(running);
    }
    let mut replacement = Replacement::default();
    if let Some(running) = adopt_serving(root, identity, &mut replacement).await? {
        return Ok(running);
    }
    // A server another starter elected is still building: a spawn now would
    // only lose the election, so the poll below waits for that server's
    // document instead. Otherwise a lost spawn race is fine - this
    // process's own spawned server finds the workspace already served, exits
    // on its own, and the poll adopts the winner once it finishes binding. A
    // spawn that cannot launch at all is reported, and the poll still gives
    // a concurrently started server its chance.
    let mut spawns = StartSpawns::<StartupCapture>::default();
    let mut awaiting_release = replacement.is_asked();
    if !awaiting_release && !matches!(probed(root).await, ServerPresence::Starting) {
        spawns.spawn_captured(root);
    }
    let opened = DatabaseActivity::observed(root).await;
    let mut building = false;
    let mut still_serving = None;
    let deadline = tokio::time::Instant::now() + START_WAIT_MAX;
    for _ in 0..START_POLL_ATTEMPT_COUNT {
        let presence = probed(root).await;
        building = matches!(presence, ServerPresence::Starting);
        let election_held = presence.election_held();
        still_serving = replacement.refusal_while_serving(&presence, identity);
        let adopted = adopt_presence(root, presence, identity, &mut replacement).await?;
        // The replaced server released the election, and no other starter
        // took it: this process starts the server it replaces it with.
        if awaiting_release && !election_held {
            awaiting_release = false;
            spawns.spawn_captured(root);
        }
        match spawns.poll(adopted, election_held) {
            SpawnPollOutcome::Ready(running) => return Ok(running),
            SpawnPollOutcome::Failed(capture) => return server_start_failed(&capture).fail(),
            SpawnPollOutcome::ElectionUnheld => spawns.spawn_captured(root),
            SpawnPollOutcome::Waiting => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    if let Some(refusal) = still_serving {
        return refusal.fail();
    }
    let closed = DatabaseActivity::observed(root).await;
    start_window_refusal(building && opened != closed).fail()
}

/// What the workspace database's files looked like at one instant: each
/// file's length and modification time, or nothing for an absent file.
///
/// A server records its diagnostics into `.rift/db` from before it claims the
/// election, while it builds its first index too, and `SQLite` in WAL mode
/// appends each commit to the write-ahead log. Two readings that differ
/// therefore show that a process wrote between them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DatabaseActivity {
    files: [Option<(u64, SystemTime)>; 2],
}

impl DatabaseActivity {
    /// Reads the database file and its write-ahead log below `root`, and none
    /// of their bytes, on the blocking pool. A reading the pool could not
    /// finish names no file.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future abandons the reading; it changes nothing.
    async fn observed(root: &Path) -> Self {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || Self::read(&root))
            .await
            .unwrap_or(Self {
                files: [None, None],
            })
    }

    /// Reads the database file and its write-ahead log below `root`.
    fn read(root: &Path) -> Self {
        let state = root.join(RIFT_STATE_DIRECTORY);
        let database = state.join(WORKSPACE_DATABASE_FILE_NAME);
        let log = state.join(format!(
            "{WORKSPACE_DATABASE_FILE_NAME}{WRITE_AHEAD_LOG_SUFFIX}"
        ));
        Self {
            files: [database, log].map(|path| {
                let metadata = std::fs::metadata(path).ok()?;
                Some((metadata.len(), metadata.modified().ok()?))
            }),
        }
    }
}

/// The refusal a request gets when the start window closed without a server
/// that answers: one the caller resends while the holder of the election is
/// `building` its first index and wrote during the window, and the
/// operator's next step otherwise.
fn start_window_refusal(building: bool) -> ErrorData {
    if building {
        return errors::mcp::start_building()
            .waited(START_WAIT_MAX)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
    }
    ErrorData::internal_error(
        format!(
            "no rift server answered for this workspace within {START_WAIT_MAX:?}; the \
             workspace has no server this caller can reach, and starting one is an \
             operator action on this host"
        ),
        None,
    )
}

/// The refusal a request gets when the spawned server exited before it
/// published a live connection.
///
/// Names what the server printed to its own standard error. The caller
/// has no shell channel of its own, so it learns what actually failed
/// instead of being pointed at a command it cannot run.
fn server_start_failed(capture: &CapturedStream) -> ErrorData {
    let failure = if capture.text.is_empty() {
        errors::mcp::spawn_no_output().mcp()
    } else {
        errors::mcp::spawn_failed()
            .stderr(capture.text.clone())
            .stderr_truncated(capture.truncated)
            .mcp()
    };
    failure.tool_error(wire::ErrorPhase::Read)
}

/// The recorded serving server as a live connection, when both halves hold.
///
/// A probe that names no serving server, and a recorded server that
/// refuses the connect or initialize, both answer `None`: the recorded
/// state is stale and the caller elects afresh. A server of another
/// identity answers as [`ServerStanding`] decides.
async fn adopt_serving(
    root: &Path,
    identity: &ProductIdentity,
    replacement: &mut Replacement,
) -> Result<Option<RunningService<RoleClient, ()>>, ErrorData> {
    adopt_presence(root, probed(root).await, identity, replacement).await
}

/// One presence probe, off the async runtime.
///
/// A probe connects to the recorded port and can spend its whole connect
/// bound doing it: on Windows a connect to a port nothing listens on keeps
/// retrying until that bound passes. The probe therefore runs on the
/// blocking pool, so the start poll never holds a runtime worker for it. A
/// probe that panicked answers that the election's state could not be
/// observed.
///
/// # Cancel safety
///
/// Dropping this future abandons the answer; the probe itself finishes on
/// the blocking pool and changes nothing.
async fn probed(root: &Path) -> ServerPresence {
    probed_with(root, probe).await
}

/// [`probed`] over any probe, so a test can hold one probe open.
async fn probed_with(
    root: &Path,
    probe: impl FnOnce(&Path) -> ServerPresence + Send + 'static,
) -> ServerPresence {
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || probe(&root))
        .await
        .unwrap_or(ServerPresence::Stale(StaleReason::ElectionUnobservable))
}

/// The serving server one probe found, as a live connection, under the
/// same rules as [`adopt_serving`].
///
/// Split from it so a caller that also weighs the probe's election state
/// reads both from one probe.
async fn adopt_presence(
    root: &Path,
    presence: ServerPresence,
    identity: &ProductIdentity,
    replacement: &mut Replacement,
) -> Result<Option<RunningService<RoleClient, ()>>, ErrorData> {
    let lock = match presence {
        ServerPresence::Serving(lock) => lock,
        ServerPresence::Stale(StaleReason::PortUnreachable { pid }) => {
            tracing::info!(
                component = "mcp",
                pid,
                "recorded server did not answer; treating the lock as stale"
            );
            return Ok(None);
        }
        ServerPresence::Starting | ServerPresence::Stale(_) | ServerPresence::Absent => {
            return Ok(None);
        }
    };
    match ServerStanding::of(identity, &lock.identity) {
        ServerStanding::Adopt => {}
        ServerStanding::Replace => {
            replacement.replace(&lock, identity).await?;
            return Ok(None);
        }
        ServerStanding::Refuse => return identity_refusal(identity, &lock).fail(),
    }
    match connect_recorded_for_root(&lock, UPSTREAM_CONNECT_TIMEOUT, root).await {
        Ok(running) => {
            tracing::info!(
                component = "mcp",
                port = lock.port,
                pid = lock.pid,
                "proxy connected to workspace server"
            );
            Ok(Some(running))
        }
        Err(failure) => {
            let detail = failure.detail();
            tracing::info!(
                component = "mcp",
                %detail,
                "recorded server did not answer; treating the lock as stale"
            );
            Ok(None)
        }
    }
}

async fn connect_repository_server(
    workspace_root: &Path,
    identity: &ProductIdentity,
) -> Result<Option<RunningService<RoleClient, ()>>, ErrorData> {
    let root = workspace_root.to_path_buf();
    let selection = tokio::task::spawn_blocking(move || select_server_configuration(&root, None))
        .await
        .ok()
        .and_then(Result::ok);
    let Some(ServerConfigurationSelection::Repository {
        common_directory,
        server,
        ..
    }) = selection
    else {
        return Ok(None);
    };
    let Some(state_directory) = repository_election_directory(&common_directory, identity).ok()
    else {
        return Ok(None);
    };
    let presence = tokio::task::spawn_blocking(move || probe_state_directory(&state_directory))
        .await
        .unwrap_or(ServerPresence::Stale(StaleReason::ElectionUnobservable));
    let ServerPresence::Serving(lock) = presence else {
        return Ok(None);
    };
    if lock.server.as_ref() != Some(&server)
        || !matches!(
            ServerStanding::of(identity, &lock.identity),
            ServerStanding::Adopt
        )
    {
        return Ok(None);
    }
    match connect_recorded_for_root(&lock, UPSTREAM_CONNECT_TIMEOUT, workspace_root).await {
        Ok(running) => Ok(Some(running)),
        Err(_) => Ok(None),
    }
}

/// What a proxy does with a recorded server, by the two product identities.
///
/// Versions order by semantic-versioning precedence, which leaves build
/// metadata aside, so two builds of one version stand level. Every release
/// bumps the version, so a proxy of a new release replaces the server of the
/// one before it, while two development builds of one version never replace
/// each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServerStanding {
    /// The identities are equal: the proxy forwards to the server.
    Adopt,
    /// The server's version strictly precedes this process's: the proxy asks
    /// it to stop and starts its own.
    Replace,
    /// The server's version equals this process's from another build, or
    /// follows it: the proxy refuses, so a server is only ever displaced by
    /// a newer release.
    Refuse,
}

impl ServerStanding {
    /// How the server recording `theirs` stands against `ours`. A version
    /// either side cannot parse orders nothing, and the proxy refuses.
    fn of(ours: &ProductIdentity, theirs: &ProductIdentity) -> Self {
        if ours == theirs {
            return Self::Adopt;
        }
        let (Ok(ours), Ok(theirs)) = (
            Version::parse(&ours.version),
            Version::parse(&theirs.version),
        ) else {
            return Self::Refuse;
        };
        match theirs.cmp_precedence(&ours) {
            Ordering::Less => Self::Replace,
            Ordering::Equal | Ordering::Greater => Self::Refuse,
        }
    }
}

/// The one server a connect asked to stop, by the pid and token its lock
/// records, and whether it accepted the request.
///
/// A server mints its token when it starts, so the pair names one server:
/// a later probe that reads it again sees that server leaving, and one that
/// reads another pair sees a server started after it.
#[derive(Debug, Default)]
struct Replacement {
    /// The lock of the server this connect asked to stop.
    asked: Option<ServerLock>,
    /// Whether that server answered the stop request with acceptance. A
    /// request that failed in transport leaves it unknown: a server already
    /// stopping closes the connection as it exits.
    accepted: bool,
}

impl Replacement {
    /// Whether this connect asked a server to stop.
    const fn is_asked(&self) -> bool {
        self.asked.is_some()
    }

    /// Asks the server `lock` records to stop so this process can serve the
    /// workspace, once per connect.
    ///
    /// The server this connect already asked answers `Ok` while it leaves.
    /// A second older server is refused rather than stopped: another process
    /// of an older release started it after the replaced one left, and
    /// stopping it in turn would only race that process for the election.
    /// A stop request that failed in transport counts the server as asked:
    /// the next reading of the election decides, since a server already
    /// stopping closes the connection as it exits, which Windows reports as a
    /// reset.
    ///
    /// # Errors
    ///
    /// Returns the identity refusal for a second server, and for a server
    /// that answered the stop request with a refusal.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future abandons the request; a request the server
    /// already received still stops it.
    async fn replace(
        &mut self,
        lock: &ServerLock,
        identity: &ProductIdentity,
    ) -> Result<(), ErrorData> {
        match &self.asked {
            Some(asked) if names_one_server(asked, lock) => return Ok(()),
            Some(_) => return identity_refusal(identity, lock).fail(),
            None => {}
        }
        match request_stop(lock).await {
            Ok(()) => self.accepted = true,
            Err(StopRequestFailure::Failed(failure)) => tracing::warn!(
                component = "mcp",
                pid = lock.pid,
                failure = ?failure,
                "the stop request to the workspace server failed in transport; the election decides"
            ),
            Err(failure @ StopRequestFailure::Refused(_)) => {
                tracing::warn!(
                    component = "mcp",
                    pid = lock.pid,
                    failure = ?failure,
                    "the workspace server did not accept the stop request"
                );
                return identity_refusal(identity, lock).fail();
            }
        }
        let server_version = lock.identity.version.as_str();
        tracing::info!(
            component = "mcp",
            pid = lock.pid,
            server_version,
            version = identity.version.as_str(),
            "asked the workspace server to stop so this rift process can serve the workspace"
        );
        self.asked = Some(lock.clone());
        Ok(())
    }

    /// The identity refusal for the server this connect asked to stop, while
    /// `presence` shows it still serving and it never accepted the request.
    fn refusal_while_serving(
        &self,
        presence: &ServerPresence,
        identity: &ProductIdentity,
    ) -> Option<ErrorData> {
        let (Some(asked), ServerPresence::Serving(lock)) = (&self.asked, presence) else {
            return None;
        };
        (!self.accepted && names_one_server(asked, lock)).then(|| identity_refusal(identity, lock))
    }
}

/// Whether two locks name one server: its pid and the token it minted.
fn names_one_server(asked: &ServerLock, lock: &ServerLock) -> bool {
    asked.pid == lock.pid && asked.token == lock.token
}

/// The refusal for a serving process whose build or served tools differ
/// and that this process does not replace.
fn identity_refusal(expected: &ProductIdentity, lock: &ServerLock) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "workspace server identity differs from this rift process: server pid {}, version {}, schema digest {}; this process version {}, schema digest {}; run rift server stop, then retry",
            lock.pid,
            lock.identity.version,
            lock.identity.schema_digest,
            expected.version,
            expected.schema_digest,
        ),
        None,
    )
}

/// Why one bounded connect attempt produced no live connection.
#[derive(Debug)]
enum ConnectAttemptFailure {
    /// The transport refused or MCP initialization failed. Boxed to keep
    /// this failure small beside the large initialize error.
    Initialize(Box<dyn std::error::Error + Send + Sync>),
    /// The attempt outlived [`UPSTREAM_CONNECT_TIMEOUT`].
    TimedOut,
}

impl ConnectAttemptFailure {
    /// One line naming what kept the attempt from connecting.
    fn detail(&self) -> String {
        match self {
            Self::Initialize(error) => error.to_string(),
            Self::TimedOut => format!("connect timed out after {UPSTREAM_CONNECT_TIMEOUT:?}"),
        }
    }
}

/// Connects and initializes against one recorded server, bounded by
/// `timeout`.
///
/// The caller passes [`UPSTREAM_CONNECT_TIMEOUT`].
#[cfg(test)]
async fn connect_recorded(
    lock: &ServerLock,
    timeout: Duration,
) -> Result<RunningService<RoleClient, ()>, ConnectAttemptFailure> {
    connect_recorded_with_root(lock, timeout, None).await
}

async fn connect_recorded_for_root(
    lock: &ServerLock,
    timeout: Duration,
    root: &Path,
) -> Result<RunningService<RoleClient, ()>, ConnectAttemptFailure> {
    connect_recorded_with_root(lock, timeout, Some(root)).await
}

async fn connect_recorded_with_root(
    lock: &ServerLock,
    timeout: Duration,
    root: Option<&Path>,
) -> Result<RunningService<RoleClient, ()>, ConnectAttemptFailure> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(format!(
        "http://127.0.0.1:{port}{MCP_PATH}",
        port = lock.port
    ))
    .auth_header(lock.token.clone());
    if let Some(root) = root {
        let canonical = std::fs::canonicalize(root)
            .map_err(|error| ConnectAttemptFailure::Initialize(Box::new(error)))?;
        let value = HeaderValue::from_str(&canonical.to_string_lossy())
            .map_err(|error| ConnectAttemptFailure::Initialize(Box::new(error)))?;
        config
            .custom_headers
            .insert(HeaderName::from_static(WORKSPACE_ROOT_HEADER), value);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    match tokio::time::timeout(timeout, ().serve(transport)).await {
        Ok(Ok(running)) => Ok(running),
        Ok(Err(error)) => Err(ConnectAttemptFailure::Initialize(Box::new(error))),
        Err(_elapsed) => Err(ConnectAttemptFailure::TimedOut),
    }
}

/// Whether a forward failure means the held upstream connection is gone.
fn transport_failed(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::TransportClosed | ServiceError::TransportSend(_)
    )
}

/// The JSON-RPC error a downstream client gets for one upstream failure.
///
/// A refusal the server classified travels unchanged, refusal code and
/// all. A transport-shaped failure reaches here only after its one
/// reconnect retry also failed, and names what the operator can check;
/// every other failure names what the upstream did.
fn forwarded_error(error: ServiceError) -> ErrorData {
    match error {
        ServiceError::McpError(data) => data,
        ServiceError::TransportClosed | ServiceError::TransportSend(_) => {
            ErrorData::internal_error(
                format!(
                    "the workspace's rift server dropped the connection and a reconnect \
                     did not recover it: {error}; check the server with \
                     `rift server start --foreground`"
                ),
                None,
            )
        }
        other => ErrorData::internal_error(
            format!("the workspace's rift server failed the forwarded request: {other}"),
            None,
        ),
    }
}

/// The advertisement served before the first successful upstream connect.
fn fallback_info(identity: &ProductIdentity) -> ServerConfig {
    let mut info = ServerConfig::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .build(),
    )
    .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")));
    info.meta = Some(crate::identity::identity_meta(identity));
    info
}

/// The upstream's negotiated facts as this proxy's own advertisement.
///
/// The protocol version is a starting point only: initialize re-negotiates
/// it against the downstream client. An upstream that named no
/// implementation identity falls back to this binary's own.
fn mirrored_info(peer_info: &ServerPeerInfo) -> ServerConfig {
    let mut info = ServerConfig::new(peer_info.capabilities.clone())
        .with_protocol_version(peer_info.protocol_version.clone())
        .with_server_info(
            peer_info
                .server_info
                .clone()
                .unwrap_or_else(|| Implementation::new("rift", env!("CARGO_PKG_VERSION"))),
        );
    info.instructions.clone_from(&peer_info.instructions);
    info.meta.clone_from(&peer_info.meta);
    info
}

impl ServerHandler for RiftProxy {
    fn get_info(&self) -> ServerConfig {
        self.advertised_info()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.forward(list_tools_request(request), |result| match result {
            ServerResult::ListToolsResult(result) => Some(result),
            _ => None,
        })
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let span = tracing::info_span!(
            "mcp.forward",
            component = "mcp",
            operation = "tools/call",
            request_id = %context.id,
            tool = %request.name
        );
        async {
            tracing::debug!("tool forward started");
            let request = ClientRequest::CallToolRequest(CallToolRequest::new(request));
            let result = self
                .forward(request, |result| match result {
                    ServerResult::CallToolResult(result) => {
                        Some(CallToolResponse::Complete(result))
                    }
                    ServerResult::InputRequiredResult(result) => {
                        Some(CallToolResponse::InputRequired(result))
                    }
                    ServerResult::CreateTaskResult(result) => Some(CallToolResponse::Task(result)),
                    _ => None,
                })
                .await;
            tracing::debug!(is_error = result.is_err(), "tool forward completed");
            result
        }
        .instrument(span)
        .await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let request = ClientRequest::ListResourcesRequest(ListResourcesRequest {
            method: ListResourcesRequestMethod,
            params: request,
            extensions: Extensions::default(),
        });
        self.forward(request, |result| match result {
            ServerResult::ListResourcesResult(result) => Some(result),
            _ => None,
        })
        .await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let request = ClientRequest::ListResourceTemplatesRequest(ListResourceTemplatesRequest {
            method: ListResourceTemplatesRequestMethod,
            params: request,
            extensions: Extensions::default(),
        });
        self.forward(request, |result| match result {
            ServerResult::ListResourceTemplatesResult(result) => Some(result),
            _ => None,
        })
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(request));
        self.forward(request, |result| match result {
            ServerResult::ReadResourceResult(result) => {
                Some(ReadResourceResponse::Complete(result))
            }
            ServerResult::InputRequiredResult(result) => {
                Some(ReadResourceResponse::InputRequired(result))
            }
            _ => None,
        })
        .await
    }
}

/// The `tools/list` request the proxy forwards for one page.
fn list_tools_request(page: Option<PaginatedRequestParams>) -> ClientRequest {
    ClientRequest::ListToolsRequest(ListToolsRequest {
        method: ListToolsRequestMethod,
        params: page,
        extensions: Extensions::default(),
    })
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    use rift_protocol::lock::{ProductIdentity, SERVER_TOKEN_LENGTH, ServerLock};
    use rmcp::model::{ProtocolVersion, ServerCapabilities, ServerPeerInfo, ServerResult};
    use rmcp::service::{QuitReason, RoleClient, RunningService, serve_directly};
    use rmcp::transport::DynamicTransportError;
    use rmcp::{ErrorData, ServiceError};
    use serde_json::json;
    use tokio::io::AsyncBufReadExt as _;

    use super::{
        ConnectAttemptFailure, Replacement, RiftProxy, ServerStanding, Upstream, UpstreamSlot,
        adopt_serving, connect_recorded, connect_upstream, fallback_info, forwarded_error,
        identity_refusal, mirrored_info, quit_reason_result, reuse_current, serve_connection,
        server_start_failed, start_window_refusal, transport_failed,
    };
    use crate::election::{ServerPresence, StaleReason, claim};
    use rift_core::CapturedStream;
    use rift_error::errors;
    use rift_protocol::error as wire;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// One build of 0.0.11, and another build of the same version.
    const BUILD_A: &str = "0.0.11+b006b8433ba06679f06a3c7f0743d65634d32c34";
    const BUILD_B: &str = "0.0.11+71ea9ed284538bd4b5429df592afd7424e2bad13";
    const SCHEMA_DIGEST_A: &str =
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SCHEMA_DIGEST_B: &str =
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn identity(version: &str, schema_digest: &str) -> ProductIdentity {
        ProductIdentity {
            version: version.to_owned(),
            schema_digest: schema_digest.to_owned(),
        }
    }

    fn test_identity() -> ProductIdentity {
        identity(BUILD_A, SCHEMA_DIGEST_A)
    }

    fn transport_send_failure() -> ServiceError {
        ServiceError::TransportSend(DynamicTransportError::from_parts(
            "probe",
            TypeId::of::<()>(),
            Box::new(std::io::Error::other("connection refused")),
        ))
    }

    fn recorded_lock(port: u16) -> ServerLock {
        ServerLock {
            port,
            token: "a".repeat(SERVER_TOKEN_LENGTH),
            pid: 4_242,
            identity: identity(BUILD_A, SCHEMA_DIGEST_A),
            server: None,
        }
    }

    /// An upstream connection built without a handshake: its transport is a
    /// duplex pipe whose other half the caller keeps alive.
    fn direct_upstream() -> (RunningService<RoleClient, ()>, tokio::io::DuplexStream) {
        let (kept_alive, transport) = tokio::io::duplex(1024);
        let running: RunningService<RoleClient, ()> = serve_directly((), transport, None);
        (running, kept_alive)
    }

    #[tokio::test]
    async fn warmup_and_request_share_one_terminal_connection_attempt() {
        let mut slot = UpstreamSlot::empty();
        let (warmup, warmup_starts) = slot.begin_connection();
        let (request, request_starts) = slot.begin_connection();
        assert!(warmup_starts);
        assert!(!request_starts);
        assert!(std::sync::Arc::ptr_eq(&warmup, &request));

        let refusal = ErrorData::new(
            rmcp::model::ErrorCode(-32000),
            "server failed to bind",
            None,
        );
        warmup.complete(Err(refusal.clone()));
        let (warmup_result, request_result) = tokio::join!(warmup.outcome(), request.outcome());
        for result in [warmup_result, request_result] {
            let received = result.expect_err("terminal connection result must be shared");
            assert_eq!(received.code, refusal.code);
            assert_eq!(received.message, refusal.message);
        }
    }

    #[test]
    fn classified_refusals_forward_unchanged() {
        let refusal = ErrorData::new(
            rmcp::model::ErrorCode(-32000),
            "the workspace configuration failed validation",
            Some(json!({"code": "configuration_invalid", "retry": "operator_action"})),
        );
        let forwarded = forwarded_error(ServiceError::McpError(refusal.clone()));
        assert_eq!(forwarded.code, refusal.code);
        assert_eq!(forwarded.message, refusal.message);
        assert_eq!(
            forwarded.data, refusal.data,
            "the refusal's classification must survive the proxy untouched"
        );
    }

    #[test]
    fn transport_failures_map_to_internal_error_with_operator_hint() {
        for failure in [ServiceError::TransportClosed, transport_send_failure()] {
            let forwarded = forwarded_error(failure);
            assert_eq!(forwarded.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
            assert!(
                forwarded.message.contains("rift server start --foreground"),
                "{}",
                forwarded.message
            );
            assert!(
                forwarded.message.contains("reconnect"),
                "{}",
                forwarded.message
            );
        }
    }

    #[test]
    fn other_service_failures_name_the_upstream_failure() {
        let forwarded = forwarded_error(ServiceError::UnexpectedResponse);
        assert_eq!(forwarded.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            forwarded.message.contains("forwarded request"),
            "{}",
            forwarded.message
        );
        assert!(
            forwarded.message.contains("Unexpected response type"),
            "the upstream failure must be named: {}",
            forwarded.message
        );
    }

    #[test]
    fn transport_shapes_trigger_reconnect_and_others_do_not() {
        assert!(transport_failed(&ServiceError::TransportClosed));
        assert!(transport_failed(&transport_send_failure()));
        assert!(!transport_failed(&ServiceError::UnexpectedResponse));
        assert!(!transport_failed(&ServiceError::McpError(
            ErrorData::internal_error("probe", None)
        )));
    }

    #[test]
    fn reuse_decision_replaces_only_the_failed_generation() {
        assert!(
            reuse_current(0, None),
            "a first attempt reuses whatever is connected"
        );
        assert!(
            reuse_current(1, Some(0)),
            "a generation newer than the failed one is a finished reconnect"
        );
        assert!(
            !reuse_current(0, Some(0)),
            "the failed generation itself must be replaced"
        );
    }

    #[test]
    fn connect_attempt_failures_name_their_cause() {
        let refused = super::ConnectAttemptFailure::Initialize(Box::new(
            rmcp::service::ClientInitializeError::Cancelled,
        ));
        assert_eq!(refused.detail(), "Cancelled");
        let timed_out = super::ConnectAttemptFailure::TimedOut;
        assert!(timed_out.detail().contains("5s"), "{}", timed_out.detail());
    }

    #[test]
    fn an_equal_identity_is_adopted() {
        assert_eq!(
            ServerStanding::of(
                &identity(BUILD_A, SCHEMA_DIGEST_A),
                &identity(BUILD_A, SCHEMA_DIGEST_A)
            ),
            ServerStanding::Adopt
        );
    }

    #[test]
    fn an_older_version_is_replaced() {
        let ours = identity(BUILD_A, SCHEMA_DIGEST_A);
        for theirs in [
            identity("0.0.10", SCHEMA_DIGEST_A),
            identity(
                "0.0.10+b006b8433ba06679f06a3c7f0743d65634d32c34",
                SCHEMA_DIGEST_A,
            ),
            identity(
                "0.0.10+71ea9ed284538bd4b5429df592afd7424e2bad13.dirty.78008464.1790239195123456789",
                SCHEMA_DIGEST_B,
            ),
        ] {
            assert_eq!(
                ServerStanding::of(&ours, &theirs),
                ServerStanding::Replace,
                "{theirs:?}"
            );
        }
    }

    /// Two builds of one version stand level whatever their commits, dirty
    /// stamps, or tool schemas, so neither replaces the other.
    #[test]
    fn another_build_of_this_version_is_refused() {
        let ours = identity(BUILD_A, SCHEMA_DIGEST_A);
        for theirs in [
            identity(BUILD_B, SCHEMA_DIGEST_A),
            identity("0.0.11", SCHEMA_DIGEST_A),
            identity(
                "0.0.11+b006b8433ba06679f06a3c7f0743d65634d32c34.dirty.78008464.1790239195123456789",
                SCHEMA_DIGEST_A,
            ),
            identity(BUILD_A, SCHEMA_DIGEST_B),
        ] {
            assert_eq!(
                ServerStanding::of(&ours, &theirs),
                ServerStanding::Refuse,
                "{theirs:?}"
            );
        }
    }

    #[test]
    fn a_newer_version_and_an_unordered_one_are_refused() {
        let ours = identity(BUILD_A, SCHEMA_DIGEST_A);
        for theirs in [
            identity("0.0.12", SCHEMA_DIGEST_A),
            identity(
                "0.1.0+71ea9ed284538bd4b5429df592afd7424e2bad13",
                SCHEMA_DIGEST_A,
            ),
            identity("1.0.0", SCHEMA_DIGEST_A),
            identity("v0.0.12", SCHEMA_DIGEST_A),
        ] {
            assert_eq!(
                ServerStanding::of(&ours, &theirs),
                ServerStanding::Refuse,
                "{theirs:?}"
            );
        }
    }

    #[test]
    fn the_identity_refusal_names_both_identities_and_the_operator_step() {
        let expected = identity(BUILD_B, SCHEMA_DIGEST_B);
        let refusal = identity_refusal(&expected, &recorded_lock(12_345));
        assert_eq!(refusal.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        for named in [
            BUILD_A,
            BUILD_B,
            SCHEMA_DIGEST_A,
            SCHEMA_DIGEST_B,
            "pid 4242",
            "rift server stop",
        ] {
            assert!(refusal.message.contains(named), "{}", refusal.message);
        }
    }

    /// A recorded server answering an authorized `POST /api/stop` the way a
    /// server does, and counting the requests it accepted. Any other token is
    /// refused with `401`.
    async fn stop_counting_server(
        token: &str,
    ) -> TestResult<(u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let expected = format!("Bearer {token}");
        let counted = Arc::clone(&accepted);
        let router = axum::Router::new().route(
            crate::http::STOP_PATH,
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let counted = Arc::clone(&counted);
                let expected = expected.clone();
                async move {
                    let authorized = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
                    if !authorized {
                        return axum::http::StatusCode::UNAUTHORIZED;
                    }
                    counted.fetch_add(1, AtomicOrdering::SeqCst);
                    axum::http::StatusCode::ACCEPTED
                }
            }),
        );
        let served = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Ok((port, accepted, served))
    }

    /// A connect asks the server it replaces to stop once. Reading that server
    /// again while it leaves sends nothing, and a second server of another
    /// identity is refused rather than stopped in turn.
    #[tokio::test]
    async fn a_replacement_asks_one_server_once_and_refuses_the_next() -> TestResult {
        let (port, accepted, served) =
            stop_counting_server(&"a".repeat(SERVER_TOKEN_LENGTH)).await?;
        let newer = identity("0.0.12", SCHEMA_DIGEST_A);
        let mut lock = recorded_lock(port);
        let mut replacement = Replacement::default();
        replacement.replace(&lock, &newer).await?;
        assert!(replacement.is_asked());
        assert_eq!(accepted.load(AtomicOrdering::SeqCst), 1);

        replacement.replace(&lock, &newer).await?;
        assert_eq!(
            accepted.load(AtomicOrdering::SeqCst),
            1,
            "the server already asked is leaving"
        );

        lock.pid = 4_243;
        lock.token = "b".repeat(SERVER_TOKEN_LENGTH);
        let refusal = replacement
            .replace(&lock, &newer)
            .await
            .expect_err("a second server of another identity must be refused");
        assert!(
            refusal.message.contains("rift server stop"),
            "{}",
            refusal.message
        );
        assert_eq!(accepted.load(AtomicOrdering::SeqCst), 1);
        served.abort();
        Ok(())
    }

    #[tokio::test]
    async fn a_server_that_refuses_the_stop_request_is_refused() -> TestResult {
        let (port, accepted, served) =
            stop_counting_server(&"b".repeat(SERVER_TOKEN_LENGTH)).await?;
        let mut replacement = Replacement::default();
        let refusal = replacement
            .replace(&recorded_lock(port), &identity("0.0.12", SCHEMA_DIGEST_A))
            .await
            .expect_err("a server that refused the stop keeps serving");
        assert!(
            refusal.message.contains("rift server stop"),
            "{}",
            refusal.message
        );
        assert!(!replacement.is_asked());
        assert_eq!(accepted.load(AtomicOrdering::SeqCst), 0);
        served.abort();
        Ok(())
    }

    /// A recorded server that resets the connection of every stop request, the way a
    /// server already stopping closes it as it exits, and keeps its port open. It counts
    /// the stop requests it reset; a presence probe connects and sends nothing, so it is
    /// not one.
    async fn stop_resetting_server()
    -> TestResult<(u16, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let reset = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reset);
        let served = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut head = [0_u8; 64];
                let read = tokio::io::AsyncReadExt::read(&mut stream, &mut head)
                    .await
                    .unwrap_or(0);
                if head[..read].starts_with(b"POST /api/stop") {
                    counted.fetch_add(1, AtomicOrdering::SeqCst);
                    let _ = stream.set_zero_linger();
                }
            }
        });
        Ok((port, reset, served))
    }

    /// A stop request the server resets does not decide the outcome: a server already
    /// stopping closes the connection as it exits, which Windows reports as
    /// `ConnectionReset`. The connect counts the server as asked, so its next reading
    /// of the election decides.
    #[tokio::test]
    async fn a_stop_the_server_resets_leaves_the_outcome_to_the_next_reading() -> TestResult {
        let (port, reset, served) = stop_resetting_server().await?;
        let mut replacement = Replacement::default();
        replacement
            .replace(&recorded_lock(port), &identity("0.0.12", SCHEMA_DIGEST_A))
            .await?;
        assert!(replacement.is_asked());
        assert_eq!(reset.load(AtomicOrdering::SeqCst), 1);
        served.abort();
        Ok(())
    }

    /// The same older server still serving once the start window closed, after it reset
    /// the stop request, is refused with the identity refusal: it never accepted the
    /// request, and the operator's step is to stop it.
    #[tokio::test]
    async fn an_older_server_still_serving_after_a_reset_stop_is_refused_at_the_window()
    -> TestResult {
        let (port, reset, served) = stop_resetting_server().await?;
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        guard.publish(&recorded_lock(port))?;

        let started = tokio::time::Instant::now();
        let root = directory.path().to_path_buf();
        let connect = tokio::spawn(async move {
            connect_upstream(&root, &identity("0.0.12", SCHEMA_DIGEST_A))
                .await
                .err()
        });
        // The stop request runs on the real clock, since a paused clock would end its
        // bound before the loopback answer arrives. The start window then runs paused.
        tokio::time::timeout(Duration::from_secs(10), async {
            while reset.load(AtomicOrdering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .map_err(|_elapsed| "the connect never asked the older server to stop")?;
        tokio::time::pause();
        let refusal = connect
            .await?
            .ok_or("an older server that still serves must be refused")?;
        assert!(
            started.elapsed() >= crate::spawn::START_WAIT_MAX,
            "the refusal waits out the start window: {:?}",
            started.elapsed()
        );
        assert!(
            refusal
                .message
                .contains("workspace server identity differs from this rift process"),
            "{}",
            refusal.message
        );
        assert!(refusal.message.contains(BUILD_A), "{}", refusal.message);
        assert_eq!(
            reset.load(AtomicOrdering::SeqCst),
            1,
            "the server is asked to stop once"
        );
        served.abort();
        drop(guard);
        Ok(())
    }

    /// Adoption asks an older recorded server to stop and answers nothing to
    /// adopt, and refuses a newer one without asking it anything.
    #[tokio::test]
    async fn adopt_replaces_an_older_server_and_refuses_a_newer_one() -> TestResult {
        let (port, accepted, served) =
            stop_counting_server(&"a".repeat(SERVER_TOKEN_LENGTH)).await?;
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        guard.publish(&recorded_lock(port))?;

        let newer_proxy = identity("0.0.12", SCHEMA_DIGEST_A);
        let mut replacement = Replacement::default();
        let adopted = adopt_serving(directory.path(), &newer_proxy, &mut replacement).await?;
        assert!(adopted.is_none(), "a replaced server is never adopted");
        assert!(replacement.is_asked());
        assert_eq!(accepted.load(AtomicOrdering::SeqCst), 1);

        let older_proxy = identity("0.0.10", SCHEMA_DIGEST_A);
        let refusal = adopt_serving(directory.path(), &older_proxy, &mut Replacement::default())
            .await
            .expect_err("a newer server must be refused");
        assert!(refusal.message.contains(BUILD_A), "{}", refusal.message);
        assert_eq!(
            accepted.load(AtomicOrdering::SeqCst),
            1,
            "a newer server is never asked to stop"
        );
        served.abort();
        drop(guard);
        Ok(())
    }

    #[test]
    fn unavailable_refusal_names_the_window_without_a_shell_command_the_caller_cannot_run() {
        let refusal = start_window_refusal(false);
        assert_eq!(refusal.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(refusal.message.contains("30s"), "{}", refusal.message);
        assert!(
            refusal.message.contains("operator action"),
            "{}",
            refusal.message
        );
        assert!(
            !refusal.message.contains('`'),
            "the caller has no shell to run a command in: {}",
            refusal.message
        );
    }

    #[test]
    fn server_start_failed_names_empty_standard_error() {
        let refusal = server_start_failed(&CapturedStream::default());
        assert!(
            refusal.message.contains("wrote no standard error"),
            "{}",
            refusal.message
        );
        let data: wire::ErrorData = serde_json::from_value(
            refusal
                .data
                .clone()
                .expect("wire failure data must be present"),
        )
        .expect("wire failure data must match schema");
        assert_eq!(data.code, wire::ErrorCode::TemporarilyUnavailable);
        assert_eq!(data.retry, wire::RetryDirective::SameRequest);
    }

    #[test]
    fn server_start_failed_redacts_captured_stderr() {
        let capture = CapturedStream {
            text: "panicked at src/main.rs:1".to_owned(),
            captured_bytes: 26,
            total_bytes: 26,
            truncated: false,
        };
        let registered = errors::mcp::spawn_failed()
            .stderr(capture.text.clone())
            .stderr_truncated(capture.truncated)
            .error();
        assert!(
            registered
                .context()
                .any(|(key, value)| { key == "stderr" && value == "[redacted]" })
        );
        assert!(
            registered.to_string().contains("[redacted]")
                && !registered.to_string().contains(&capture.text)
        );
        let refusal = server_start_failed(&capture);
        assert!(
            refusal.message.contains("[redacted]"),
            "{}",
            refusal.message
        );
        assert!(!refusal.message.contains(&capture.text));
        let data: wire::ErrorData = serde_json::from_value(
            refusal
                .data
                .clone()
                .expect("wire failure data must be present"),
        )
        .expect("wire failure data must match schema");
        assert_eq!(data.code, wire::ErrorCode::TemporarilyUnavailable);
        assert_eq!(data.retry, wire::RetryDirective::SameRequest);
        assert!(
            !refusal.message.contains('`'),
            "the caller has no shell to run a command in: {}",
            refusal.message
        );
    }

    #[test]
    fn server_start_failed_names_truncation_when_the_capture_was_cut_short() {
        let capture = CapturedStream {
            text: "panicked at src/main.rs:1".to_owned(),
            captured_bytes: 26,
            total_bytes: 9_000,
            truncated: true,
        };
        let refusal = server_start_failed(&capture);
        assert!(
            refusal.message.contains("stderr_truncated true"),
            "a truncated capture must say so: {}",
            refusal.message
        );
    }

    #[test]
    fn fallback_advertisement_names_rift_and_enables_tools() {
        let info = fallback_info(&test_identity());
        assert_eq!(info.server_info.name, "rift");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn mirrored_advertisement_carries_the_upstream_facts() {
        let peer_info = ServerPeerInfo::new(
            ProtocolVersion::default(),
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions("upstream instructions");
        let info = mirrored_info(&peer_info);
        assert_eq!(info.instructions.as_deref(), Some("upstream instructions"));
        assert!(info.capabilities.tools.is_some());
        assert_eq!(
            info.server_info.name, "rift",
            "an upstream without an identity falls back to this binary's own"
        );
    }

    #[test]
    fn advertised_info_serves_the_fallback_then_the_recorded_mirror() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity());
        assert_eq!(proxy.advertised_info().server_info.name, "rift");
        let mirrored = fallback_info(&test_identity()).with_instructions("recorded");
        *proxy.lock_advertised() = Some(mirrored);
        assert_eq!(
            proxy.advertised_info().instructions.as_deref(),
            Some("recorded")
        );
    }

    #[test]
    fn proxy_errors_carry_registered_slugs_and_sources() {
        let quit = errors::mcp::proxy_unexpected_quit().error();
        assert_eq!(quit.slug(), errors::mcp::proxy_unexpected_quit::SLUG);
        let rendered = quit.to_string();
        assert_eq!(
            rendered,
            "MCP service ended unexpectedly; report this internal failure with its full context"
        );
        assert!(std::error::Error::source(&quit).is_none());

        let initialize = errors::mcp::proxy_initialization_failed()
            .source(rmcp::service::ServerInitializeError::Cancelled)
            .error();
        assert_eq!(
            initialize.slug(),
            errors::mcp::proxy_initialization_failed::SLUG
        );
        let rendered_initialize = initialize.to_string();
        assert!(
            rendered_initialize.contains("MCP proxy initialization failed"),
            "{rendered_initialize}"
        );
        assert!(std::error::Error::source(&initialize).is_some());
    }

    #[test]
    fn an_answer_of_another_kind_refuses_as_unexpected() {
        let refusal = super::answered_as(ServerResult::empty(()), |result| match result {
            ServerResult::ListToolsResult(result) => Some(result),
            _ => None,
        })
        .expect_err("an answer of another kind must refuse");
        assert!(
            refusal.message.contains("forwarded request"),
            "{}",
            refusal.message
        );
    }

    /// Bound on a stalled forward in the unanswered-server case, far past the budget the
    /// workspace below gives it: a forward still waiting here has no bound of its own.
    const STALLED_FORWARD_MAX: Duration = Duration::from_secs(600);

    /// A server that accepted the connection and then stopped answering: the forward
    /// refuses once the budget the workspace's `[server]` table sets passes, and the
    /// refusal is one the caller can retry.
    #[tokio::test(start_paused = true)]
    async fn a_forward_the_server_never_answers_refuses_at_its_budget() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("rift.toml"),
            "[server]\nreadiness_timeout = \"1s\"\nworker_queue_timeout = \"1s\"\n",
        )?;
        let proxy = RiftProxy::new(directory.path(), test_identity());
        let (running, upstream_silent) = direct_upstream();
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }

        let started = tokio::time::Instant::now();
        let forward = proxy.forward(super::list_tools_request(None), |result| match result {
            ServerResult::ListToolsResult(result) => Some(result),
            _ => None,
        });
        let answered = tokio::time::timeout(STALLED_FORWARD_MAX, forward)
            .await
            .map_err(|_elapsed| "the forward kept waiting for a silent server past its budget")?;
        let waited = started.elapsed();
        let refusal = answered.expect_err("a silent server must refuse the forward");

        assert_eq!(
            waited,
            Duration::from_secs(2) + super::FORWARD_ANSWER_GRACE,
            "the forward waits out the two request bounds and the grace, then refuses"
        );
        let data = refusal
            .data
            .ok_or("the refusal carries its classification")?;
        assert_eq!(data["code"], json!("temporarily_unavailable"), "{data}");
        assert!(
            refusal.message.contains("did not answer"),
            "{}",
            refusal.message
        );

        // The silent server received the request, then its cancellation, both naming
        // one request id: the budget's end cancels the work instead of abandoning it.
        let mut received = tokio::io::BufReader::new(upstream_silent).lines();
        let request: serde_json::Value = serde_json::from_str(
            &received
                .next_line()
                .await?
                .ok_or("the request reached the server")?,
        )?;
        let cancellation: serde_json::Value = serde_json::from_str(
            &received
                .next_line()
                .await?
                .ok_or("the cancellation reached the server")?,
        )?;
        assert_eq!(request["method"], json!("tools/list"), "{request}");
        assert_eq!(
            cancellation["method"],
            json!("notifications/cancelled"),
            "{cancellation}"
        );
        assert_eq!(
            cancellation["params"]["requestId"], request["id"],
            "{cancellation}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn mirror_skips_an_upstream_without_negotiated_info() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity());
        let (running, _upstream_alive) = direct_upstream();
        proxy.mirror_advertised(&running);
        assert!(
            proxy.lock_advertised().is_none(),
            "an upstream without negotiated info must not be mirrored"
        );
    }

    #[test]
    fn poisoned_advertised_lock_still_serves_info() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity());
        let poisoner = proxy.clone();
        std::thread::spawn(move || {
            let _guard = poisoner
                .advertised
                .lock()
                .expect("the first lock of a fresh proxy must succeed");
            panic!("poison the advertised lock");
        })
        .join()
        .expect_err("the poisoning thread must end by panic");
        assert!(
            proxy.advertised.lock().is_err(),
            "the lock must be poisoned for the recovery arm to matter"
        );
        assert_eq!(proxy.advertised_info().server_info.name, "rift");
        *proxy.lock_advertised() =
            Some(fallback_info(&test_identity()).with_instructions("recovered"));
        assert_eq!(
            proxy.advertised_info().instructions.as_deref(),
            Some("recovered")
        );
    }

    /// The probe runs on the blocking pool: on a single-threaded runtime,
    /// another task completes while a probe still blocks, and that task is
    /// what releases the probe.
    #[tokio::test(flavor = "current_thread")]
    async fn a_probe_that_blocks_leaves_the_runtime_free() -> TestResult {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let probing = super::probed_with(std::path::Path::new("."), move |_root| {
            match released.recv_timeout(Duration::from_secs(5)) {
                Ok(()) => ServerPresence::Absent,
                Err(_) => ServerPresence::Stale(StaleReason::DocumentUnreadable),
            }
        });
        let releasing = async { release.send(()) };
        let (presence, sent) = tokio::join!(probing, releasing);
        sent?;
        assert!(
            matches!(presence, ServerPresence::Absent),
            "the runtime released the probe while it blocked: {presence:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn adopt_treats_an_unanswering_recorded_server_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let port = {
            let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            listener.local_addr()?.port()
        };
        guard.publish(&recorded_lock(port))?;
        assert!(
            adopt_serving(
                directory.path(),
                &test_identity(),
                &mut Replacement::default()
            )
            .await?
            .is_none(),
            "a recorded server that answers nothing must be treated as stale"
        );
        Ok(())
    }

    /// A server another starter elected is still building: the connect spawns nothing
    /// and waits for that holder's document, refusing once the start window closes.
    #[tokio::test(start_paused = true)]
    async fn connect_spawns_nothing_while_another_starter_holds_the_election() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let refusal = connect_upstream(directory.path(), &test_identity())
            .await
            .expect_err("a holder that never publishes must exhaust the start window");
        assert_eq!(refusal.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            refusal
                .message
                .contains("no rift server answered for this workspace within 30s"),
            "{}",
            refusal.message
        );
        Ok(())
    }

    /// A holder of the election that publishes nothing but writes to the
    /// workspace database during the start window is still building: the
    /// connect refuses with a refusal the caller resends, naming the build.
    #[tokio::test(start_paused = true)]
    async fn a_building_holder_refuses_with_a_retryable_refusal() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let log = directory.path().join(".rift").join("db-wal");
        let writing = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::fs::write(&log, b"a commit the building server wrote")
        };
        let identity = test_identity();
        let (refusal, written) =
            tokio::join!(connect_upstream(directory.path(), &identity), writing);
        written?;
        let refusal = refusal.expect_err("a holder that never publishes exhausts the window");
        let data = refusal
            .data
            .ok_or("the refusal carries its classification")?;
        assert_eq!(data["code"], json!("temporarily_unavailable"), "{data}");
        assert!(
            refusal
                .message
                .contains("did not finish building its first index within 30s"),
            "{}",
            refusal.message
        );
        assert!(
            refusal
                .message
                .contains("resend the same request after a short delay"),
            "{}",
            refusal.message
        );
        Ok(())
    }

    /// Only a holder still building, together with a database that moved,
    /// selects the retryable refusal.
    #[test]
    fn the_start_window_refusal_follows_the_evidence() {
        let building = super::start_window_refusal(true);
        assert!(
            building
                .message
                .contains("did not finish building its first index within 30s"),
            "{}",
            building.message
        );
        let data: wire::ErrorData = serde_json::from_value(
            building
                .data
                .clone()
                .expect("wire failure data must be present"),
        )
        .expect("wire failure data must match schema");
        assert_eq!(data.code, wire::ErrorCode::TemporarilyUnavailable);
        assert_eq!(data.retry, wire::RetryDirective::SameRequest);
        assert!(
            building
                .message
                .contains("resend the same request after a short delay"),
            "{}",
            building.message
        );
        let wedged = super::start_window_refusal(false);
        assert_eq!(wedged.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            wedged.message.contains("operator action on this host"),
            "{}",
            wedged.message
        );
    }

    /// Database activity reads each file's length and modification time, and
    /// moves when either file is written.
    #[test]
    fn database_activity_moves_when_the_database_is_written() -> TestResult {
        let directory = tempfile::tempdir()?;
        let before = super::DatabaseActivity::read(directory.path());
        assert_eq!(before.files, [None, None]);
        std::fs::create_dir_all(directory.path().join(".rift"))?;
        std::fs::write(directory.path().join(".rift").join("db"), b"pages")?;
        let written = super::DatabaseActivity::read(directory.path());
        assert_ne!(written, before);
        std::fs::write(directory.path().join(".rift").join("db-wal"), b"a commit")?;
        assert_ne!(super::DatabaseActivity::read(directory.path()), written);
        Ok(())
    }

    /// A server still building its first index has published no lock
    /// document, so no identity is read and nothing asks it to stop.
    #[tokio::test]
    async fn a_building_server_is_never_asked_to_stop() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut replacement = Replacement::default();
        let adopted = super::adopt_presence(
            directory.path(),
            ServerPresence::Starting,
            &identity("999.0.0", SCHEMA_DIGEST_A),
            &mut replacement,
        )
        .await?;
        assert!(adopted.is_none());
        assert!(!replacement.is_asked());
        Ok(())
    }

    /// A spawn that cannot launch is reported, and the poll still gives a concurrently
    /// started server its window before refusing.
    #[tokio::test(start_paused = true)]
    async fn connect_reports_a_spawn_that_cannot_launch_and_still_polls() -> TestResult {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing");
        let refusal = connect_upstream(&missing, &test_identity())
            .await
            .expect_err("a workspace nobody serves must exhaust the start window");
        assert_eq!(refusal.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
        assert!(
            refusal
                .message
                .contains("no rift server answered for this workspace within 30s"),
            "{}",
            refusal.message
        );
        Ok(())
    }

    #[tokio::test]
    async fn connect_attempt_times_out_against_a_silent_server() -> TestResult {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let lock = recorded_lock(listener.local_addr()?.port());
        let failure = connect_recorded(&lock, Duration::from_millis(50))
            .await
            .expect_err("a server that accepts and answers nothing must time out");
        assert!(
            matches!(failure, ConnectAttemptFailure::TimedOut),
            "{failure:?}"
        );
        drop(listener);
        Ok(())
    }

    #[tokio::test]
    async fn task_error_preserves_the_join_source() {
        let task = tokio::spawn(async { panic!("test join failure") });
        let join = task.await.expect_err("test task must fail");
        let error = quit_reason_result(QuitReason::JoinError(join))
            .expect_err("join error quit reason must fail");
        assert_eq!(error.slug(), errors::mcp::proxy_task_failed::SLUG);
        assert!(error.to_string().contains("MCP proxy service task failed"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn closed_transport_fails_initialization() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (server_transport, client_transport) = tokio::io::duplex(1024);
        drop(client_transport);
        let error = Box::pin(serve_connection(
            RiftProxy::new(directory.path(), test_identity()),
            server_transport,
        ))
        .await
        .expect_err("a closed transport must fail initialization");
        assert_eq!(error.slug(), errors::mcp::proxy_initialization_failed::SLUG);
    }

    #[tokio::test]
    async fn tool_diagnostics_correlate_proxy_and_server_requests() -> TestResult {
        use rmcp::ServiceExt as _;

        // Issue #483 needs the request still awaiting an answer to retain its IDs.
        let log = tempfile::NamedTempFile::new()?;
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter("rift=info,rift_mcp=debug,rift_server=debug,rift_index=info")
            .with_ansi(false)
            .with_writer(log.reopen()?)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        let server = crate::server::RiftMcp::build_settled(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
        )
        .await?;
        let (server_transport, upstream_transport) = tokio::io::duplex(64 * 1024);
        let (serving, upstream) =
            tokio::join!(server.serve(server_transport), ().serve(upstream_transport));
        let serving = serving?;
        let upstream = upstream?;
        // Advance the upstream's IDs so matching the downstream ID cannot pass by chance.
        upstream.list_tools(None).await?;
        let proxy = RiftProxy::new(directory.path(), test_identity());
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running: upstream,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let (proxy_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let (forwarding, client) =
            tokio::join!(proxy.serve(proxy_transport), ().serve(client_transport));
        let forwarding = forwarding?;
        let client = client?;
        let params = super::CallToolRequestParams::new("search").with_arguments(
            serde_json::from_value(json!({"query": "zzdiagnosticsecret", "scope": "local"}))?,
        );
        let request = super::ClientRequest::CallToolRequest(super::CallToolRequest::new(params));
        let handle = client
            .send_cancellable_request(request, super::PeerRequestOptions::default())
            .await?;
        let request_id = handle.id.to_string();
        let result = handle.await_response().await?;
        let ServerResult::CallToolResult(result) = result else {
            return Err("search must return a tool result".into());
        };
        assert_ne!(result.is_error, Some(true));
        let invalid = client
            .call_tool(super::CallToolRequestParams::new("nodes").with_arguments(
                serde_json::from_value(json!({"path": "../outside.rs", "position": 0}))?,
            ))
            .await
            .expect_err("invalid paths retain the wire refusal");
        let ServiceError::McpError(invalid) = invalid else {
            return Err("invalid paths must return an MCP error".into());
        };
        assert_eq!(
            invalid.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!("invalid_request"))
        );
        let records = std::fs::read_to_string(log.path())?;
        assert_tool_diagnostics(&records, &request_id)?;
        client.cancel().await?;
        forwarding.cancel().await?;
        serving.cancel().await?;
        Ok(())
    }

    fn assert_tool_diagnostics(records: &str, request_id: &str) -> TestResult {
        let forwarded = records
            .lines()
            .find(|line| {
                line.contains("forwarded request awaiting response")
                    && line.contains(&format!("request_id={request_id}"))
                    && line.contains("tool=search")
            })
            .ok_or("the proxy records the active forward and downstream request ID")?;
        let upstream_id = forwarded
            .split("upstream_request_id=")
            .nth(1)
            .and_then(|value| value.split_whitespace().next())
            .ok_or("the forward records its upstream request ID")?;
        assert_ne!(upstream_id, request_id);
        for event in [
            "tool request started",
            "worker admission started",
            "worker admitted",
            "request capture compared with publication",
            "tool request completed",
        ] {
            assert!(
                records.lines().any(|line| {
                    line.contains("mcp.request{")
                        && line.contains(&format!("request_id={upstream_id}"))
                        && line.contains(event)
                }),
                "the server event retains the upstream request ID: {event}\n{records}"
            );
        }
        assert!(
            records
                .lines()
                .any(|line| line.contains("tool request completed")
                    && line.contains("is_error=false"))
        );
        assert!(
            records
                .lines()
                .any(|line| line.contains("tool request completed")
                    && line.contains("is_error=true"))
        );
        assert!(
            !records.contains("zzdiagnosticsecret"),
            "request arguments stay out of diagnostics: {records}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_client_ends_the_connection_cleanly() {
        use rmcp::ServiceExt as _;
        let directory = tempfile::tempdir().expect("temporary directory");
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(serve_connection(
            RiftProxy::new(directory.path(), test_identity()),
            server_transport,
        ));
        let client = ().serve(client_transport).await.expect("client must initialize");
        let advertised = client.peer_info().expect("server info must be advertised");
        assert_eq!(
            advertised
                .server_info
                .as_ref()
                .expect("implementation identity must be advertised")
                .name,
            "rift"
        );
        client.cancel().await.expect("client must cancel");
        connection
            .await
            .expect("serve task must join")
            .expect("a cancelled client must end the connection cleanly");
    }
}
