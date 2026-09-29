//! Stateless stdio proxy to the workspace's elected rift server.
//!
//! `rift mcp` serves agents over stdio while the workspace's state lives in
//! one `rift server` process. Every request forwards to that server: the
//! proxy adopts a recorded server when the lock document names a live one,
//! starts a detached server when the workspace has none, and reconnects -
//! single-flight, one retry per request - when the server it held goes
//! away, so an agent session survives server restarts.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rift_core::{CapturedStream, CliCode, Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::error as wire;
use rift_protocol::lock::{ProductIdentity, ServerLock};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, Implementation, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ServerCapabilities, ServerConfig, ServerPeerInfo,
};
use rmcp::service::{
    ClientInitializeError, Peer, QuitReason, RequestContext, RoleClient, RoleServer,
    RunningService, ServerInitializeError,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ErrorData, ServerHandler, ServiceError, ServiceExt as _};

use crate::election::{ServerPresence, StaleReason, probe};
use crate::failure::WireFailure as _;
use crate::http::MCP_PATH;
use crate::spawn::{
    PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, StartupCapture,
    spawn_detached_server_with_captured_stderr,
};

/// Bound on one upstream connect-and-initialize attempt.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Failure while starting or running the stdio MCP proxy.
pub type ProxyServeError = Error<ProxyFault>;

/// One proxy failure: what stopped `rift mcp` from serving agents.
#[derive(Debug)]
pub enum ProxyFault {
    /// Current process identity could not be computed.
    Identity(std::io::Error),
    /// MCP initialization over stdio failed.
    Initialize(Box<ServerInitializeError>),
    /// The MCP service task failed.
    Task(tokio::task::JoinError),
    /// The MCP service ended unexpectedly.
    UnexpectedQuit,
}

impl Fault for ProxyFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Initialize(_) => ErrorName::Wire(ErrorCode::TemporarilyUnavailable),
            Self::Identity(_) | Self::Task(_) | Self::UnexpectedQuit => {
                ErrorName::Wire(ErrorCode::InternalError)
            }
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let detail = match self {
            Self::Identity(_) => "product identity failed",
            Self::Initialize(_) => "MCP initialization failed",
            Self::Task(_) => "MCP service task failed",
            Self::UnexpectedQuit => "MCP service ended unexpectedly",
        };
        vec![ErrorContext::new("detail", detail)]
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Identity(source) => Some(source),
            Self::Initialize(source) => Some(source.as_ref()),
            Self::Task(source) => Some(source),
            Self::UnexpectedQuit => None,
        }
    }
}

/// Serves agents over stdio MCP, forwarding every request to the
/// workspace's elected server.
///
/// Serving starts immediately: a background warmup attempts the first
/// upstream connect - starting a server when the workspace has none - and
/// each request that arrives earlier waits on the same single-flight
/// connect. Stdout carries protocol frames only; diagnostics go to
/// tracing/stderr, and the upstream bearer token appears in neither.
///
/// # Errors
///
/// Returns [`ProxyServeError`] for initialization or service-task failure.
///
/// # Cancel safety
///
/// Dropping this future closes the owned MCP service and the upstream
/// connection; the detached server keeps serving the workspace.
pub async fn serve_proxy(root: &Path) -> Result<(), ProxyServeError> {
    tracing::info!(component = "mcp", transport = "stdio", "MCP proxy starting");
    let identity = crate::identity::product_identity()
        .await
        .map_err(|error| Error::new(ProxyFault::Identity(error)))?;
    let proxy = RiftProxy::new(root, identity);
    let warmup = tokio::spawn(warm_up(proxy.clone()));
    let outcome = serve_connection(proxy, crate::transport::guarded_stdio()).await;
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
) -> Result<(), ProxyServeError>
where
    Transport: rmcp::transport::IntoTransport<RoleServer, TransportError, Adapter>,
    TransportError: std::error::Error + Send + Sync + 'static,
{
    let service = proxy
        .serve(transport)
        .await
        .map_err(|error| Error::new(ProxyFault::Initialize(Box::new(error))))?;
    tracing::info!(component = "mcp", transport = "stdio", "MCP proxy ready");
    let reason = service.waiting().await;
    let outcome = reason
        .map_err(|error| Error::new(ProxyFault::Task(error)))
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
fn quit_reason_result(reason: QuitReason) -> Result<(), ProxyServeError> {
    match reason {
        QuitReason::Closed | QuitReason::Cancelled => Ok(()),
        QuitReason::JoinError(error) => Err(Error::new(ProxyFault::Task(error))),
        _ => Err(Error::new(ProxyFault::UnexpectedQuit)),
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
    fn new(root: &Path, identity: ProductIdentity) -> Self {
        Self {
            root: Arc::from(root),
            identity: Arc::new(identity),
            upstream: Arc::new(tokio::sync::Mutex::new(UpstreamSlot::empty())),
            advertised: Arc::new(std::sync::Mutex::new(None)),
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

    /// Forwards one request to the upstream with a single reconnect retry.
    ///
    /// A transport-shaped failure marks the held connection dead and
    /// retries exactly once on a freshly leased connection; every other
    /// failure maps straight through [`forwarded_error`].
    ///
    /// # Cancel safety
    ///
    /// Dropping this future abandons the forward; a reconnect another
    /// request started is unaffected.
    async fn forward<Request, Value, Forward, Fut>(
        &self,
        request: Request,
        send: Forward,
    ) -> Result<Value, ErrorData>
    where
        Request: Clone,
        Forward: Fn(Peer<RoleClient>, Request) -> Fut,
        Fut: Future<Output = Result<Value, ServiceError>>,
    {
        let (peer, generation) = self.leased_peer(None).await?;
        let failure = match send(peer, request.clone()).await {
            Ok(value) => return Ok(value),
            Err(error) if transport_failed(&error) => error,
            Err(error) => return Err(forwarded_error(error)),
        };
        tracing::info!(
            component = "mcp",
            failure = %failure,
            "upstream connection lost; reconnecting"
        );
        let (peer, _generation) = self.leased_peer(Some(generation)).await?;
        send(peer, request).await.map_err(forwarded_error)
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
/// the window keep polling; a spawned server that exits before the window
/// closes refuses with its captured stderr, unless it lost the start
/// election. A lost election keeps polling for the winner, who may still be
/// binding, and spawns again once a probe finds no process holding the
/// election, at most [`START_SPAWN_COUNT_MAX`] spawns in all; exhaustion
/// refuses with the operator's next step.
///
/// # Cancel safety
///
/// Dropping this future abandons the connect; a spawned server keeps
/// serving and the next attempt adopts it.
async fn connect_upstream(
    root: &Path,
    identity: &ProductIdentity,
) -> Result<RunningService<RoleClient, ()>, ErrorData> {
    if let Some(running) = adopt_serving(root, identity).await? {
        return Ok(running);
    }
    // A server another starter elected is still building: a spawn now would
    // only lose the election, so the poll below waits for that server's
    // document instead. Otherwise a lost spawn race is fine - this
    // process's own spawned server finds the workspace already served, exits
    // on its own, and the poll adopts the winner once it finishes binding. A
    // spawn that cannot launch at all is reported, and the poll still gives
    // a concurrently started server its chance.
    let mut spawns = StartSpawns::default();
    if !matches!(probe(root), ServerPresence::Starting) {
        spawns.spawn(root);
    }
    let deadline = tokio::time::Instant::now() + START_WAIT_MAX;
    for _ in 0..START_POLL_ATTEMPT_COUNT {
        let presence = probe(root);
        let election_held = presence.election_held();
        let adopted = adopt_presence(presence, identity).await?;
        match spawns.poll(adopted, election_held) {
            SpawnPollOutcome::Ready(running) => return Ok(running),
            SpawnPollOutcome::Failed(refusal) => return Err(refusal),
            SpawnPollOutcome::ElectionUnheld => {
                tracing::info!(
                    component = "mcp",
                    "no process holds the election the spawned server lost; spawning again"
                );
                spawns.spawn(root);
            }
            SpawnPollOutcome::Waiting => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
    }
    Err(upstream_unavailable())
}

/// Most detached servers one connect spawns.
///
/// A spawned server loses the start election whenever its claim meets any
/// lock on the election file, a probe's shared lock included: Windows
/// releases a closed handle's locks lazily, so a probe that already let go
/// can still hold one. A loss that leaves the election unheld is that case,
/// and the connect spawns again. The count bounds the spawns when something
/// keeps such a lock for longer; the rest of the start window then passes
/// as a wait.
const START_SPAWN_COUNT_MAX: u32 = 4;

/// The detached servers one connect spawned, and the latest one as the
/// poll last saw it.
#[derive(Debug, Default)]
struct StartSpawns {
    latest: SpawnWatch,
    spawn_count: u32,
}

/// The latest server one connect spawned, as its poll last saw it.
#[derive(Debug, Default)]
enum SpawnWatch {
    /// No spawned server is outstanding: none was spawned, the spawn could
    /// not launch, or the spawn count is spent.
    #[default]
    Idle,
    /// The spawned server has not closed its standard error yet.
    Running(StartupCapture),
    /// The spawned server exited after losing the start election.
    LostElection,
}

impl StartSpawns {
    /// Spawns one more detached server, unless [`START_SPAWN_COUNT_MAX`]
    /// spawns already ran.
    fn spawn(&mut self, root: &Path) {
        if self.spawn_count >= START_SPAWN_COUNT_MAX {
            let spawn_count = self.spawn_count;
            tracing::warn!(
                component = "mcp",
                spawn_count,
                "the spawn count is spent; the start window passes as a wait"
            );
            self.latest = SpawnWatch::Idle;
            return;
        }
        self.spawn_count += 1;
        self.latest = match spawn_detached_server_with_captured_stderr(root) {
            Ok(startup) => SpawnWatch::Running(startup),
            Err(error) => {
                tracing::warn!(component = "mcp", %error, "detached server spawn failed");
                SpawnWatch::Idle
            }
        };
    }

    /// Classifies one poll iteration: an adopted connection wins outright;
    /// otherwise the latest spawn's exit, or the election it lost, decides.
    ///
    /// `election_held` is what this iteration's probe found. It decides only
    /// for a loss an earlier iteration observed, so the probe that sends the
    /// connect to spawn again was read after the losing server exited.
    ///
    /// Split from [`connect_upstream`] so the ordering - success checked
    /// before the spawned server's exit - is testable without a real process
    /// or a real upstream connection.
    fn poll<Adopted>(
        &mut self,
        adopted: Option<Adopted>,
        election_held: bool,
    ) -> SpawnPollOutcome<Adopted> {
        if let Some(running) = adopted {
            return SpawnPollOutcome::Ready(running);
        }
        let exited = match &mut self.latest {
            SpawnWatch::Running(startup) => startup.exited(),
            SpawnWatch::LostElection if !election_held => return SpawnPollOutcome::ElectionUnheld,
            SpawnWatch::LostElection | SpawnWatch::Idle => return SpawnPollOutcome::Waiting,
        };
        self.latest_exit(exited)
    }

    /// Classifies the latest spawn's exit: a lost election is recorded for
    /// the next iteration to weigh, and any other exit fails the connect with
    /// the server's captured stderr.
    fn latest_exit<Adopted>(
        &mut self,
        exited: Option<CapturedStream>,
    ) -> SpawnPollOutcome<Adopted> {
        let Some(capture) = exited else {
            return SpawnPollOutcome::Waiting;
        };
        if !lost_start_election(&capture) {
            return SpawnPollOutcome::Failed(server_start_failed(&capture));
        }
        tracing::info!(
            component = "mcp",
            stderr = %capture.text,
            "the spawned server lost the start election"
        );
        self.latest = SpawnWatch::LostElection;
        SpawnPollOutcome::Waiting
    }
}

/// One poll iteration's outcome against a possibly still-spawning server.
enum SpawnPollOutcome<Adopted> {
    /// The workspace's server answered; the latest spawn's capture is never
    /// consulted, so the daemon's stderr keeps draining on its own.
    Ready(Adopted),
    /// The spawned server exited before it started serving, for a reason
    /// adoption cannot resolve.
    Failed(ErrorData),
    /// The spawned server lost the start election and no process holds it:
    /// nothing is left to publish, and the connect spawns again.
    ElectionUnheld,
    /// Nothing decided yet: the spawned server is still starting, or it lost
    /// the start election to a winner that may still be binding.
    Waiting,
}

/// Whether a spawned server's captured stderr names a lost start election:
/// its claim met a lock on the election file - a concurrent starter's, or a
/// probe's the operating system has not released yet - and it exited on its
/// own, printing the same `server_already_serving` refusal an operator sees
/// from `rift server start --foreground`. The marker is built from the CLI
/// registry so the match cannot drift from the code the binary actually
/// prints.
fn lost_start_election(capture: &CapturedStream) -> bool {
    let marker = format!(
        "error[{code}]",
        code = ErrorName::Cli(CliCode::ServerAlreadyServing).code()
    );
    capture.text.contains(&marker)
}

/// The refusal a request gets when the spawned server exited before it
/// published a live connection.
///
/// Names what the server printed to its own standard error. The caller
/// has no shell channel of its own, so it learns what actually failed
/// instead of being pointed at a command it cannot run.
fn server_start_failed(capture: &CapturedStream) -> ErrorData {
    let fault = if capture.text.is_empty() {
        SpawnFault::NoOutput
    } else {
        SpawnFault::CapturedStderr {
            text: capture.text.clone(),
            truncated: capture.truncated,
        }
    };
    Error::new(fault).tool_error(wire::ErrorPhase::Read)
}

/// Why a detached server spawn failed to start serving.
#[derive(Debug)]
enum SpawnFault {
    /// The spawned server exited before publishing its lock document, and
    /// wrote nothing to its own standard error.
    NoOutput,
    /// The spawned server exited before publishing its lock document; its
    /// captured standard error names what happened.
    CapturedStderr {
        /// The captured prefix.
        text: String,
        /// Whether the capture stopped short of the full stream.
        truncated: bool,
    },
}

impl Fault for SpawnFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::TemporarilyUnavailable)
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::NoOutput => vec![ErrorContext::new(
                "detail",
                "the spawned server exited before it started serving, with no output on its \
                 standard error",
            )],
            Self::CapturedStderr { text, truncated } => {
                let mut context = vec![
                    ErrorContext::new(
                        "detail",
                        "the spawned server exited before it started serving",
                    ),
                    ErrorContext::new("stderr", text.clone()),
                ];
                if *truncated {
                    context.push(ErrorContext::new("stderr_truncated", "true"));
                }
                context
            }
        }
    }
}

/// The recorded serving server as a live connection, when both halves hold.
///
/// A probe that names no serving server, and a recorded server that
/// refuses the connect or initialize, both answer `None`: the recorded
/// state is stale and the caller elects afresh.
async fn adopt_serving(
    root: &Path,
    identity: &ProductIdentity,
) -> Result<Option<RunningService<RoleClient, ()>>, ErrorData> {
    adopt_presence(probe(root), identity).await
}

/// The serving server one probe found, as a live connection, under the
/// same rules as [`adopt_serving`].
///
/// Split from it so a caller that also weighs the probe's election state
/// reads both from one probe.
async fn adopt_presence(
    presence: ServerPresence,
    identity: &ProductIdentity,
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
    require_identity_match(identity, &lock)?;
    match connect_recorded(&lock, UPSTREAM_CONNECT_TIMEOUT).await {
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

/// Refuses a serving process whose executable or served tools differ.
fn require_identity_match(expected: &ProductIdentity, lock: &ServerLock) -> Result<(), ErrorData> {
    if lock.identity == *expected {
        return Ok(());
    }
    Err(ErrorData::internal_error(
        format!(
            "workspace server identity differs from this rift process: server pid {}, version {}, executable digest {}, schema digest {}; this process version {}, executable digest {}, schema digest {}; run rift server stop, then retry",
            lock.pid,
            lock.identity.version,
            lock.identity.executable_digest,
            lock.identity.schema_digest,
            expected.version,
            expected.executable_digest,
            expected.schema_digest,
        ),
        None,
    ))
}

/// Why one bounded connect attempt produced no live connection.
#[derive(Debug)]
enum ConnectAttemptFailure {
    /// The transport refused or MCP initialization failed. Boxed to keep
    /// this failure small beside the large initialize error.
    Initialize(Box<ClientInitializeError>),
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
async fn connect_recorded(
    lock: &ServerLock,
    timeout: Duration,
) -> Result<RunningService<RoleClient, ()>, ConnectAttemptFailure> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{port}{MCP_PATH}",
            port = lock.port
        ))
        .auth_header(lock.token.clone()),
    );
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

/// The refusal a request gets when no server answered within the start
/// window.
fn upstream_unavailable() -> ErrorData {
    ErrorData::internal_error(
        format!(
            "no rift server answered for this workspace within {START_WAIT_MAX:?}; the \
             workspace has no server this caller can reach, and starting one is an \
             operator action on this host"
        ),
        None,
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
        self.forward(request, |peer, request| async move {
            peer.list_tools(request).await
        })
        .await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.forward(request, |peer, request| async move {
            peer.call_tool_once(request).await
        })
        .await
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.forward(request, |peer, request| async move {
            peer.list_resources(request).await
        })
        .await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.forward(request, |peer, request| async move {
            peer.list_resource_templates(request).await
        })
        .await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.forward(request, |peer, request| async move {
            peer.read_resource(request).await
        })
        .await
        .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use rift_protocol::lock::{ProductIdentity, SERVER_TOKEN_LENGTH, ServerLock};
    use rmcp::model::{ProtocolVersion, ServerCapabilities, ServerPeerInfo};
    use rmcp::service::{QuitReason, RoleClient, RunningService, serve_directly};
    use rmcp::transport::DynamicTransportError;
    use rmcp::{ErrorData, ServiceError};
    use serde_json::json;

    use super::{
        ConnectAttemptFailure, ProxyFault, RiftProxy, START_SPAWN_COUNT_MAX, SpawnPollOutcome,
        SpawnWatch, StartSpawns, StartupCapture, Upstream, UpstreamSlot, adopt_serving,
        connect_recorded, connect_upstream, fallback_info, forwarded_error, lost_start_election,
        mirrored_info, quit_reason_result, require_identity_match, reuse_current, serve_connection,
        server_start_failed, transport_failed, upstream_unavailable,
    };
    use crate::election::claim;
    use rift_core::{CapturedStream, Error};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    const EXECUTABLE_DIGEST_A: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const EXECUTABLE_DIGEST_B: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SCHEMA_DIGEST_A: &str =
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SCHEMA_DIGEST_B: &str =
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn identity(version: &str, executable_digest: &str, schema_digest: &str) -> ProductIdentity {
        ProductIdentity {
            version: version.to_owned(),
            executable_digest: executable_digest.to_owned(),
            schema_digest: schema_digest.to_owned(),
        }
    }

    fn test_identity() -> ProductIdentity {
        identity("0.0.11", EXECUTABLE_DIGEST_A, SCHEMA_DIGEST_A)
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
            identity: identity("0.0.11", EXECUTABLE_DIGEST_A, SCHEMA_DIGEST_A),
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
    fn matching_product_identities_are_accepted() {
        let expected = identity("0.0.11", EXECUTABLE_DIGEST_A, SCHEMA_DIGEST_A);
        let lock = recorded_lock(12_345);
        assert_eq!(require_identity_match(&expected, &lock), Ok(()));
    }

    #[test]
    fn same_version_with_a_different_executable_digest_is_refused() {
        let expected = identity("0.0.11", EXECUTABLE_DIGEST_B, SCHEMA_DIGEST_A);
        let lock = recorded_lock(12_345);
        let refusal = require_identity_match(&expected, &lock)
            .expect_err("another executable at the same version must be refused");
        assert!(refusal.message.contains(EXECUTABLE_DIGEST_A));
        assert!(refusal.message.contains(EXECUTABLE_DIGEST_B));
        assert!(refusal.message.contains("pid 4242"), "{}", refusal.message);
        assert!(
            refusal.message.contains("rift server stop"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn a_different_schema_digest_is_refused() {
        let expected = identity("0.0.11", EXECUTABLE_DIGEST_A, SCHEMA_DIGEST_B);
        let lock = recorded_lock(12_345);
        let refusal = require_identity_match(&expected, &lock)
            .expect_err("another served tool schema must be refused");
        assert!(refusal.message.contains(SCHEMA_DIGEST_A));
        assert!(refusal.message.contains(SCHEMA_DIGEST_B));
    }

    #[test]
    fn a_different_package_version_is_refused() {
        let expected = identity("0.0.12", EXECUTABLE_DIGEST_A, SCHEMA_DIGEST_A);
        let lock = recorded_lock(12_345);
        let refusal = require_identity_match(&expected, &lock)
            .expect_err("another package version must be refused");
        assert!(refusal.message.contains("0.0.11"));
        assert!(refusal.message.contains("0.0.12"));
    }

    #[test]
    fn unavailable_refusal_names_the_window_without_a_shell_command_the_caller_cannot_run() {
        let refusal = upstream_unavailable();
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

    /// A test double whose reads block on a channel, so a test controls
    /// exactly when the simulated pipe closes. A sent message larger than
    /// one read buffer is retained across calls, the same way a real
    /// pipe's bytes are.
    struct BlockingChannelStream {
        receiver: std::sync::mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    impl BlockingChannelStream {
        fn new(receiver: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
            Self {
                receiver,
                pending: Vec::new(),
            }
        }
    }

    impl std::io::Read for BlockingChannelStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.receiver.recv() {
                    Ok(bytes) => self.pending = bytes,
                    Err(_closed) => return Ok(0),
                }
            }
            let taken = self.pending.len().min(buffer.len());
            buffer[..taken].copy_from_slice(&self.pending[..taken]);
            self.pending.drain(..taken);
            Ok(taken)
        }
    }

    /// Polls `startup` until its capture is taken, bounded so a defect in
    /// the background drain fails the test instead of hanging it.
    fn wait_for_exit(startup: &mut StartupCapture) -> CapturedStream {
        for _ in 0..1_000 {
            if let Some(captured) = startup.exited() {
                return captured;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the background drain must finish once the stream closes");
    }

    /// The stderr a spawned server prints when it loses the start election.
    const LOST_ELECTION_STDERR: &[u8] =
        b"rift: error[server_already_serving]: another rift server \
        already serves this workspace; connect to the listed server, or run `rift server stop` \
        before serving again";

    /// Spawns watched through `stream` alone: one spawn ran, and its stderr is still draining.
    fn watching(stream: BlockingChannelStream) -> StartSpawns {
        StartSpawns {
            latest: SpawnWatch::Running(StartupCapture::spawn(stream)),
            spawn_count: 1,
        }
    }

    /// Polls `spawns` with no adoption until the latest spawn's exit is observed, asserting
    /// every poll before it waited; bounded so a defect in the drain fails instead of hanging.
    fn poll_until_exit_observed(spawns: &mut StartSpawns, election_held: bool) {
        for _ in 0..1_000 {
            if !matches!(spawns.latest, SpawnWatch::Running(_)) {
                return;
            }
            let outcome = spawns.poll::<u32>(None, election_held);
            assert!(
                matches!(outcome, SpawnPollOutcome::Waiting),
                "a poll that observes the loss waits"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the background drain must finish once the stream closes");
    }

    #[test]
    fn spawn_poll_prefers_adoption_over_a_finished_capture() {
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        drop(sender);
        // The capture has finished (the stream closed), and adoption also
        // succeeded on this same iteration: adoption must win, and the
        // capture must never be consulted.
        let SpawnWatch::Running(startup) = &mut spawns.latest else {
            panic!("the capture must still be present");
        };
        let _ = wait_for_exit(startup);
        let outcome = spawns.poll(Some(7_u32), false);
        assert!(
            matches!(outcome, SpawnPollOutcome::Ready(7)),
            "adoption must win over a finished capture"
        );
    }

    #[test]
    fn spawn_poll_waits_while_the_capture_is_still_open() {
        let (_sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        let outcome = spawns.poll::<u32>(None, false);
        assert!(matches!(outcome, SpawnPollOutcome::Waiting));
    }

    #[test]
    fn spawn_poll_fails_once_the_spawned_server_exited() {
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        sender
            .send(b"listener bind failed: address in use".to_vec())
            .expect("receiver still open");
        drop(sender);
        let outcome = loop {
            let outcome = spawns.poll::<u32>(None, false);
            if !matches!(outcome, SpawnPollOutcome::Waiting) {
                break outcome;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        let SpawnPollOutcome::Failed(refusal) = outcome else {
            panic!("an exited spawn must fail the poll");
        };
        assert!(
            refusal.message.contains("listener bind failed"),
            "{}",
            refusal.message
        );
    }

    /// A lost election never fails the poll: while another process holds the
    /// election, the winner may still be binding, and the poll keeps waiting
    /// for it.
    #[test]
    fn spawn_poll_keeps_waiting_while_the_election_the_spawned_server_lost_is_held() {
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        sender
            .send(LOST_ELECTION_STDERR.to_vec())
            .expect("receiver still open");
        drop(sender);
        poll_until_exit_observed(&mut spawns, true);
        for _ in 0..200 {
            let outcome = spawns.poll::<u32>(None, true);
            assert!(
                matches!(outcome, SpawnPollOutcome::Waiting),
                "a spawned server that lost the election to a live holder must keep the poll \
                 waiting for the winner, not fail it"
            );
        }
    }

    /// A lost election that leaves the election unheld has no winner to wait
    /// for, and the poll says so - but only on a probe read after the loss
    /// was observed, since the probe of the observing iteration may predate
    /// the losing server's exit.
    #[test]
    fn spawn_poll_names_an_unheld_election_only_after_the_loss_was_observed() {
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        sender
            .send(LOST_ELECTION_STDERR.to_vec())
            .expect("receiver still open");
        drop(sender);
        poll_until_exit_observed(&mut spawns, false);
        assert!(
            matches!(spawns.latest, SpawnWatch::LostElection),
            "the observed loss is recorded"
        );
        assert!(
            matches!(
                spawns.poll::<u32>(None, false),
                SpawnPollOutcome::ElectionUnheld
            ),
            "a later probe that finds the election unheld must send the connect to spawn again"
        );
        assert!(
            matches!(spawns.poll::<u32>(None, true), SpawnPollOutcome::Waiting),
            "a later probe that finds the election held waits for that holder"
        );
    }

    /// Below the bound a spawn is counted even when it cannot launch; at the
    /// bound nothing is spawned, and the poll waits out the window.
    #[test]
    fn the_spawn_count_bounds_the_spawns_one_connect_makes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing");
        let mut spawns = StartSpawns {
            latest: SpawnWatch::LostElection,
            spawn_count: START_SPAWN_COUNT_MAX - 1,
        };
        spawns.spawn(&missing);
        assert_eq!(spawns.spawn_count, START_SPAWN_COUNT_MAX);
        assert!(
            matches!(spawns.latest, SpawnWatch::Idle),
            "a spawn that cannot launch leaves nothing to watch"
        );

        spawns.latest = SpawnWatch::LostElection;
        spawns.spawn(&missing);
        assert_eq!(
            spawns.spawn_count, START_SPAWN_COUNT_MAX,
            "a spent count spawns nothing"
        );
        assert!(matches!(spawns.latest, SpawnWatch::Idle));
        assert!(
            matches!(spawns.poll::<u32>(None, false), SpawnPollOutcome::Waiting),
            "with the count spent the poll only waits"
        );
        Ok(())
    }

    #[test]
    fn lost_start_election_recognizes_the_server_already_serving_marker() {
        let capture = CapturedStream {
            text: "rift: error[server_already_serving]: another rift server already serves \
                   this workspace; connect to the listed server, or run `rift server stop` \
                   before serving again"
                .to_owned(),
            captured_bytes: 0,
            total_bytes: 0,
            truncated: false,
        };
        assert!(lost_start_election(&capture));
    }

    #[test]
    fn lost_start_election_rejects_an_unrelated_startup_failure() {
        let capture = CapturedStream {
            text: "listener bind failed: address in use".to_owned(),
            captured_bytes: 0,
            total_bytes: 0,
            truncated: false,
        };
        assert!(!lost_start_election(&capture));
    }

    #[test]
    fn server_start_failed_names_no_output_when_stderr_was_empty() {
        let refusal = server_start_failed(&CapturedStream::default());
        assert!(refusal.message.contains("no output"), "{}", refusal.message);
    }

    #[test]
    fn server_start_failed_carries_the_captured_stderr_text() {
        let capture = CapturedStream {
            text: "panicked at src/main.rs:1".to_owned(),
            captured_bytes: 26,
            total_bytes: 26,
            truncated: false,
        };
        let refusal = server_start_failed(&capture);
        assert!(
            refusal.message.contains("panicked at src/main.rs:1"),
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
    fn proxy_faults_carry_registry_codes_and_sources() {
        let quit = Error::new(ProxyFault::UnexpectedQuit);
        assert_eq!(quit.descriptor().code(), "internal_error");
        let rendered = quit.to_string();
        assert!(
            rendered.contains("MCP service ended unexpectedly"),
            "{rendered}"
        );
        assert!(std::error::Error::source(&quit).is_none());

        let initialize = Error::new(ProxyFault::Initialize(Box::new(
            rmcp::service::ServerInitializeError::Cancelled,
        )));
        assert_eq!(
            initialize.descriptor().code(),
            "temporarily_unavailable",
            "an initialization failure must classify as transient"
        );
        let rendered_initialize = initialize.to_string();
        assert!(
            rendered_initialize.contains("MCP initialization failed"),
            "{rendered_initialize}"
        );
        assert!(std::error::Error::source(&initialize).is_some());
    }

    #[tokio::test]
    async fn forward_maps_a_non_transport_failure_without_retry() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity());
        let (running, _upstream_alive) = direct_upstream();
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let refusal = proxy
            .forward((), |_peer, ()| async {
                Err::<(), _>(ServiceError::UnexpectedResponse)
            })
            .await
            .expect_err("a non-transport failure must refuse without retry");
        assert!(
            refusal.message.contains("forwarded request"),
            "{}",
            refusal.message
        );
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
            adopt_serving(directory.path(), &test_identity())
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
        assert_eq!(refusal.message, upstream_unavailable().message);
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
        assert_eq!(refusal.message, upstream_unavailable().message);
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
    async fn task_fault_preserves_the_join_source() {
        let task = tokio::spawn(async { panic!("test join failure") });
        let join = task.await.expect_err("test task must fail");
        let error = quit_reason_result(QuitReason::JoinError(join))
            .expect_err("join error quit reason must fail");
        assert_eq!(error.descriptor().code(), "internal_error");
        assert!(error.to_string().contains("MCP service task failed"));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[tokio::test]
    async fn closed_transport_fails_initialization() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let (server_transport, client_transport) = tokio::io::duplex(1024);
        drop(client_transport);
        let error = serve_connection(
            RiftProxy::new(directory.path(), test_identity()),
            server_transport,
        )
        .await
        .expect_err("a closed transport must fail initialization");
        assert!(matches!(error.fault(), ProxyFault::Initialize(_)));
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
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
