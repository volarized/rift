//! Stateless stdio proxy to the workspace's elected rift server.
//!
//! `rift mcp` serves agents over stdio while the workspace's state lives in
//! one `rift server` process. Every request forwards to that server: the
//! proxy adopts a recorded server when the lock document names a live one,
//! starts a detached server when the workspace has none, and reconnects -
//! single-flight, one retry per request - when the server it held goes
//! away, so an agent session survives server restarts.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::http::{HeaderName, HeaderValue};
use rift_core::CapturedStream;
use rift_core::constants::{
    INDEX_DATABASE_FILE_NAME, METRICS_DATABASE_FILE_NAME, RIFT_STATE_DIRECTORY,
    WRITE_AHEAD_LOG_SUFFIX,
};
use rift_error::{RiftError, errors};
use rift_protocol::configuration::ServerConfiguration;
use rift_protocol::error as wire;
use rift_protocol::identity::SymbolOwner;
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

#[cfg(test)]
use crate::election::probe;
use crate::election::{
    ElectionObservation, ServerPresence, StaleReason, observe, presence_field,
    probe_state_directory,
};
use crate::failure::{McpErrorExt as _, WireFailure as _};
use crate::http::{MCP_PATH, StopRequestFailure, WORKSPACE_ROOT_HEADER, request_stop};
use crate::identity::BuildCheckout;
use crate::metrics::{
    Ending, MCP_CLIENT_OPERATION_DURATION, McpRequest, RESOURCE_TEMPLATES_LIST, RESOURCES_LIST,
    RESOURCES_READ, TOOLS_LIST,
};
use crate::output::OutputPolicy;
use crate::repository::{
    ServerConfigurationSelection, repository_election_directory, select_server_configuration,
};
use crate::spawn::{
    PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, SpawnPollOutcome,
    StartSpawns, StartupCapture,
};
#[cfg(test)]
use crate::validation::ConfigurationState;

mod candidates;
mod identity;
pub(crate) use candidates::normalized_params;
mod extra;
mod http_client;
pub(crate) use candidates::captured_expiry;
pub(crate) use http_client::encode_bounded;

/// Bound on one upstream connect-and-initialize attempt.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Allowance past the server's own request bounds for its answer to be
/// rendered and to travel back to the proxy.
const FORWARD_ANSWER_GRACE: Duration = Duration::from_secs(10);

/// Serves agents over stdio MCP, forwarding every request to the
/// workspace's elected server, as the build `checkout` describes.
///
/// `output` selects which representations of tool answers the proxy
/// forwards; it stays fixed for the proxy's lifetime and across upstream
/// reconnects.
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
pub async fn serve_proxy(
    root: &Path,
    checkout: BuildCheckout,
    output: OutputPolicy,
    extra: &[String],
    home: Option<&Path>,
) -> Result<(), RiftError> {
    rift_tracing::info!(
        component = "mcp",
        transport = "stdio",
        phase = "start",
        "MCP proxy starting"
    );
    let configuration = rift_server::load_configuration(root)?;
    let registrations = extra::Registrations::accept(root, &configuration.mcp, extra, home)?;
    let identity = crate::identity::product_identity(checkout)
        .await
        .map_err(|error| errors::mcp::proxy_identity_failed().source(error).error())?;
    let proxy =
        RiftProxy::with_configuration(root, identity, output, &configuration.server, registrations);
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
    rift_tracing::info!(
        component = "mcp",
        transport = "stdio",
        outcome = "ok",
        "MCP proxy ready"
    );
    let reason = service.waiting().await;
    let outcome = reason
        .map_err(|error| errors::mcp::proxy_task_failed().source(error).error())
        .and_then(quit_reason_result);
    rift_tracing::info!(
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
        rift_tracing::warn!(
            component = "mcp",
            refusal = %refusal,
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
    accepted_root: Option<Arc<Path>>,
    owner: SymbolOwner,
    identity: Arc<ProductIdentity>,
    upstream: Arc<tokio::sync::Mutex<UpstreamSlot>>,
    advertised: Arc<std::sync::Mutex<Option<ServerConfig>>>,
    /// How long one forwarded request may go unanswered: see [`forward_budget`].
    forward_budget: Duration,
    /// Which representations of tool answers this proxy forwards. It lives
    /// here, not on the upstream slot, so a reconnect keeps it.
    output: OutputPolicy,
    /// Named roots retained for this connection, including reconnects.
    extra: Arc<extra::Registrations>,
    backends: Arc<BTreeMap<String, RiftProxy>>,
    candidate_views: Arc<candidates::CandidateViews>,
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
    result: std::sync::Mutex<Option<Result<(), RiftError>>>,
    finished: tokio::sync::Notify,
}

impl ConnectAttempt {
    fn pending() -> Self {
        Self {
            result: std::sync::Mutex::new(None),
            finished: tokio::sync::Notify::new(),
        }
    }

    fn complete(&self, result: Result<(), RiftError>) {
        *self.lock_result() = Some(result);
        self.finished.notify_waiters();
    }

    async fn outcome(&self) -> Result<(), RiftError> {
        loop {
            let notified = self.finished.notified();
            if let Some(result) = self.lock_result().clone() {
                return result;
            }
            notified.await;
        }
    }

    fn lock_result(&self) -> std::sync::MutexGuard<'_, Option<Result<(), RiftError>>> {
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
    /// `[server]` table the workspace accepts when the proxy starts, and
    /// forwarding tool answers as `output` selects.
    #[cfg(test)]
    fn new(root: &Path, identity: ProductIdentity, output: OutputPolicy) -> Self {
        let server = ConfigurationState::accept(root).server_configuration();
        Self::with_configuration(
            root,
            identity,
            output,
            &server,
            extra::Registrations::accept(
                root,
                &rift_protocol::configuration::McpConfiguration::default(),
                &[],
                None,
            )
            .expect("the fixture has an accepted primary root"),
        )
    }

    fn with_configuration(
        root: &Path,
        identity: ProductIdentity,
        output: OutputPolicy,
        server: &ServerConfiguration,
        extra: extra::Registrations,
    ) -> Self {
        let backends = extra
            .entries()
            .map(|(name, root)| {
                let mut backend = Self::for_owner(
                    root,
                    SymbolOwner::Local,
                    identity.clone(),
                    output,
                    forward_budget(server),
                );
                backend.accepted_root = Some(Arc::from(root));
                (name.to_owned(), backend)
            })
            .collect();
        let mut proxy = Self::for_owner(
            root,
            SymbolOwner::Local,
            identity,
            output,
            forward_budget(server),
        );
        proxy.accepted_root = extra.primary_root().map(Arc::from);
        proxy.extra = Arc::new(extra);
        proxy.backends = Arc::new(backends);
        proxy
    }

    fn for_owner(
        root: &Path,
        owner: SymbolOwner,
        identity: ProductIdentity,
        output: OutputPolicy,
        forward_budget: Duration,
    ) -> Self {
        Self {
            root: Arc::from(root),
            accepted_root: None,
            owner,
            identity: Arc::new(identity),
            upstream: Arc::new(tokio::sync::Mutex::new(UpstreamSlot::empty())),
            advertised: Arc::new(std::sync::Mutex::new(None)),
            forward_budget,
            output,
            extra: Arc::new(extra::Registrations::default()),
            backends: Arc::new(BTreeMap::new()),
            candidate_views: Arc::new(candidates::CandidateViews::default()),
        }
    }

    /// A forwarded `tools/call` answer, as this proxy's output policy
    /// selects it.
    ///
    /// Only a completed result carries `structuredContent`; an input
    /// request and a task pass through untouched.
    fn selected_response(&self, response: CallToolResponse) -> CallToolResponse {
        match response {
            CallToolResponse::Complete(result) => {
                CallToolResponse::Complete(self.output.select_result(result))
            }
            other => other,
        }
    }

    fn call_backend(&self, request: &CallToolRequestParams) -> Result<&Self, RiftError> {
        let Some(arguments) = request.arguments.as_ref() else {
            return Ok(self);
        };
        let traversal = arguments.get("traversal");
        let identity_owner = arguments
            .get("id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                traversal
                    .and_then(|value| value.get("seed"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(rift_protocol::identity::SymbolIdentity::parse)
            .transpose()?
            .map(|identity| identity.owner().clone())
            .filter(|owner| matches!(owner, SymbolOwner::Local | SymbolOwner::NamedLocal { .. }));
        let scope = arguments
            .get("scope")
            .map(|value| serde_json::from_value::<rift_protocol::read::SearchScope>(value.clone()))
            .transpose()
            .map_err(|_| Self::scope_refusal(request, "scope"))?;
        let scope_owner = match &scope {
            Some(rift_protocol::read::SearchScope::Local) => Some(SymbolOwner::Local),
            Some(rift_protocol::read::SearchScope::NamedLocal { name }) => {
                Some(SymbolOwner::NamedLocal { name: name.clone() })
            }
            _ => None,
        };
        if let (Some(identity), Some(scope)) = (&identity_owner, &scope_owner)
            && identity != scope
        {
            return Err(Self::scope_refusal(request, "scope"));
        }
        if traversal.is_none()
            && matches!(scope, Some(rift_protocol::read::SearchScope::All))
            && self.extra.names().next().is_some()
        {
            return errors::server::read_unavailable()
                .operation(request.name.as_ref())
                .detail("registered project aggregation is unavailable")
                .fail();
        }
        let selected = scope_owner.or(identity_owner);
        match selected {
            Some(SymbolOwner::NamedLocal { name }) => {
                if matches!(scope, Some(rift_protocol::read::SearchScope::Global))
                    || (traversal.is_none()
                        && matches!(scope, Some(rift_protocol::read::SearchScope::All)))
                {
                    return Err(Self::scope_refusal(request, "scope"));
                }
                self.backends
                    .get(&name)
                    .ok_or_else(|| self.extra.refusal(&name, &request.name, "scope"))
            }
            _ => Ok(self),
        }
    }

    fn scope_refusal(request: &CallToolRequestParams, field: &str) -> RiftError {
        errors::mcp::parameter_invalid()
            .tool(request.name.as_ref())
            .field(field)
            .accepted("local, global, all, or an accepted named project")
            .error()
    }

    /// A forwarded `resources/read` answer, as this proxy's output policy
    /// selects it.
    ///
    /// Only a completed read carries contents; an input request passes
    /// through untouched.
    fn selected_resource(&self, response: ReadResourceResponse) -> ReadResourceResponse {
        match response {
            ReadResourceResponse::Complete(result) => {
                ReadResourceResponse::Complete(self.output.select_resource(result))
            }
            other => other,
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
    ) -> Result<(Peer<RoleClient>, u64), RiftError> {
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
            let connected =
                connect_upstream_for_owner(&proxy.root, &proxy.owner, &proxy.identity).await;
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
        self.forward_registered(request, answer)
            .await
            .map_err(|error| error.mcp().tool_error(wire::ErrorPhase::Read))?
    }

    async fn forward_registered<Value>(
        &self,
        request: ClientRequest,
        answer: fn(ServerResult) -> Option<Value>,
    ) -> Result<Result<Value, ErrorData>, RiftError> {
        let (peer, generation) = self.leased_peer(None).await?;
        let failure = match self.answered(&peer, request.clone()).await? {
            Ok(result) => return answered_as(result, answer).map(Ok),
            Err(error) if transport_failed(&error) => error,
            Err(error) => return forwarded_error(error).map(Err),
        };
        rift_tracing::info!(
            component = "mcp",
            failure = %failure,
            "upstream connection lost; reconnecting"
        );
        let (peer, _generation) = self.leased_peer(Some(generation)).await?;
        match self.answered(&peer, request).await? {
            Ok(result) => answered_as(result, answer).map(Ok),
            Err(error) => forwarded_error(error).map(Err),
        }
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
    ) -> Result<Result<ServerResult, ServiceError>, RiftError> {
        let budget = self.forward_budget;
        let answer = match peer
            .send_cancellable_request(request, PeerRequestOptions::with_timeout(budget))
            .await
        {
            Ok(handle) => {
                // rmcp chooses the id inside the send, so it is known only once the
                // send returns; the forwarding span declared `request_id` empty to take
                // the id the server's request span carries, so the forward's records
                // share the server request's `request_id`. After a reconnect the second
                // send's id replaces the first.
                rift_tracing::Span::current().record("request_id", handle.id.to_string().as_str());
                rift_tracing::debug!(component = "mcp", request_id = %handle.id, "forwarded request awaiting response");
                let result = handle.await_response().await;
                rift_tracing::debug!(
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
                rift_tracing::warn!(
                    component = "mcp",
                    budget = ?budget,
                    "the workspace server did not answer a forwarded request within its budget; \
                     the request is cancelled"
                );
                rift_tracing::publish_in_flight("forward budget");
                errors::mcp::forward_unanswered().waited(budget).fail()
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
) -> Result<Value, RiftError> {
    answer(result).ok_or_else(|| {
        errors::mcp::forward_failed()
            .source(ServiceError::UnexpectedResponse)
            .error()
    })
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
/// its first index and wrote the index or the metrics database during the window,
/// the refusal is one the caller resends; any other exhaustion refuses with
/// the operator's next step.
///
/// A workspace whose settings select a repository server asks that server
/// first, and again on each poll round while its answer is
/// [`RepositoryAsk::Transient`]: the repository server claims the
/// workspace's election and publishes no document there, so the workspace's
/// own election probes as [`ServerPresence::Starting`] until the window
/// closes. A [`RepositoryAsk::Terminal`] answer ends the asking.
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
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    connect_upstream_with(root, identity, || connect_repository_server(root, identity)).await
}

async fn connect_upstream_for_owner(
    root: &Path,
    owner: &SymbolOwner,
    identity: &ProductIdentity,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    if matches!(owner, SymbolOwner::Local) {
        return connect_upstream(root, identity).await;
    }
    crate::storage::workspace_state_directory(root, owner)?;
    connect_upstream_for_owner_with(root, owner, identity, || async {
        RepositoryAsk::Terminal(RepositoryMiss::NotRepository)
    })
    .await
}

/// [`connect_upstream`] over any ask of the repository server, so a test
/// can order what each ask answers.
async fn connect_upstream_with<Asking: Future<Output = RepositoryAsk>>(
    root: &Path,
    identity: &ProductIdentity,
    ask_repository: impl FnMut() -> Asking,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    connect_upstream_for_owner_with(root, &SymbolOwner::Local, identity, ask_repository).await
}

async fn connect_upstream_for_owner_with<Asking: Future<Output = RepositoryAsk>>(
    root: &Path,
    owner: &SymbolOwner,
    identity: &ProductIdentity,
    mut ask_repository: impl FnMut() -> Asking,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    let started = tokio::time::Instant::now();
    let mut reported = None;
    let mut closing = StartWindowClose::default();
    let asked = ask_repository().await;
    asked.report_change(&mut reported);
    let mut repository_transient = match asked {
        RepositoryAsk::Connected(running) => return Ok(running),
        RepositoryAsk::Transient(miss) => {
            closing.repository_miss = Some(miss);
            true
        }
        RepositoryAsk::Terminal(miss) => {
            closing.repository_miss = Some(miss);
            false
        }
    };
    let mut replacement = Replacement::default();
    let presence = observed_for_owner(root, owner).await?.presence;
    if let Some(running) =
        adopt_presence_for_owner(root, owner, presence, identity, &mut replacement).await?
    {
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
    if !awaiting_release
        && !matches!(
            observed_for_owner(root, owner).await?.presence,
            ServerPresence::Starting
        )
    {
        spawn_for_owner(&mut spawns, root, owner)?;
    }
    let opened = DatabaseActivity::observed_for_owner(root, owner).await?;
    let mut building = false;
    let mut still_serving = None;
    let mut probe_reads = None;
    let deadline = tokio::time::Instant::now() + START_WAIT_MAX;
    for _ in 0..START_POLL_ATTEMPT_COUNT {
        let observation = observed_for_owner(root, owner).await?;
        observation.report_change(&mut probe_reads);
        let presence = observation.presence;
        closing.rounds += 1;
        closing.presence = presence_field(&presence);
        building = matches!(presence, ServerPresence::Starting);
        let election_held = presence.election_held();
        still_serving = replacement.refusal_while_serving(&presence, identity);
        let adopted =
            adopt_presence_for_owner(root, owner, presence, identity, &mut replacement).await?;
        // The replaced server released the election, and no other starter
        // took it: this process starts the server it replaces it with.
        if awaiting_release && !election_held {
            awaiting_release = false;
            spawn_for_owner(&mut spawns, root, owner)?;
        }
        match spawns.poll(adopted, election_held) {
            SpawnPollOutcome::Ready(running) => {
                closing.record_answered(started.elapsed());
                return Ok(running);
            }
            SpawnPollOutcome::Failed(capture) => return Err(server_start_failed(&capture)),
            SpawnPollOutcome::ElectionUnheld => spawn_for_owner(&mut spawns, root, owner)?,
            SpawnPollOutcome::Waiting => {}
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PRESENCE_POLL_INTERVAL).await;
        if repository_transient {
            let asked = ask_repository().await;
            asked.report_change(&mut reported);
            match asked {
                RepositoryAsk::Connected(running) => {
                    closing.record_answered(started.elapsed());
                    return Ok(running);
                }
                RepositoryAsk::Transient(miss) => closing.repository_miss = Some(miss),
                RepositoryAsk::Terminal(miss) => {
                    repository_transient = false;
                    closing.repository_miss = Some(miss);
                }
            }
        }
    }
    closing.building = building;
    if let Some(refusal) = still_serving {
        closing.record(started.elapsed(), None, &refusal);
        return Err(refusal);
    }
    let closed = DatabaseActivity::observed_for_owner(root, owner).await?;
    let wrote = opened != closed;
    let refusal = start_window_refusal(building && wrote);
    closing.record(started.elapsed(), Some(wrote), &refusal);
    Err(refusal)
}

fn spawn_for_owner(
    spawns: &mut StartSpawns<StartupCapture>,
    root: &Path,
    owner: &SymbolOwner,
) -> Result<(), RiftError> {
    if matches!(owner, SymbolOwner::Local) {
        spawns.spawn_captured(root);
        return Ok(());
    }
    spawns.spawn_captured_with_owner(root, owner)
}

async fn observed_for_owner(
    root: &Path,
    owner: &SymbolOwner,
) -> Result<ElectionObservation, RiftError> {
    if matches!(owner, SymbolOwner::Local) {
        return Ok(observed(root).await);
    }
    let root = root.to_path_buf();
    let owner = owner.clone();
    tokio::task::spawn_blocking(move || crate::election::observe_with_owner(&root, &owner))
        .await
        .map_err(|error| errors::mcp::proxy_task_failed().source(error).error())?
}

async fn adopt_presence_for_owner(
    root: &Path,
    owner: &SymbolOwner,
    presence: ServerPresence,
    identity: &ProductIdentity,
    replacement: &mut Replacement,
) -> Result<Option<RunningService<RoleClient, ()>>, RiftError> {
    if let ServerPresence::Serving(lock) = &presence {
        crate::election::validate_workspace_lock(root, owner, lock)?;
    }
    adopt_presence(root, presence, identity, replacement).await
}

/// What a start window last saw, logged once as the window closes: on a
/// server that answers, or beside the refusal it returns.
#[derive(Debug, Default)]
struct StartWindowClose {
    /// The poll rounds the window ran.
    rounds: u32,
    /// The workspace election's presence at the last round.
    presence: String,
    /// The last ask of the repository server, when it missed.
    repository_miss: Option<RepositoryMiss>,
    /// Whether the last round found the election held with no document.
    building: bool,
}

impl StartWindowClose {
    /// Logs the window's close after `elapsed`, with `wrote` naming whether
    /// a database file changed during the window when the close read it, and
    /// the `refusal` the request gets.
    fn record(&self, elapsed: Duration, wrote: Option<bool>, refusal: &RiftError) {
        rift_tracing::info!(
            component = "mcp",
            rounds = self.rounds,
            elapsed_ms = elapsed.as_millis(),
            presence = %self.presence,
            repository_miss = self.repository_miss.as_ref().map(|miss| format!("{miss:?}")),
            building = self.building,
            wrote,
            refusal = %refusal,
            "start window closed without a server that answers"
        );
    }

    /// Logs the window's close after `elapsed` on a server that answers: the poll rounds
    /// it ran and the workspace election's presence at the last round.
    fn record_answered(&self, elapsed: Duration) {
        rift_tracing::info!(
            component = "mcp",
            rounds = self.rounds,
            elapsed_ms = elapsed.as_millis(),
            presence = %self.presence,
            outcome = "ok",
            "start window closed with a server that answers"
        );
    }
}

/// What the index and metrics databases' files looked like at one instant: each
/// file's length and modification time, or nothing for an absent file.
///
/// A server writes `.rift/index` while it builds its first index and records its
/// diagnostics into `.rift/metrics`, and `SQLite` in WAL mode appends each commit to
/// the database's write-ahead log. Two readings that differ in any of the four files
/// therefore show that a process wrote between them.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DatabaseActivity {
    files: [Option<(u64, SystemTime)>; 4],
}

impl DatabaseActivity {
    async fn observed_for_owner(root: &Path, owner: &SymbolOwner) -> Result<Self, RiftError> {
        if matches!(owner, SymbolOwner::Local) {
            return Ok(Self::observed(root).await);
        }
        let state = crate::storage::workspace_state_directory(root, owner)?;
        tokio::task::spawn_blocking(move || Self::read_state(&state))
            .await
            .map_err(|error| errors::mcp::proxy_task_failed().source(error).error())
    }

    /// Reads each database file and its write-ahead log below `root`, and none
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
            .unwrap_or(Self { files: [None; 4] })
    }

    /// Reads the index and metrics database files and their write-ahead logs below
    /// `root`.
    fn read(root: &Path) -> Self {
        let state = root.join(RIFT_STATE_DIRECTORY);
        Self::read_state(&state)
    }

    fn read_state(state: &Path) -> Self {
        let log = |database: &str| state.join(format!("{database}{WRITE_AHEAD_LOG_SUFFIX}"));
        let paths = [
            state.join(INDEX_DATABASE_FILE_NAME),
            log(INDEX_DATABASE_FILE_NAME),
            state.join(METRICS_DATABASE_FILE_NAME),
            log(METRICS_DATABASE_FILE_NAME),
        ];
        Self {
            files: paths.map(|path| {
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
fn start_window_refusal(building: bool) -> RiftError {
    if building {
        return errors::mcp::start_building().waited(START_WAIT_MAX).error();
    }
    errors::mcp::start_unreachable()
        .waited(START_WAIT_MAX)
        .error()
}

/// The refusal a request gets when the spawned server exited before it
/// published a live connection.
///
/// Names what the server printed to its own standard error. The caller
/// has no shell channel of its own, so it learns what actually failed
/// instead of being pointed at a command it cannot run.
fn server_start_failed(capture: &CapturedStream) -> RiftError {
    if capture.text.is_empty() {
        errors::mcp::spawn_no_output().error()
    } else {
        errors::mcp::spawn_failed()
            .stderr(capture.text.clone())
            .stderr_truncated(capture.truncated)
            .error()
    }
}

/// The recorded serving server as a live connection, when both halves hold.
///
/// A probe that names no serving server, and a recorded server that
/// refuses the connect or initialize, both answer `None`: the recorded
/// state is stale and the caller elects afresh. A server of another
/// identity answers as [`ServerStanding`] decides.
#[cfg(test)]
async fn adopt_serving(
    root: &Path,
    identity: &ProductIdentity,
    replacement: &mut Replacement,
) -> Result<Option<RunningService<RoleClient, ()>>, RiftError> {
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
#[cfg(test)]
async fn probed(root: &Path) -> ServerPresence {
    probed_with(root, probe).await
}

/// [`probed`] with the reads that decided the presence, so the start poll
/// can log a read that failed.
///
/// # Cancel safety
///
/// Dropping this future abandons the answer; the probe itself finishes on
/// the blocking pool and changes nothing.
async fn observed(root: &Path) -> ElectionObservation {
    let root = root.to_path_buf();
    let fallback = ElectionObservation::unobservable(&root);
    tokio::task::spawn_blocking(move || observe(&root))
        .await
        .unwrap_or(fallback)
}

/// [`probed`] over any probe, so a test can hold one probe open.
#[cfg(test)]
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
) -> Result<Option<RunningService<RoleClient, ()>>, RiftError> {
    let lock = match presence {
        ServerPresence::Serving(lock) => *lock,
        ServerPresence::Stale(StaleReason::PortUnreachable { pid }) => {
            rift_tracing::info!(
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
        ServerStanding::Refuse => return Err(identity_refusal(identity, &lock)),
    }
    match connect_recorded_for_root(&lock, UPSTREAM_CONNECT_TIMEOUT, root).await {
        Ok(running) => {
            rift_tracing::info!(
                component = "mcp",
                port = lock.port,
                pid = lock.pid,
                "proxy connected to workspace server"
            );
            Ok(Some(running))
        }
        Err(failure) => {
            rift_tracing::info!(
                component = "mcp",
                failure = %failure,
                "recorded server did not answer; treating the lock as stale"
            );
            Ok(None)
        }
    }
}

/// What one ask of the repository server answered.
#[derive(Debug)]
enum RepositoryAsk {
    /// The repository server answered for this workspace.
    Connected(RunningService<RoleClient, ()>),
    /// A later ask within the start window can connect: the repository
    /// server is not serving yet, or did not answer the connect while it
    /// builds the workspace inside the initialize request.
    Transient(RepositoryMiss),
    /// No later ask within the start window connects: the workspace selects
    /// its own server, or the repository server is one this process does not
    /// adopt.
    Terminal(RepositoryMiss),
}

impl RepositoryAsk {
    /// Logs this ask's miss when its arm differs from the arm `reported`
    /// holds, so a start window of repeated misses logs each change of arm
    /// once, not once per poll round.
    fn report_change(&self, reported: &mut Option<std::mem::Discriminant<RepositoryMiss>>) {
        let (Self::Transient(miss) | Self::Terminal(miss)) = self else {
            return;
        };
        let arm = std::mem::discriminant(miss);
        if *reported != Some(arm) {
            *reported = Some(arm);
            miss.report();
        }
    }
}

/// Why one ask of the repository server did not connect.
#[derive(Debug)]
enum RepositoryMiss {
    /// The workspace's settings select its own server.
    NotRepository,
    /// The workspace configuration was refused, or reading it did not finish.
    SelectionRefused { detail: String },
    /// The repository election directory could not be named.
    ElectionDirectory { detail: String },
    /// The repository election names no serving server.
    NotServing { presence: String },
    /// The serving server records other settings or another identity.
    NotAdopted {
        pid: u32,
        settings_match: bool,
        identity_adopted: bool,
    },
    /// The serving server did not answer the connect.
    Unanswered {
        pid: u32,
        port: u16,
        elapsed_ms: u128,
        detail: String,
    },
}

impl RepositoryMiss {
    /// Logs this miss. A workspace that selects its own server is the
    /// ordinary case and logs nothing.
    fn report(&self) {
        match self {
            Self::NotRepository => {}
            Self::SelectionRefused { detail } => rift_tracing::info!(
                component = "mcp",
                %detail,
                "server configuration selection refused; polling the workspace election"
            ),
            Self::ElectionDirectory { detail } => rift_tracing::info!(
                component = "mcp",
                %detail,
                "repository election directory unavailable; polling the workspace election"
            ),
            Self::NotServing { presence } => rift_tracing::info!(
                component = "mcp",
                %presence,
                "repository server not serving; polling the workspace election"
            ),
            Self::NotAdopted {
                pid,
                settings_match,
                identity_adopted,
            } => rift_tracing::info!(
                component = "mcp",
                pid,
                settings_match,
                identity_adopted,
                "repository server not adopted; polling the workspace election"
            ),
            Self::Unanswered {
                pid,
                port,
                elapsed_ms,
                detail,
            } => rift_tracing::info!(
                component = "mcp",
                pid,
                port,
                elapsed_ms,
                %detail,
                "repository server did not answer; polling the workspace election"
            ),
        }
    }
}

/// Asks the repository server the workspace's settings select, once.
///
/// # Cancel safety
///
/// Dropping this future abandons the ask; it changes nothing.
async fn connect_repository_server(
    workspace_root: &Path,
    identity: &ProductIdentity,
) -> RepositoryAsk {
    let root = workspace_root.to_path_buf();
    let selection =
        match tokio::task::spawn_blocking(move || select_server_configuration(&root, None)).await {
            Ok(Ok(selection)) => selection,
            Ok(Err(refusal)) => {
                return RepositoryAsk::Terminal(RepositoryMiss::SelectionRefused {
                    detail: refusal.message.to_string(),
                });
            }
            Err(error) => {
                return RepositoryAsk::Terminal(RepositoryMiss::SelectionRefused {
                    detail: error.to_string(),
                });
            }
        };
    let ServerConfigurationSelection::Repository {
        common_directory,
        server,
        ..
    } = selection
    else {
        return RepositoryAsk::Terminal(RepositoryMiss::NotRepository);
    };
    let state_directory = match repository_election_directory(&common_directory, identity) {
        Ok(state_directory) => state_directory,
        Err(refusal) => {
            return RepositoryAsk::Terminal(RepositoryMiss::ElectionDirectory {
                detail: refusal.message.to_string(),
            });
        }
    };
    let presence = tokio::task::spawn_blocking(move || probe_state_directory(&state_directory))
        .await
        .unwrap_or(ServerPresence::Stale(StaleReason::ElectionUnobservable));
    let ServerPresence::Serving(lock) = presence else {
        return RepositoryAsk::Transient(RepositoryMiss::NotServing {
            presence: format!("{presence:?}"),
        });
    };
    let settings_match = lock.server.as_ref() == Some(&server);
    let identity_adopted = matches!(
        ServerStanding::of(identity, &lock.identity),
        ServerStanding::Adopt
    );
    if !settings_match || !identity_adopted {
        return RepositoryAsk::Terminal(RepositoryMiss::NotAdopted {
            pid: lock.pid,
            settings_match,
            identity_adopted,
        });
    }
    let started = tokio::time::Instant::now();
    match connect_recorded_for_root(&lock, UPSTREAM_CONNECT_TIMEOUT, workspace_root).await {
        Ok(running) => RepositoryAsk::Connected(running),
        Err(failure) => RepositoryAsk::Transient(RepositoryMiss::Unanswered {
            pid: lock.pid,
            port: lock.port,
            elapsed_ms: started.elapsed().as_millis(),
            detail: failure.to_string(),
        }),
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
    ) -> Result<(), RiftError> {
        match &self.asked {
            Some(asked) if names_one_server(asked, lock) => return Ok(()),
            Some(_) => return Err(identity_refusal(identity, lock)),
            None => {}
        }
        match request_stop(lock).await {
            Ok(()) => self.accepted = true,
            Err(StopRequestFailure::Failed(failure)) => rift_tracing::warn!(
                component = "mcp",
                pid = lock.pid,
                failure = ?failure,
                "the stop request to the workspace server failed in transport; the election decides"
            ),
            Err(failure @ StopRequestFailure::Refused(_)) => {
                rift_tracing::warn!(
                    component = "mcp",
                    pid = lock.pid,
                    failure = ?failure,
                    "the workspace server did not accept the stop request"
                );
                return Err(identity_refusal(identity, lock));
            }
        }
        let server_version = lock.identity.version.as_str();
        rift_tracing::info!(
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
    ) -> Option<RiftError> {
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
fn identity_refusal(expected: &ProductIdentity, lock: &ServerLock) -> RiftError {
    errors::mcp::server_identity_mismatch()
        .pid(lock.pid)
        .server_version(&lock.identity.version)
        .server_schema_digest(&lock.identity.schema_digest)
        .version(&expected.version)
        .schema_digest(&expected.schema_digest)
        .error()
}

/// Connects and initializes against one recorded server, bounded by
/// `timeout`.
///
/// The caller passes [`UPSTREAM_CONNECT_TIMEOUT`].
#[cfg(test)]
async fn connect_recorded(
    lock: &ServerLock,
    timeout: Duration,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    connect_recorded_with_root(lock, timeout, None).await
}

async fn connect_recorded_for_root(
    lock: &ServerLock,
    timeout: Duration,
    root: &Path,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    connect_recorded_with_root(lock, timeout, Some(root)).await
}

async fn connect_recorded_with_root(
    lock: &ServerLock,
    timeout: Duration,
    root: Option<&Path>,
) -> Result<RunningService<RoleClient, ()>, RiftError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(format!(
        "http://127.0.0.1:{port}{MCP_PATH}",
        port = lock.port
    ))
    .auth_header(lock.token.clone());
    if let Some(root) = root {
        let canonical = std::fs::canonicalize(root).map_err(|error| {
            errors::mcp::proxy_initialization_failed()
                .source(error)
                .error()
        })?;
        let value = HeaderValue::from_str(&canonical.to_string_lossy()).map_err(|error| {
            errors::mcp::proxy_initialization_failed()
                .source(error)
                .error()
        })?;
        config
            .custom_headers
            .insert(HeaderName::from_static(WORKSPACE_ROOT_HEADER), value);
    }
    let transport = StreamableHttpClientTransport::with_client(
        http_client::HttpClient::new(
            reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|source| {
                    errors::mcp::proxy_initialization_failed()
                        .source(source)
                        .error()
                })?,
        ),
        config,
    );
    match tokio::time::timeout(timeout, ().serve(transport)).await {
        Ok(Ok(running)) => Ok(running),
        Ok(Err(error)) => errors::mcp::proxy_initialization_failed()
            .source(error)
            .fail(),
        Err(elapsed) => errors::mcp::proxy_initialization_failed()
            .source(elapsed)
            .fail(),
    }
}

/// Whether a forward failure means the held upstream connection is gone.
fn transport_failed(error: &ServiceError) -> bool {
    matches!(
        error,
        ServiceError::TransportClosed | ServiceError::TransportSend(_)
    )
}

/// The unchanged remote MCP error or registered local forwarding failure.
///
/// A refusal the server classified travels unchanged, refusal code and
/// all. A transport-shaped failure reaches here only after its one
/// reconnect retry also failed, and names what the operator can check;
/// every other failure names what the upstream did.
fn forwarded_error(error: ServiceError) -> Result<ErrorData, RiftError> {
    match error {
        ServiceError::McpError(data) => Ok(data),
        other => errors::mcp::forward_failed().source(other).fail(),
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
        let answered;
        let elapsed = rift_tracing::measure_elapsed!("tools/list", {
            answered = self
                .forward(list_tools_request(request), |result| match result {
                    ServerResult::ListToolsResult(result) => Some(result),
                    _ => None,
                })
                .await;
        })
        .ok()
        .map(|((), measurement)| measurement.elapsed());
        McpRequest::method(TOOLS_LIST).record(
            &MCP_CLIENT_OPERATION_DURATION,
            elapsed,
            Ending::of(&answered),
        );
        Ok(self.output.select_listing(answered?))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let measured = McpRequest::tool_call(&request.name);
        let span = rift_tracing::info_span!(
            "mcp.forward",
            component = "mcp",
            operation = "tools/call",
            request_id = rift_tracing::empty!(),
            caller_request_id = %context.id,
            tool = %request.name,
            error.type = rift_tracing::empty!()
        );
        let operation = span.clone();
        let answered;
        let elapsed = rift_tracing::measure_elapsed!("tools/call", {
            answered = span
                .instrument(async {
                    rift_tracing::debug!("tool forward started");
                    let registered: Result<_, RiftError> = match self.captured_read(&request).await
                    {
                        Err(error) => Err(error),
                        Ok(Some(answer)) => Ok(answer),
                        Ok(None) => match self.captured_search(&request).await {
                            Err(refusal) => Err(refusal),
                            Ok(Some(answer)) => Ok(answer),
                            Ok(None) => match self.call_backend(&request) {
                                Err(refusal) => Err(refusal),
                                Ok(backend) => {
                                    let request = ClientRequest::CallToolRequest(
                                        CallToolRequest::new(request),
                                    );
                                    backend
                                        .forward_registered(request, |result| match result {
                                            ServerResult::CallToolResult(result) => {
                                                Some(CallToolResponse::Complete(result))
                                            }
                                            ServerResult::InputRequiredResult(result) => {
                                                Some(CallToolResponse::InputRequired(result))
                                            }
                                            ServerResult::CreateTaskResult(result) => {
                                                Some(CallToolResponse::Task(result))
                                            }
                                            _ => None,
                                        })
                                        .await
                                }
                            },
                        },
                    };
                    if let Err(error) = &registered {
                        operation.record("error.type", error.slug().as_str());
                    }
                    let result = registered
                        .map_err(|error| error.mcp().tool_error(wire::ErrorPhase::Read))
                        .and_then(std::convert::identity);
                    rift_tracing::debug!(is_error = result.is_err(), "tool forward completed");
                    result
                })
                .await;
        })
        .ok()
        .map(|((), measurement)| measurement.elapsed());
        measured.record(
            &MCP_CLIENT_OPERATION_DURATION,
            elapsed,
            Ending::of_tool_call(&answered),
        );
        Ok(self.selected_response(answered?))
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
        let answered;
        let elapsed = rift_tracing::measure_elapsed!("resources/list", {
            answered = self
                .forward(request, |result| match result {
                    ServerResult::ListResourcesResult(result) => Some(result),
                    _ => None,
                })
                .await;
        })
        .ok()
        .map(|((), measurement)| measurement.elapsed());
        McpRequest::method(RESOURCES_LIST).record(
            &MCP_CLIENT_OPERATION_DURATION,
            elapsed,
            Ending::of(&answered),
        );
        answered
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
        let answered;
        let elapsed = rift_tracing::measure_elapsed!("resources/templates/list", {
            answered = self
                .forward(request, |result| match result {
                    ServerResult::ListResourceTemplatesResult(result) => Some(result),
                    _ => None,
                })
                .await;
        })
        .ok()
        .map(|((), measurement)| measurement.elapsed());
        McpRequest::method(RESOURCE_TEMPLATES_LIST).record(
            &MCP_CLIENT_OPERATION_DURATION,
            elapsed,
            Ending::of(&answered),
        );
        answered
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let request = ClientRequest::ReadResourceRequest(ReadResourceRequest::new(request));
        let answered;
        let elapsed = rift_tracing::measure_elapsed!("resources/read", {
            answered = self
                .forward(request, |result| match result {
                    ServerResult::ReadResourceResult(result) => {
                        Some(ReadResourceResponse::Complete(result))
                    }
                    ServerResult::InputRequiredResult(result) => {
                        Some(ReadResourceResponse::InputRequired(result))
                    }
                    _ => None,
                })
                .await;
        })
        .ok()
        .map(|((), measurement)| measurement.elapsed());
        McpRequest::method(RESOURCES_READ).record(
            &MCP_CLIENT_OPERATION_DURATION,
            elapsed,
            Ending::of(&answered),
        );
        Ok(self.selected_resource(answered?))
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
    use crate::failure::WireFailure as _;
    use std::any::TypeId;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    use rift_protocol::lock::{ProductIdentity, SERVER_TOKEN_LENGTH, ServerLock};
    use rmcp::model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, CreateTaskResult,
        InputRequiredResult, JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ResourceContents,
        ServerCapabilities, ServerPeerInfo, ServerResult, Task, TaskStatus, Tool,
    };
    use rmcp::service::{
        QuitReason, RequestContext, RoleClient, RoleServer, RunningService, serve_directly,
    };
    use rmcp::transport::DynamicTransportError;
    use rmcp::{ErrorData, ServiceError};
    use serde_json::json;
    use tokio::io::AsyncBufReadExt as _;

    use super::{
        Replacement, RepositoryAsk, RepositoryMiss, RiftProxy, ServerStanding, Upstream,
        UpstreamSlot, adopt_serving, connect_recorded, connect_upstream, connect_upstream_with,
        fallback_info, forwarded_error, identity_refusal, mirrored_info, quit_reason_result,
        reuse_current, serve_connection, server_start_failed, start_window_refusal,
        transport_failed,
    };
    use crate::election::{ServerPresence, StaleReason, claim};
    use crate::failure::McpErrorExt as _;
    use crate::output::OutputPolicy;
    use rift_core::CapturedStream;
    use rift_error::errors;
    use rift_protocol::error as wire;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn assert_operator_refusal(refusal: &ErrorData) {
        assert_eq!(refusal.code, crate::failure::RIFT_ERROR_CODE);
        let data: wire::ErrorData = serde_json::from_value(
            refusal
                .data
                .clone()
                .expect("registered refusal carries wire data"),
        )
        .expect("registered refusal carries the served error model");
        assert_eq!(data.code, wire::ErrorCode::InternalError);
        assert_eq!(data.retry, wire::RetryDirective::OperatorAction);
    }

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
            root: None,
            owner: None,
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

        let refusal = errors::mcp::start_unreachable()
            .waited(super::START_WAIT_MAX)
            .error();
        warmup.complete(Err(refusal.clone()));
        let (warmup_result, request_result) = tokio::join!(warmup.outcome(), request.outcome());
        for result in [warmup_result, request_result] {
            let received = result.expect_err("terminal connection result must be shared");
            assert_eq!(received.slug(), refusal.slug());
            assert_eq!(received.to_string(), refusal.to_string());
        }
    }

    #[test]
    fn classified_refusals_forward_unchanged() {
        let refusal = ErrorData::new(
            rmcp::model::ErrorCode(-32000),
            "the workspace configuration failed validation",
            Some(json!({"code": "configuration_invalid", "retry": "operator_action"})),
        );
        let forwarded = forwarded_error(ServiceError::McpError(refusal.clone()))
            .expect("remote MCP errors remain unchanged");
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
            let error = forwarded_error(failure).expect_err("transport failure is registered");
            assert_eq!(error.slug().as_str(), "rift.mcp.forward_failed");
            let forwarded = error.mcp().tool_error(wire::ErrorPhase::Read);
            assert_operator_refusal(&forwarded);
            assert!(
                forwarded.message.contains("rift server start --foreground"),
                "{}",
                forwarded.message
            );
            assert!(
                forwarded.message.contains("forwarded request"),
                "{}",
                forwarded.message
            );
        }
    }

    #[test]
    fn other_service_failures_name_the_upstream_failure() {
        let error = forwarded_error(ServiceError::UnexpectedResponse)
            .expect_err("unexpected response is registered");
        assert_eq!(error.slug().as_str(), "rift.mcp.forward_failed");
        let forwarded = error.mcp().tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&forwarded);
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
        let refused = errors::mcp::proxy_initialization_failed()
            .source(rmcp::service::ClientInitializeError::Cancelled)
            .error();
        assert_eq!(
            refused.slug().as_str(),
            "rift.mcp.proxy_initialization_failed"
        );
        assert!(refused.to_string().contains("Cancelled"));
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
        let refusal = identity_refusal(&expected, &recorded_lock(12_345))
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&refusal);
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
            .expect_err("a second server of another identity must be refused")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
            .expect_err("a server that refused the stop keeps serving")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
            .ok_or("an older server that still serves must be refused")?
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
            .expect_err("a newer server must be refused")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let refusal = start_window_refusal(false)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&refusal);
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
        let refusal = server_start_failed(&CapturedStream::default())
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let refusal = server_start_failed(&capture)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let refusal = server_start_failed(&capture)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
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
        .expect_err("an answer of another kind must refuse")
        .mcp()
        .tool_error(wire::ErrorPhase::Read);
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
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("rift.toml"),
            "[server]\nreadiness_timeout = \"1s\"\nworker_queue_timeout = \"1s\"\n",
        )?;
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
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
        // A silent server answers nothing, so the forward's `answer` never runs.
        let forward = rift_tracing::traced!(component = "mcp", operation = "tools/call", async {
            let request = super::list_tools_request(None);
            proxy
                .forward(request, |_answer| None::<ListToolsResult>)
                .await
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
        drop(recorder);
        let records = drain.queued_records();
        let table = records
            .iter()
            .find(|record| record.message() == "operations in flight")
            .ok_or("the budget's end published the operations in flight")?;
        let table: serde_json::Value = serde_json::from_str(table.fields())?;
        assert_eq!(table["reason"], "forward budget", "{table}");
        let listed: serde_json::Value =
            serde_json::from_str(table["operations"].as_str().ok_or("operations")?)?;
        assert!(
            listed
                .as_array()
                .into_iter()
                .flatten()
                .any(|entry| entry["operation"] == "tools/call"),
            "{table}"
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
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
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
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
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
            .expect_err("a holder that never publishes must exhaust the start window")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&refusal);
        assert!(
            refusal
                .message
                .contains("no rift server answered for this workspace within 30s"),
            "{}",
            refusal.message
        );
        Ok(())
    }

    /// A repository server holds the workspace's election and publishes no
    /// document there, which the held election below stands in for. Its first
    /// ask misses through a transient arm, and the start window asks again on
    /// the next poll round, which connects. The window's close logs once,
    /// naming the rounds it ran and the outcome.
    #[tokio::test(start_paused = true)]
    async fn a_transient_repository_miss_is_asked_again_inside_the_start_window() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let (running, _kept_alive) = direct_upstream();
        let mut answers = vec![
            RepositoryAsk::Connected(running),
            RepositoryAsk::Transient(RepositoryMiss::Unanswered {
                pid: 4_242,
                port: 0,
                elapsed_ms: 5_000,
                detail: "connect timed out after 5s".to_owned(),
            }),
        ];
        let mut asked = 0_u32;
        let connected = connect_upstream_with(directory.path(), &test_identity(), || {
            asked += 1;
            std::future::ready(
                answers
                    .pop()
                    .unwrap_or(RepositoryAsk::Terminal(RepositoryMiss::NotRepository)),
            )
        })
        .await;
        assert!(connected.is_ok(), "{connected:?}");
        assert_eq!(asked, 2, "the second ask connects");
        drop(recorder);
        let records = drain.queued_records();
        let closed = records
            .iter()
            .filter(|record| record.message() == "start window closed with a server that answers")
            .collect::<Vec<_>>();
        assert_eq!(closed.len(), 1, "the window's close logs once: {records:?}");
        assert_eq!(closed[0].level(), "info");
        let fields: serde_json::Value = serde_json::from_str(closed[0].fields())?;
        assert_eq!(fields["rounds"], "1");
        assert_eq!(fields["outcome"], "ok");
        assert_eq!(fields["presence"], "Starting");
        assert!(fields["elapsed_ms"].is_string(), "{fields}");
        Ok(())
    }

    /// A terminal repository miss ends the asking, and the workspace's own
    /// election decides the start window: a holder that publishes no document
    /// exhausts it.
    #[tokio::test(start_paused = true)]
    async fn a_terminal_repository_miss_is_asked_once() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let mut asked = 0_u32;
        let refusal = connect_upstream_with(directory.path(), &test_identity(), || {
            asked += 1;
            std::future::ready(RepositoryAsk::Terminal(RepositoryMiss::NotAdopted {
                pid: 4_242,
                settings_match: false,
                identity_adopted: true,
            }))
        })
        .await
        .expect_err("a holder that never publishes must exhaust the start window")
        .mcp()
        .tool_error(wire::ErrorPhase::Read);
        assert_eq!(asked, 1, "a terminal miss is asked once");
        assert_operator_refusal(&refusal);
        assert!(
            refusal
                .message
                .contains("no rift server answered for this workspace within 30s"),
            "{}",
            refusal.message
        );
        Ok(())
    }

    /// A holder of the election that publishes nothing but writes the
    /// index database's or the metrics database's write-ahead log alone during
    /// the start window is still building: the connect refuses with a refusal
    /// the caller resends, naming the build.
    #[tokio::test(start_paused = true)]
    async fn a_building_holder_refuses_with_a_retryable_refusal() -> TestResult {
        for written_log in ["index-wal", "metrics-wal"] {
            building_holder_refuses_after_writing(written_log).await?;
        }
        Ok(())
    }

    /// One start window whose election holder writes `written_log` below `.rift`.
    async fn building_holder_refuses_after_writing(written_log: &str) -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let log = directory.path().join(".rift").join(written_log);
        let writing = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::fs::write(&log, b"a commit the building server wrote")
        };
        let identity = test_identity();
        let (refusal, written) =
            tokio::join!(connect_upstream(directory.path(), &identity), writing);
        written?;
        let refusal = refusal
            .expect_err("a holder that never publishes exhausts the window")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let building = super::start_window_refusal(true)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
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
        let wedged = super::start_window_refusal(false)
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&wedged);
        assert!(
            wedged.message.contains("operator action on this host"),
            "{}",
            wedged.message
        );
    }

    /// Database activity reads each file's length and modification time, and
    /// moves when any one of the four files is written alone.
    #[test]
    fn database_activity_moves_when_any_database_file_is_written() -> TestResult {
        let directory = tempfile::tempdir()?;
        let before = super::DatabaseActivity::read(directory.path());
        assert_eq!(before.files, [None; 4]);
        let state = directory.path().join(".rift");
        std::fs::create_dir_all(&state)?;
        let mut previous = before;
        for file in ["index", "index-wal", "metrics", "metrics-wal"] {
            std::fs::write(state.join(file), b"a commit")?;
            let written = super::DatabaseActivity::read(directory.path());
            assert_ne!(
                written, previous,
                "writing {file} alone must move the reading"
            );
            previous = written;
        }
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
            .expect_err("a workspace nobody serves must exhaust the start window")
            .mcp()
            .tool_error(wire::ErrorPhase::Read);
        assert_operator_refusal(&refusal);
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
            failure.slug().as_str() == "rift.mcp.proxy_initialization_failed"
                && failure.to_string().contains("deadline has elapsed"),
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
            RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default()),
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
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("rift=info,rift_mcp=debug,rift_server=debug,rift_index=info")
            .install()?;
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
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
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
            .await?;
        assert_eq!(invalid.is_error, Some(true), "{invalid:?}");
        assert!(
            invalid
                .content
                .first()
                .and_then(|block| block.as_text())
                .is_some_and(|block| block.text.contains("invalid_request")),
            "invalid paths retain the refusal code: {invalid:?}"
        );
        assert_tool_diagnostics(&drain.queued_records(), &request_id)?;
        client.cancel().await?;
        forwarding.cancel().await?;
        serving.cancel().await?;
        Ok(())
    }

    /// Whether `record` was emitted inside the `span` that served `request_id` for the
    /// internal `search_candidates` tool: its `root_span` member names that span and
    /// carries both fields.
    ///
    /// The member is the span the record was emitted in, read from the subscriber's span
    /// stack when the event fired. A record a background lane's task emits while the
    /// request waits carries the lane's root span or none, never this one.
    fn emitted_in(record: &rift_tracing::LogRecord, span: &str, request_id: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(record.fields()).is_ok_and(|fields| {
            let root = &fields["root_span"];
            root["name"] == span
                && root["fields"]["request_id"] == request_id
                && root["fields"]["tool"] == "search_candidates"
        })
    }

    /// The `request_id` of the `mcp.forward` span the record of `message` was emitted in
    /// for the `search` tool, once the forward recorded it.
    fn forward_request_id(records: &[rift_tracing::LogRecord], message: &str) -> Option<String> {
        records.iter().find_map(|record| {
            let fields = serde_json::from_str::<serde_json::Value>(record.fields()).ok()?;
            let root = &fields["root_span"];
            (record.message() == message
                && root["name"] == "mcp.forward"
                && root["fields"]["tool"] == "search")
                .then(|| root["fields"]["request_id"].as_str().map(str::to_owned))
                .flatten()
        })
    }

    /// The proxy's forward of the caller's `caller_id` and the server's request carry one
    /// `request_id`, the id the server's `mcp.request` span carries, and the caller's id
    /// is not it; `is_error=false` maps to the `is_error` field `"false"`.
    fn assert_tool_diagnostics(records: &[rift_tracing::LogRecord], caller_id: &str) -> TestResult {
        let server_id = forward_request_id(records, "forwarded request awaiting response")
            .ok_or("the proxy records the active forward under the server's request id")?;
        assert_ne!(
            server_id, caller_id,
            "the forwarding span carries the id the server sees, not the caller's"
        );
        let forwarded = records
            .iter()
            .find(|record| record.message() == "forwarded request awaiting response")
            .ok_or("the proxy records its pending request")?;
        let fields: serde_json::Value = serde_json::from_str(forwarded.fields())?;
        assert_eq!(
            fields["root_span"]["fields"]["caller_request_id"],
            caller_id
        );
        assert_forward_joins_server_request(records, &server_id)?;
        for event in [
            "tool request started",
            "worker admission started",
            "worker admitted",
            "request capture compared with publication",
            "tool request completed",
        ] {
            assert!(
                records.iter().any(|record| {
                    record.message() == event && emitted_in(record, "mcp.request", &server_id)
                }),
                "the server event retains the forwarded request id: {event}\n{records:#?}"
            );
        }
        let completed = |record: &rift_tracing::LogRecord, is_error: &str| {
            record.message() == "tool request completed"
                && serde_json::from_str::<serde_json::Value>(record.fields())
                    .is_ok_and(|fields| fields["is_error"] == is_error)
        };
        assert!(
            records.iter().any(|record| {
                completed(record, "false") && emitted_in(record, "mcp.request", &server_id)
            }),
            "the forwarded search completes without an error: {records:#?}"
        );
        assert!(records.iter().any(|record| completed(record, "true")));
        assert!(
            !records.iter().any(|record| {
                [
                    record.target(),
                    record.component(),
                    record.operation(),
                    record.message(),
                    record.fields(),
                ]
                .iter()
                .any(|text| text.contains("zzdiagnosticsecret"))
            }),
            "request arguments stay out of diagnostics: {records:#?}"
        );
        Ok(())
    }

    /// The root span's `request_id` is what a printed line and the failure window group
    /// records of one request by. The `mcp.forward` records after the send, and its close
    /// record, carry `server_id`, the `request_id` of the server's `mcp.request` span, so
    /// the proxy's lines and the server's join one request group.
    fn assert_forward_joins_server_request(
        records: &[rift_tracing::LogRecord],
        server_id: &str,
    ) -> TestResult {
        assert_eq!(
            forward_request_id(records, "forwarded request completed").as_deref(),
            Some(server_id),
            "the forward's record after the answer carries the server's request id"
        );
        let forward = records
            .iter()
            .filter_map(|record| {
                serde_json::from_str::<serde_json::Value>(record.fields())
                    .ok()
                    .filter(|fields| {
                        record.message() == "mcp.forward" && fields["span"] == "closed"
                    })
            })
            .find(|fields| fields["tool"] == "search")
            .ok_or_else(|| format!("the proxy closes the forwarding span: {records:#?}"))?;
        assert_eq!(
            forward["request_id"], server_id,
            "the forwarding span closes under the server's request id: {forward}"
        );
        assert!(
            records.iter().any(|record| {
                record.message() == "tool request completed"
                    && emitted_in(record, "mcp.request", server_id)
            }),
            "the server's request span carries the id the forwarding span carries: \
             {records:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_client_ends_the_connection_cleanly() {
        use rmcp::ServiceExt as _;
        let directory = tempfile::tempdir().expect("temporary directory");
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(serve_connection(
            RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default()),
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

    /// An upstream that lists two tools over two pages, each with an output
    /// schema, and answers a call with structured content, as the shared
    /// server does.
    #[derive(Clone)]
    struct StructuredUpstream {
        root: String,
    }

    const SECOND_PAGE_CURSOR: &str = "second-page";
    /// A resource the upstream answers with compact text and JSON.
    const TWO_CONTENT_URI: &str = "rift://map";
    /// A resource the upstream answers with JSON alone.
    const JSON_ONLY_URI: &str = "rift://json-only";

    fn upstream_tool(name: &'static str) -> Tool {
        let object = |value: serde_json::Value| -> JsonObject {
            serde_json::from_value(value).expect("test schema must be a JSON object")
        };
        Tool::new(name, "an upstream tool", object(json!({"type": "object"})))
            .with_raw_output_schema(object(json!({"type": "object"})).into())
    }

    impl rmcp::ServerHandler for StructuredUpstream {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                ServerCapabilities::builder()
                    .enable_tools()
                    .enable_resources()
                    .build(),
            )
        }

        fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = Result<ReadResourceResponse, ErrorData>> {
            let uri = request.uri.as_str();
            std::future::ready(match uri {
                TWO_CONTENT_URI => Ok(ReadResourceResult::new(vec![
                    ResourceContents::text("map 3f9a1c2e", uri).with_mime_type("text/plain"),
                    ResourceContents::text("{\"revision\":\"3f9a1c2e\"}", uri)
                        .with_mime_type("application/json"),
                ])
                .into()),
                JSON_ONLY_URI => Ok(ReadResourceResult::new(vec![
                    ResourceContents::text("{}", uri).with_mime_type("application/json"),
                ])
                .into()),
                other => Err(ErrorData::resource_not_found(
                    format!("no resource is published at {other:?}"),
                    None,
                )),
            })
        }

        fn list_tools(
            &self,
            request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> {
            let second = request
                .and_then(|page| page.cursor)
                .is_some_and(|cursor| cursor == SECOND_PAGE_CURSOR);
            if second {
                return std::future::ready(Ok(ListToolsResult::with_all_items(vec![
                    upstream_tool("nodes"),
                ])));
            }
            let mut first = ListToolsResult::with_all_items(vec![upstream_tool("search")]);
            first.next_cursor = Some(SECOND_PAGE_CURSOR.to_owned());
            std::future::ready(Ok(first))
        }

        fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> {
            std::future::ready(match request.name.as_ref() {
                "search_candidates" => {
                    let response = crate::server::candidates::SearchCandidateResponse {
                        root: self.root.clone(),
                        owner: rift_protocol::identity::SymbolOwner::Local,
                        view: rift_protocol::symbol_read::CapturedView {
                            id: rift_protocol::symbol_read::CapturedViewId::parse("claims.primary")
                                .expect("a captured fixture view"),
                            expires_at: "2099-01-01T00:00:00Z".to_owned(),
                        },
                        inputs: rift_server::SearchCandidateInputs::default(),
                        ranking: rift_protocol::configuration::RankingConfiguration::default(),
                        results_max: 100,
                        global: None,
                    };
                    Ok(CallToolResult::structured(
                        serde_json::to_value(response).expect("typed candidate response"),
                    )
                    .into())
                }
                "failing" => Ok(CallToolResult::structured_error(json!({"code": "failed"})).into()),
                other => Err(ErrorData::invalid_params(
                    format!("unknown tool {other}"),
                    None,
                )),
            })
        }
    }

    /// A downstream client talking to a proxy under `output`, whose upstream
    /// is [`StructuredUpstream`]. The fields keep the connection tasks alive.
    struct ProxiedClient {
        client: RunningService<RoleClient, ()>,
        _upstream: RunningService<RoleServer, StructuredUpstream>,
        _connection: tokio::task::JoinHandle<Result<(), rift_error::RiftError>>,
        _directory: tempfile::TempDir,
    }

    async fn proxied_client(output: OutputPolicy) -> TestResult<ProxiedClient> {
        use rmcp::ServiceExt as _;
        let directory = tempfile::tempdir()?;
        let proxy = RiftProxy::new(directory.path(), test_identity(), output);
        let (upstream_client_half, upstream_server_half) = tokio::io::duplex(64 * 1024);
        // Each rmcp handshake future is about 4,900 bytes on aarch64; boxed, the three of
        // them stay out of every test that awaits this fixture.
        let (running, upstream) = tokio::join!(
            Box::pin(().serve(upstream_client_half)),
            Box::pin(
                StructuredUpstream {
                    root: directory
                        .path()
                        .canonicalize()?
                        .to_string_lossy()
                        .into_owned(),
                }
                .serve(upstream_server_half),
            )
        );
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running: running?,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let (proxy_half, client_half) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(serve_connection(proxy, proxy_half));
        Ok(ProxiedClient {
            client: Box::pin(().serve(client_half)).await?,
            _upstream: upstream?,
            _connection: connection,
            _directory: directory,
        })
    }

    #[tokio::test]
    async fn text_output_strips_schemas_and_structured_content_through_the_handlers() -> TestResult
    {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let tools = proxied.client.list_all_tools().await?;
        let names: Vec<_> = tools.iter().map(|tool| tool.name.as_ref()).collect();
        assert_eq!(names, ["search", "nodes"], "every page must be listed");
        assert!(
            tools.iter().all(|tool| tool.output_schema.is_none()),
            "text output must list no output schema on any page: {tools:?}"
        );

        let result = proxied
            .client
            .call_tool(CallToolRequestParams::new("search"))
            .await?;
        assert_eq!(result.structured_content, None);
        assert_eq!(result.content.len(), 1, "content must stay: {result:?}");
        assert_eq!(result.is_error, Some(false));
        Ok(())
    }

    /// Every request the proxy forwards lands in `mcp.client.operation.duration`: a tool
    /// answered with `isError` as `tool_error`, an upstream JSON-RPC refusal under its code.
    #[tokio::test]
    async fn each_forwarded_request_records_its_client_operation_duration() -> TestResult {
        use crate::metrics::tests::recorded;

        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let proxied = proxied_client(OutputPolicy::Text).await?;
        proxied.client.list_all_tools().await?;
        proxied
            .client
            .call_tool(CallToolRequestParams::new("search"))
            .await?;
        let failing = proxied
            .client
            .call_tool(CallToolRequestParams::new("failing"))
            .await?;
        assert_eq!(failing.is_error, Some(true));
        let refused = proxied
            .client
            .call_tool(CallToolRequestParams::new("unknown"))
            .await;
        assert!(refused.is_err(), "the upstream refuses an unknown tool");
        let Err(ServiceError::McpError(ref error)) = refused else {
            return Err("an upstream refusal retains its JSON-RPC error".into());
        };
        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "unknown tool unknown");
        assert!(error.data.is_none());
        proxied
            .client
            .read_resource(ReadResourceRequestParams::new(JSON_ONLY_URI))
            .await?;

        let snapshot = recorder.metrics();
        let name = "mcp.client.operation.duration";
        assert_eq!(
            recorded(&snapshot, name, &[("mcp.method.name", "tools/list")]),
            2,
            "one per listed page: {snapshot:?}"
        );
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "tools/call"),
                    ("gen_ai.tool.name", "search")
                ],
            ),
            1
        );
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "tools/call"),
                    ("error.type", "tool_error")
                ],
            ),
            1,
            "a tool outside the served tools names none"
        );
        assert_eq!(
            recorded(
                &snapshot,
                name,
                &[
                    ("mcp.method.name", "tools/call"),
                    ("error.type", "-32602"),
                    ("rpc.response.status_code", "-32602"),
                ],
            ),
            1
        );
        assert_eq!(
            recorded(&snapshot, name, &[("mcp.method.name", "resources/read")]),
            1
        );
        Ok(())
    }

    /// The media type of every content one read returned.
    fn media_types(result: &ReadResourceResult) -> Vec<Option<&str>> {
        result
            .contents
            .iter()
            .map(|content| match content {
                ResourceContents::TextResourceContents { mime_type, .. } => mime_type.as_deref(),
                other => unreachable!("the upstream answers text, not {other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn text_output_keeps_the_text_content_of_a_resource_read() -> TestResult {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let result = proxied
            .client
            .read_resource(ReadResourceRequestParams::new(TWO_CONTENT_URI))
            .await?;
        assert_eq!(media_types(&result), [Some("text/plain")]);
        match &result.contents[..] {
            [ResourceContents::TextResourceContents { uri, text, .. }] => {
                assert_eq!(uri, TWO_CONTENT_URI);
                assert_eq!(text, "map 3f9a1c2e");
            }
            other => panic!("one text content must stay, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn text_output_keeps_a_resource_that_holds_json_alone() -> TestResult {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let result = proxied
            .client
            .read_resource(ReadResourceRequestParams::new(JSON_ONLY_URI))
            .await?;
        assert_eq!(media_types(&result), [Some("application/json")]);
        Ok(())
    }

    #[tokio::test]
    async fn a_resource_the_upstream_does_not_publish_stays_refused_through_the_proxy() -> TestResult
    {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let refused = proxied
            .client
            .read_resource(ReadResourceRequestParams::new("rift://unpublished"))
            .await
            .expect_err("an unpublished resource must be refused");
        assert!(
            format!("{refused:?}").contains("no resource is published at"),
            "the upstream refusal must reach the client: {refused:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_tool_the_upstream_does_not_serve_stays_refused_through_the_proxy() -> TestResult {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let refused = proxied
            .client
            .call_tool(CallToolRequestParams::new("unserved"))
            .await
            .expect_err("an unserved tool must be refused");
        assert!(
            format!("{refused:?}").contains("unknown tool unserved"),
            "the upstream refusal must reach the client: {refused:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn named_projects_are_refused_before_primary_forwarding() -> TestResult {
        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("rift_mcp=debug")
            .install()?;
        let proxied = proxied_client(OutputPolicy::All).await?;
        let identity = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::NamedLocal {
                name: "cloud".to_owned(),
            },
            rift_protocol::read::Language::from_identity_segment("rust")
                .expect("a canonical fixture language"),
            vec!["beacon".to_owned()],
        )
        .expect("a canonical named fixture identity")
        .wire_identity();
        for (tool, arguments) in [
            ("get_symbol", json!({"id": identity})),
            ("search", json!({"query": "beacon", "scope": "local@cloud"})),
            ("search", json!({"query": "beacon", "scope": "local@Cloud"})),
        ] {
            let request =
                CallToolRequestParams::new(tool).with_arguments(serde_json::from_value(arguments)?);
            let refused = proxied
                .client
                .call_tool(request)
                .await
                .expect_err("an unknown named project never reaches the primary upstream");
            let ServiceError::McpError(error) = refused else {
                return Err("a named refusal retains its JSON-RPC error".into());
            };
            let error: wire::ErrorData =
                serde_json::from_value(error.data.ok_or("a refusal retains its wire payload")?)?;
            assert_eq!(error.code, wire::ErrorCode::InvalidRequest);
            assert_eq!(error.retry, wire::RetryDirective::Never);
        }
        let primary =
            proxied
                .client
                .call_tool(CallToolRequestParams::new("search").with_arguments(
                    serde_json::from_value(json!({"query": "beacon", "scope": "local"}))?,
                ))
                .await?;
        let primary: rift_protocol::search::SearchResult = serde_json::from_value(
            primary
                .structured_content
                .ok_or("primary search returns structured content")?,
        )?;
        assert!(primary.results.is_empty());
        assert_eq!(primary.pagination.page_index, 0);
        assert_eq!(primary.pagination.total_pages, 0);
        let view = primary
            .view
            .ok_or("primary search retains its captured view")?;
        assert_eq!(view.id.as_str(), "claims.primary");
        assert_eq!(view.expires_at, "2099-01-01T00:00:00Z");
        let records = drain.queued_records();
        let completed = records
            .iter()
            .filter(|record| record.message() == "tool forward completed")
            .map(|record| serde_json::from_str::<serde_json::Value>(record.fields()))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(completed.len(), 4);
        for (fields, tool) in completed[..3]
            .iter()
            .zip(["get_symbol", "search", "search"])
        {
            let operation = &fields["root_span"];
            assert_eq!(operation["name"], "mcp.forward");
            assert_eq!(operation["fields"]["operation"], "tools/call");
            assert_eq!(operation["fields"]["tool"], tool);
            assert!(!operation["fields"]["caller_request_id"].is_null());
            assert_eq!(
                operation["fields"]["error.type"],
                errors::mcp::parameter_invalid::SLUG.as_str()
            );
            assert_eq!(fields["is_error"], "true");
        }
        assert!(completed[3]["root_span"]["fields"]["error.type"].is_null());
        assert_eq!(completed[3]["is_error"], "false");
        Ok(())
    }

    fn named_proxy(root: &std::path::Path, extra_root: &std::path::Path) -> TestResult<RiftProxy> {
        let mut saved = rift_protocol::configuration::McpConfiguration::default();
        saved.extra.insert(
            "cloud".to_owned(),
            extra_root.to_string_lossy().into_owned(),
        );
        let extra = super::extra::Registrations::accept(root, &saved, &[], None)?;
        Ok(RiftProxy::with_configuration(
            root,
            test_identity(),
            OutputPolicy::All,
            &rift_protocol::configuration::ServerConfiguration::default(),
            extra,
        ))
    }

    #[tokio::test]
    async fn named_backend_retains_root_owner_and_independent_connection_state() -> TestResult {
        let primary = tempfile::tempdir()?;
        let extra = tempfile::tempdir()?;
        let proxy = named_proxy(primary.path(), extra.path())?;
        let request = CallToolRequestParams::new("search")
            .with_arguments(serde_json::from_value(json!({"scope": "local@cloud"}))?);
        let backend = proxy.call_backend(&request)?;
        assert_eq!(
            backend.root.as_ref(),
            extra.path().canonicalize()?.as_path()
        );
        assert_eq!(backend.owner, rift_protocol::identity::SymbolOwner::Local);
        assert!(!Arc::ptr_eq(&proxy.upstream, &backend.upstream));
        assert!(!Arc::ptr_eq(&proxy.advertised, &backend.advertised));
        assert_eq!(proxy.forward_budget, backend.forward_budget);
        assert_eq!(proxy.upstream.lock().await.generation_next, 0);
        assert_eq!(backend.upstream.lock().await.generation_next, 0);
        assert!(!primary.path().join(".rift").exists());
        assert!(!extra.path().join(".rift").exists());
        Ok(())
    }

    fn assert_named_get(
        first_value: &serde_json::Value,
        arguments: &serde_json::Value,
        proxy: &RiftProxy,
    ) {
        assert_eq!(first_value["symbol"]["id"], arguments["id"]);
        assert_eq!(first_value["declarations"][0]["source"], "fn load() {}");
        assert_eq!(first_value["declarations"][0]["path"], "lib.rs");
        assert_eq!(
            proxy.backends["cloud"].owner,
            rift_protocol::identity::SymbolOwner::Local
        );
    }

    fn assert_named_walk(initial: &serde_json::Value) {
        assert_eq!(initial["results"][0]["hit"]["symbol"]["name"], "zulu");
        assert_eq!(initial["results"][1]["hit"]["symbol"]["name"], "alpha");
        for (index, name) in ["zulu", "alpha"].into_iter().enumerate() {
            assert_eq!(
                initial["results"][index]["hit"]["symbol"]["id"],
                format!("rift://symbol/local@cloud/rust/app/{name}")
            );
            assert_eq!(
                initial["results"][index]["source"],
                format!("fn {name}() {{}}")
            );
            assert_eq!(initial["results"][index]["path"], "lib.rs");
            assert_eq!(initial["results"][index]["view"], initial["view"]);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn named_initial_get_keeps_typed_ids_and_original_view_after_extra_root_retargets()
    -> TestResult {
        use std::os::unix::fs::symlink;
        use tokio::io::AsyncWriteExt as _;
        let primary = tempfile::tempdir()?;
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let registration_path = primary.path().join("extra");
        symlink(first.path(), &registration_path)?;
        let mut proxy = named_proxy(primary.path(), &registration_path)?;
        let backend = proxy.backends.get("cloud").ok_or("accepted backend")?;
        let (running, upstream) = direct_upstream();
        {
            let mut slot = backend.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let expires_at = jiff::Timestamp::now()
            .checked_add(std::time::Duration::from_secs(600))?
            .to_string();
        let answering = tokio::spawn(async move {
            let (reading, mut writing) = tokio::io::split(upstream);
            let mut lines = tokio::io::BufReader::new(reading).lines();
            let mut count = 0;
            while count < 2 {
                let line = lines
                    .next_line()
                    .await?
                    .ok_or(std::io::ErrorKind::UnexpectedEof)?;
                let request: serde_json::Value = serde_json::from_str(&line)?;
                assert_eq!(
                    request["params"]["arguments"]["id"],
                    "rift://symbol/local/rust/app/load"
                );
                if count == 0 {
                    assert_eq!(request["params"]["arguments"]["rev"], "HEAD");
                } else {
                    assert!(request["params"]["arguments"].get("rev").is_none());
                    assert_eq!(request["params"]["arguments"]["view"], "claims.original");
                }
                let answer = json!({"content":[],"structuredContent":{
                    "outcome":"found", "symbol":{"id":"rift://symbol/local/rust/app/load", "language":"rust", "name":"load", "kind":"function"},
                    "view":{"id":"claims.original","expires_at":expires_at},
                    "declarations":[{"origin":{"location":"project","source_kind":"authored"},"path":"lib.rs","range":{"start":0,"end":12},"line":1,"source":"fn load() {}", "source_complete":true,"signature_indices":[],"type_indices":[],"documentation_indices":[]}]
                }});
                let response = json!({"jsonrpc":"2.0","id":request["id"],"result":answer});
                writing
                    .write_all(format!("{response}\n").as_bytes())
                    .await?;
                count += 1;
            }
            Ok::<_, std::io::Error>(count)
        });
        let arguments = json!({"id":"rift://symbol/local@cloud/rust/app/load","rev":"HEAD"});
        let request = CallToolRequestParams::new("get_symbol")
            .with_arguments(serde_json::from_value(arguments.clone())?);
        let first_response = proxy
            .captured_read(&request)
            .await?
            .ok_or("captured request")??;
        let CallToolResponse::Complete(first_response) = first_response else {
            return Err("complete result".into());
        };
        let first_value = first_response.structured_content.ok_or("typed result")?;
        assert_named_get(&first_value, &arguments, &proxy);
        std::fs::remove_file(&registration_path)?;
        symlink(second.path(), &registration_path)?;
        let mut held = arguments;
        held.as_object_mut().ok_or("request object")?.remove("rev");
        held["view"] = json!("claims.original");
        // The served revision is a backend request selector, not a rewritten identity.
        let initial_revision = first_value["view"].clone();
        let held_request = CallToolRequestParams::new("get_symbol")
            .with_arguments(serde_json::from_value(held.clone())?);
        // Keep the same accepted startup root even after the configured symlink changes.
        assert_eq!(
            proxy.backends["cloud"].accepted_root.as_deref(),
            Some(first.path().canonicalize()?.as_path())
        );
        let held_response = proxy
            .captured_read(&held_request)
            .await?
            .ok_or("held request")??;
        let CallToolResponse::Complete(held_response) = held_response else {
            return Err("complete held result".into());
        };
        assert_eq!(
            held_response
                .structured_content
                .as_ref()
                .ok_or("held value")?["view"],
            initial_revision
        );
        assert_eq!(
            held_response
                .structured_content
                .as_ref()
                .ok_or("held value")?["symbol"]["id"],
            held["id"]
        );
        assert_eq!(answering.await??, 2);
        proxy.backends = Arc::new(std::collections::BTreeMap::new());
        assert!(
            proxy.captured_read(&held_request).await.is_err(),
            "removed registration cannot retarget an issued view"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn named_initial_walk_keeps_order_source_and_view_after_extra_root_retargets()
    -> TestResult {
        use std::os::unix::fs::symlink;
        use tokio::io::AsyncWriteExt as _;
        let primary = tempfile::tempdir()?;
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let registration_path = primary.path().join("walk");
        symlink(first.path(), &registration_path)?;
        let mut proxy = named_proxy(primary.path(), &registration_path)?;
        let backend = proxy.backends.get("cloud").ok_or("accepted backend")?;
        let (running, upstream) = direct_upstream();
        {
            let mut slot = backend.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let expires_at = jiff::Timestamp::now()
            .checked_add(std::time::Duration::from_secs(600))?
            .to_string();
        let answering = tokio::spawn(async move {
            let (reading, mut writing) = tokio::io::split(upstream);
            let mut lines = tokio::io::BufReader::new(reading).lines();
            for count in 0..2 {
                let line = lines
                    .next_line()
                    .await?
                    .ok_or(std::io::ErrorKind::UnexpectedEof)?;
                let request: serde_json::Value = serde_json::from_str(&line)?;
                let arguments = &request["params"]["arguments"];
                assert_eq!(
                    arguments["traversal"]["seed"],
                    "rift://symbol/local/rust/app/load"
                );
                assert_eq!(arguments["scope"], "all");
                assert_eq!(arguments["limit"], 2);
                assert_eq!(arguments["page_index"], 0);
                if count == 0 {
                    assert!(arguments.get("view").is_none());
                } else {
                    assert_eq!(arguments["view"], "claims.walk");
                }
                let view = json!({"id":"claims.walk", "expires_at":expires_at});
                let hits: Vec<_> = ["zulu", "alpha"]
                    .into_iter()
                    .map(|name| {
                        json!({"view":view, "hit":{"target":"symbol", "symbol":{
                        "id":format!("rift://symbol/local/rust/app/{name}"),
                        "language":"rust", "name":name, "kind":"function"
                    }}, "source":format!("fn {name}() {{}}"), "path":"lib.rs",
                    "range":{"start":0,"end":12}, "line":1})
                    })
                    .collect();
                let answer = json!({"content":[], "structuredContent":{
                    "view":view, "results":hits,
                    "pagination":{"page_index":0, "total_pages":1}
                }});
                let response = json!({"jsonrpc":"2.0", "id":request["id"], "result":answer});
                writing
                    .write_all(format!("{response}\n").as_bytes())
                    .await?;
            }
            Ok::<_, std::io::Error>(2)
        });
        let mut arguments = json!({"scope":"all", "limit":2,
        "include":["source"], "traversal":{
            "seed":"rift://symbol/local@cloud/rust/app/load", "direction":"incoming"
        }});
        let initial = CallToolRequestParams::new("search")
            .with_arguments(serde_json::from_value(arguments.clone())?);
        let CallToolResponse::Complete(initial) = proxy
            .captured_read(&initial)
            .await?
            .ok_or("captured walk")??
        else {
            return Err("complete walk".into());
        };
        let initial = initial.structured_content.ok_or("typed walk")?;
        assert_named_walk(&initial);
        assert_eq!(proxy.upstream.lock().await.generation_next, 0);
        std::fs::remove_file(&registration_path)?;
        symlink(second.path(), &registration_path)?;
        arguments["view"] = json!("claims.walk");
        let held =
            CallToolRequestParams::new("search").with_arguments(serde_json::from_value(arguments)?);
        let CallToolResponse::Complete(continued) =
            proxy.captured_read(&held).await?.ok_or("held walk")??
        else {
            return Err("complete held walk".into());
        };
        assert_eq!(continued.structured_content.ok_or("held result")?, initial);
        assert_eq!(
            proxy.backends["cloud"].accepted_root.as_deref(),
            Some(first.path().canonicalize()?.as_path())
        );
        assert_eq!(answering.await??, 2);
        proxy.backends = Arc::new(std::collections::BTreeMap::new());
        assert!(proxy.captured_read(&held).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn unknown_or_conflicting_named_scope_never_leases_a_backend() -> TestResult {
        let primary = tempfile::tempdir()?;
        let extra = tempfile::tempdir()?;
        let proxy = named_proxy(primary.path(), extra.path())?;
        let id = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            rift_protocol::read::Language::from_identity_segment("rust").expect("fixture language"),
            vec!["beacon".to_owned()],
        )?
        .wire_identity();
        for arguments in [
            json!({"scope":"local@unknown"}),
            json!({"scope":"local@cloud", "id":id}),
        ] {
            let request = CallToolRequestParams::new("search")
                .with_arguments(serde_json::from_value(arguments)?);
            let error = proxy
                .call_backend(&request)
                .expect_err("unaccepted owner refuses before connection");
            assert_eq!(error.slug(), errors::mcp::parameter_invalid::SLUG);
        }
        assert_eq!(proxy.backends.len(), 1);
        assert_eq!(proxy.upstream.lock().await.generation_next, 0);
        assert_eq!(
            proxy.backends["cloud"]
                .upstream
                .lock()
                .await
                .generation_next,
            0
        );
        assert!(!primary.path().join(".rift").exists());
        assert!(!extra.path().join(".rift").exists());
        Ok(())
    }

    #[tokio::test]
    async fn named_adoption_refuses_wrong_root_or_owner_before_connection() -> TestResult {
        let root = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let owner = rift_protocol::identity::SymbolOwner::NamedLocal {
            name: "cloud".to_owned(),
        };
        for (recorded_root, recorded_owner) in [
            (
                root.path().canonicalize()?,
                rift_protocol::identity::SymbolOwner::Local,
            ),
            (other.path().canonicalize()?, owner.clone()),
        ] {
            let mut lock = recorded_lock(1);
            lock.root = Some(recorded_root);
            lock.owner = Some(recorded_owner);
            let refusal = super::adopt_presence_for_owner(
                root.path(),
                &owner,
                crate::election::ServerPresence::Serving(Box::new(lock)),
                &test_identity(),
                &mut Replacement::default(),
            )
            .await
            .expect_err("a foreign lock cannot reach the connect or replacement path");
            assert_eq!(refusal.slug(), errors::mcp::election_storage_failed::SLUG);
        }
        assert!(!root.path().join(".rift").exists());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn forwarding_budget_records_the_registered_error_before_transport_conversion()
    -> TestResult {
        use rmcp::ServiceExt as _;

        let (_recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .capture("rift_mcp=debug")
            .install()?;
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("rift.toml"),
            "[server]\nreadiness_timeout = \"1s\"\nworker_queue_timeout = \"1s\"\n",
        )?;
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::default());
        let (running, _upstream_silent) = direct_upstream();
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let (proxy_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let (forwarding, client) =
            tokio::join!(proxy.serve(proxy_transport), ().serve(client_transport));
        let forwarding = forwarding?;
        let client = client?;
        let error = client
            .call_tool(CallToolRequestParams::new("search"))
            .await
            .expect_err("a silent upstream refuses at the forwarding budget");
        let ServiceError::McpError(error) = error else {
            return Err("forwarding budget refusal retains its JSON-RPC error".into());
        };
        let data: wire::ErrorData =
            serde_json::from_value(error.data.ok_or("refusal carries its wire payload")?)?;
        assert_eq!(data.code, wire::ErrorCode::TemporarilyUnavailable);
        assert_eq!(data.retry, wire::RetryDirective::SameRequest);
        let records = drain.queued_records();
        let completed = records
            .iter()
            .find(|record| record.message() == "tool forward completed")
            .ok_or("operation records its completed refusal")?;
        let fields: serde_json::Value = serde_json::from_str(completed.fields())?;
        let operation = &fields["root_span"];
        assert_eq!(operation["name"], "mcp.forward");
        assert_eq!(operation["fields"]["operation"], "tools/call");
        assert_eq!(operation["fields"]["tool"], "search");
        assert!(!operation["fields"]["caller_request_id"].is_null());
        assert!(!operation["fields"]["request_id"].is_null());
        assert_eq!(
            operation["fields"]["error.type"],
            errors::mcp::forward_unanswered::SLUG.as_str()
        );
        assert_eq!(fields["is_error"], "true");
        client.cancel().await?;
        forwarding.cancel().await?;
        Ok(())
    }

    #[tokio::test]
    async fn all_output_keeps_both_contents_of_a_resource_read() -> TestResult {
        let proxied = proxied_client(OutputPolicy::All).await?;
        let result = proxied
            .client
            .read_resource(ReadResourceRequestParams::new(TWO_CONTENT_URI))
            .await?;
        assert_eq!(
            media_types(&result),
            [Some("text/plain"), Some("application/json")]
        );
        Ok(())
    }

    #[test]
    fn text_output_passes_an_input_required_resource_read_through() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::Text);
        let input_required = InputRequiredResult::from_request_state("opaque");
        match proxy.selected_resource(ReadResourceResponse::InputRequired(input_required.clone())) {
            ReadResourceResponse::InputRequired(passed) => {
                assert_eq!(passed.request_state, input_required.request_state);
            }
            other => panic!("an input request must pass through, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_output_keeps_is_error_on_a_failing_result() -> TestResult {
        let proxied = proxied_client(OutputPolicy::Text).await?;
        let result = proxied
            .client
            .call_tool(CallToolRequestParams::new("failing"))
            .await?;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, None);
        assert_eq!(result.content.len(), 1, "content must stay: {result:?}");
        Ok(())
    }

    #[tokio::test]
    async fn all_output_keeps_schemas_and_structured_content_through_the_handlers() -> TestResult {
        let proxied = proxied_client(OutputPolicy::All).await?;
        let tools = proxied.client.list_all_tools().await?;
        assert_eq!(tools.len(), 2, "every page must be listed");
        assert!(
            tools.iter().all(|tool| tool.output_schema.is_some()),
            "all output must list the output schema on every page: {tools:?}"
        );

        let result = proxied
            .client
            .call_tool(CallToolRequestParams::new("search"))
            .await?;
        let value = result
            .structured_content
            .as_ref()
            .ok_or("structured search result")?;
        let result_view: rift_protocol::read::SearchResult = serde_json::from_value(value.clone())?;
        assert!(result_view.results.is_empty());
        let view = result_view
            .view
            .ok_or("search preserves its captured view")?;
        assert_eq!(view.id.as_str(), "claims.primary");
        assert_eq!(view.expires_at, "2099-01-01T00:00:00Z");
        assert_eq!(result.content.len(), 1, "content must stay: {result:?}");
        Ok(())
    }

    #[test]
    fn a_proxy_clone_keeps_the_output_policy() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::Text);
        assert_eq!(proxy.clone().output, OutputPolicy::Text);
    }

    #[test]
    fn text_output_passes_input_required_and_task_answers_through() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::Text);

        let input_required = InputRequiredResult::from_request_state("opaque");
        match proxy.selected_response(CallToolResponse::InputRequired(input_required.clone())) {
            CallToolResponse::InputRequired(passed) => {
                assert_eq!(passed.request_state, input_required.request_state);
            }
            other => panic!("an input request must pass through, got {other:?}"),
        }

        let task = CreateTaskResult::new(Task::new(
            "task-1",
            TaskStatus::Working,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
        ));
        match proxy.selected_response(CallToolResponse::Task(task.clone())) {
            CallToolResponse::Task(passed) => assert_eq!(passed, task),
            other => panic!("a task answer must pass through, got {other:?}"),
        }
    }

    /// A start window that closes on a held election with no document logs one record
    /// naming the rounds it ran, the last presence of the workspace election, the last
    /// repository miss, the time it took, and the refusal it returns. Repeated probes
    /// with the same failed read log that read once.
    #[tokio::test(start_paused = true)]
    async fn an_exhausted_start_window_records_what_it_last_saw() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let refusal = connect_upstream_with(directory.path(), &test_identity(), || {
            std::future::ready(RepositoryAsk::Terminal(RepositoryMiss::NotAdopted {
                pid: 4_242,
                settings_match: false,
                identity_adopted: true,
            }))
        })
        .await
        .expect_err("a holder that never publishes must exhaust the start window")
        .mcp()
        .tool_error(wire::ErrorPhase::Read);
        drop(recorder);
        let records = drain.queued_records();
        let messages = |message: &str| {
            records
                .iter()
                .filter(|record| record.message() == message)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            messages("election probe read failed").len(),
            1,
            "a failed read repeated every round logs once"
        );
        let closed = messages("start window closed without a server that answers");
        assert_eq!(closed.len(), 1, "the window's close logs once");
        assert_eq!(closed[0].level(), "info");
        let fields: serde_json::Value = serde_json::from_str(closed[0].fields())?;
        let rounds: u32 = fields["rounds"].as_str().ok_or("rounds")?.parse()?;
        assert!(rounds > 1, "{fields}");
        let elapsed_ms: u64 = fields["elapsed_ms"].as_str().ok_or("elapsed_ms")?.parse()?;
        assert!(
            elapsed_ms >= u64::try_from(crate::spawn::START_WAIT_MAX.as_millis())?,
            "{fields}"
        );
        assert_eq!(fields["presence"], "Starting");
        assert_eq!(fields["building"], "true");
        assert_eq!(fields["wrote"], "false");
        assert!(
            fields["repository_miss"]
                .as_str()
                .is_some_and(|miss| miss.starts_with("NotAdopted {")),
            "{fields}"
        );
        assert_eq!(fields["refusal"], refusal.message.as_ref());
        Ok(())
    }

    /// A start window whose repository asks miss transiently, then terminally, asks
    /// once per poll round until the terminal miss and never again, and its close names
    /// that last miss.
    #[tokio::test(start_paused = true)]
    async fn a_repository_miss_turning_terminal_inside_the_start_window_ends_the_asking()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let not_serving = || RepositoryMiss::NotServing {
            presence: "Absent".to_owned(),
        };
        let mut answers = vec![
            RepositoryAsk::Terminal(RepositoryMiss::NotAdopted {
                pid: 4_242,
                settings_match: false,
                identity_adopted: true,
            }),
            RepositoryAsk::Transient(not_serving()),
            RepositoryAsk::Transient(not_serving()),
        ];
        let mut asked = 0_u32;
        let refusal = connect_upstream_with(directory.path(), &test_identity(), || {
            asked += 1;
            std::future::ready(
                answers
                    .pop()
                    .unwrap_or(RepositoryAsk::Transient(not_serving())),
            )
        })
        .await
        .expect_err("a holder that never publishes must exhaust the start window")
        .mcp()
        .tool_error(wire::ErrorPhase::Read);
        drop(recorder);

        assert_eq!(asked, 3, "the terminal third ask ends the asking");
        assert_operator_refusal(&refusal);
        let records = drain.queued_records();
        let closed = records
            .iter()
            .find(|record| record.message() == "start window closed without a server that answers")
            .ok_or("the window's close is recorded")?;
        let fields: serde_json::Value = serde_json::from_str(closed.fields())?;
        let miss = fields["repository_miss"]
            .as_str()
            .ok_or("repository_miss")?;
        assert!(miss.starts_with("NotAdopted {"), "{fields}");
        Ok(())
    }

    /// Each repository miss logs one `info` record naming why the ask missed, except the
    /// workspace that selects its own server, which is the ordinary case.
    #[test]
    fn each_repository_miss_but_a_workspace_server_logs_why_the_ask_missed() -> TestResult {
        let misses = [
            RepositoryMiss::NotRepository,
            RepositoryMiss::SelectionRefused {
                detail: "rift.toml failed validation".to_owned(),
            },
            RepositoryMiss::ElectionDirectory {
                detail: "the identity did not serialize".to_owned(),
            },
            RepositoryMiss::NotServing {
                presence: "Absent".to_owned(),
            },
            RepositoryMiss::NotAdopted {
                pid: 4_242,
                settings_match: false,
                identity_adopted: true,
            },
            RepositoryMiss::Unanswered {
                pid: 4_242,
                port: 47_000,
                elapsed_ms: 5_000,
                detail: "connect timed out after 5s".to_owned(),
            },
        ];
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        for miss in &misses {
            miss.report();
        }
        drop(recorder);

        let records = drain.queued_records();
        let messages: Vec<&str> = records
            .iter()
            .map(rift_tracing::LogRecord::message)
            .collect();
        assert_eq!(
            messages,
            [
                "server configuration selection refused; polling the workspace election",
                "repository election directory unavailable; polling the workspace election",
                "repository server not serving; polling the workspace election",
                "repository server not adopted; polling the workspace election",
                "repository server did not answer; polling the workspace election",
            ]
        );
        assert!(records.iter().all(|record| record.level() == "info"));
        assert!(records[0].fields().contains("rift.toml failed validation"));
        assert!(
            records[1]
                .fields()
                .contains("the identity did not serialize")
        );
        Ok(())
    }

    /// A workspace whose configuration is refused misses the repository ask terminally,
    /// carrying the refusal's message.
    #[tokio::test]
    async fn a_refused_workspace_configuration_misses_the_repository_ask_terminally() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("rift.toml"), "unknown = true\n")?;
        let refused = crate::repository::select_server_configuration(directory.path(), None)
            .expect_err("an unknown key refuses the configuration");

        let asked = super::connect_repository_server(directory.path(), &test_identity()).await;

        assert!(
            matches!(
                &asked,
                RepositoryAsk::Terminal(RepositoryMiss::SelectionRefused { detail })
                    if *detail == refused.message
            ),
            "{asked:?}"
        );
        Ok(())
    }

    /// Accepts each connection to `listener` and closes it at once: a port that answers a
    /// connect and then serves nothing.
    async fn close_each_connection(listener: tokio::net::TcpListener) -> std::io::Result<()> {
        loop {
            drop(listener.accept().await?);
        }
    }

    /// A committed repository whose election, for this test's identity, records the
    /// serving server `lock` on a port that answers a connect and serves nothing.
    struct RecordedRepositoryServer {
        root: std::path::PathBuf,
        port: u16,
        _guard: crate::election::ElectionGuard,
        _closing: tokio::task::JoinHandle<std::io::Result<()>>,
        _directory: tempfile::TempDir,
    }

    async fn recorded_repository_server(
        server: Option<rift_protocol::configuration::ServerConfiguration>,
    ) -> TestResult<RecordedRepositoryServer> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        rift_history::fixture::init(&root);
        std::fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        rift_history::fixture::commit_all(&root, "add source");
        let common =
            crate::repository::discover_common_directory(&root).ok_or("a git directory")?;
        let identity = test_identity();
        let state = crate::repository::repository_election_directory(&common, &identity)?;
        let guard = crate::election::claim_state_directory(&state)?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        guard.publish(&ServerLock {
            server,
            ..recorded_lock(port)
        })?;
        Ok(RecordedRepositoryServer {
            root,
            port,
            _guard: guard,
            _closing: tokio::spawn(close_each_connection(listener)),
            _directory: directory,
        })
    }

    /// A repository server that records other settings than the workspace accepts is not
    /// adopted, and the ask misses terminally without connecting.
    #[tokio::test]
    async fn a_repository_server_recording_other_settings_misses_the_ask_terminally() -> TestResult
    {
        let recorded = recorded_repository_server(None).await?;

        let asked = super::connect_repository_server(&recorded.root, &test_identity()).await;

        assert!(
            matches!(
                asked,
                RepositoryAsk::Terminal(RepositoryMiss::NotAdopted {
                    pid: 4_242,
                    settings_match: false,
                    identity_adopted: true,
                })
            ),
            "{asked:?}"
        );
        Ok(())
    }

    /// A repository server this process adopts that does not answer the connect misses
    /// the ask transiently, naming its port.
    #[tokio::test]
    async fn an_adopted_repository_server_that_does_not_answer_misses_the_ask_transiently()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let accepted = crate::validation::ConfigurationState::accept(directory.path());
        let recorded = recorded_repository_server(Some(accepted.server_configuration())).await?;

        let asked = super::connect_repository_server(&recorded.root, &test_identity()).await;

        assert!(
            matches!(
                asked,
                RepositoryAsk::Transient(RepositoryMiss::Unanswered { pid: 4_242, port, .. })
                    if port == recorded.port
            ),
            "{asked:?}"
        );
        Ok(())
    }

    /// A recorded server whose port answers a connect but serves no MCP is treated as
    /// stale, and the failed connect is recorded.
    #[tokio::test]
    async fn adopt_treats_a_recorded_server_that_serves_nothing_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        guard.publish(&recorded_lock(listener.local_addr()?.port()))?;
        let closing = tokio::spawn(close_each_connection(listener));
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;

        let adopted = adopt_serving(
            directory.path(),
            &test_identity(),
            &mut Replacement::default(),
        )
        .await?;
        drop(recorder);

        assert!(
            adopted.is_none(),
            "a server that serves nothing is not adopted"
        );
        let records = drain.queued_records();
        let stale = records
            .iter()
            .find(|record| {
                record.message() == "recorded server did not answer; treating the lock as stale"
            })
            .ok_or("the failed connect is recorded")?;
        let fields: serde_json::Value = serde_json::from_str(stale.fields())?;
        let failure = fields["failure"]
            .as_str()
            .ok_or("stale connect failure is recorded")?;
        assert!(
            failure.contains("MCP proxy initialization failed"),
            "{failure}"
        );
        assert!(failure.contains("Send message error"), "{failure}");
        closing.abort();
        Ok(())
    }

    /// Answers each request read off `upstream` with `answer` of its method, as a
    /// workspace server answering a request with a result of the kind it chose.
    async fn answer_requests(
        upstream: tokio::io::DuplexStream,
        answer: fn(&str) -> serde_json::Value,
    ) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt as _;
        let (reading, mut writing) = tokio::io::split(upstream);
        let mut lines = tokio::io::BufReader::new(reading).lines();
        loop {
            let line = lines
                .next_line()
                .await?
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            let request: serde_json::Value = serde_json::from_str(&line)?;
            let result = answer(request["method"].as_str().unwrap_or_default());
            let response = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
            writing
                .write_all(format!("{response}\n").as_bytes())
                .await?;
        }
    }

    /// A downstream client declaring `client` of a proxy whose upstream answers through
    /// [`answer_requests`]. The fields keep the connection tasks alive.
    struct AnsweredClient {
        client: RunningService<RoleClient, rmcp::model::ClientConfig>,
        _answering: tokio::task::JoinHandle<std::io::Result<()>>,
        _connection: tokio::task::JoinHandle<Result<(), rift_error::RiftError>>,
        _directory: tempfile::TempDir,
    }

    async fn answered_client(
        client: rmcp::model::ClientConfig,
        answer: fn(&str) -> serde_json::Value,
    ) -> TestResult<AnsweredClient> {
        use rmcp::ServiceExt as _;
        let directory = tempfile::tempdir()?;
        let proxy = RiftProxy::new(directory.path(), test_identity(), OutputPolicy::All);
        let (running, upstream) = direct_upstream();
        {
            let mut slot = proxy.upstream.lock().await;
            slot.connected = Some(Upstream {
                running,
                generation: 0,
            });
            slot.generation_next = 1;
        }
        let answering = tokio::spawn(answer_requests(upstream, answer));
        let (proxy_half, client_half) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(serve_connection(proxy, proxy_half));
        Ok(AnsweredClient {
            client: Box::pin(client.serve(client_half)).await?,
            _answering: answering,
            _connection: connection,
            _directory: directory,
        })
    }

    /// A client declaring no capability, at the default protocol version.
    fn plain_client() -> rmcp::model::ClientConfig {
        rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("probe", "0.0.1"),
        )
    }

    /// The task a server answers a tool call with.
    fn working_task() -> CreateTaskResult {
        CreateTaskResult::new(Task::new(
            "task-1",
            TaskStatus::Working,
            "2026-01-01T00:00:00Z",
            "2026-01-01T00:00:00Z",
        ))
    }

    /// An upstream that answers each request with a result of another kind than the
    /// request reads: every forwarding handler refuses it as an unexpected response.
    #[tokio::test]
    async fn an_answer_of_another_kind_is_refused_by_every_forwarding_handler() -> TestResult {
        let answered = answered_client(plain_client(), |method| match method {
            "tools/list" => json!({"content": [{"type": "text", "text": "a tool result"}]}),
            _ => json!({"tools": []}),
        })
        .await?;
        let client = &answered.client;
        let refusals = [
            format!("{:?}", client.list_tools(None).await.err()),
            format!(
                "{:?}",
                client
                    .call_tool(CallToolRequestParams::new("search"))
                    .await
                    .err()
            ),
            format!("{:?}", client.list_resources(None).await.err()),
            format!("{:?}", client.list_resource_templates(None).await.err()),
            format!(
                "{:?}",
                client
                    .read_resource(ReadResourceRequestParams::new(TWO_CONTENT_URI))
                    .await
                    .err()
            ),
        ];
        for refusal in refusals {
            assert!(
                refusal.contains("failed the forwarded request"),
                "every handler refuses an answer of another kind: {refusal}"
            );
        }
        Ok(())
    }

    /// A task the upstream answers a tool call with reaches a client that declared tasks
    /// unchanged.
    #[tokio::test]
    async fn a_task_answer_passes_through_the_proxy_to_a_client_that_declared_tasks() -> TestResult
    {
        let client = rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::builder()
                .enable_tasks()
                .build(),
            rmcp::model::Implementation::new("probe", "0.0.1"),
        );
        let answered = answered_client(client, |_method| {
            serde_json::to_value(working_task()).unwrap_or_default()
        })
        .await?;

        let response = answered
            .client
            .call_tool_once(CallToolRequestParams::new("nodes"))
            .await?;

        assert!(
            matches!(&response, CallToolResponse::Task(task) if *task == working_task()),
            "{response:?}"
        );
        Ok(())
    }

    /// An input request the upstream answers a tool call or a resource read with leaves
    /// the proxy as an input request, which rmcp then refuses to a client that did not
    /// negotiate the 2026-07-28 protocol, not as the proxy's own unexpected-response
    /// refusal.
    #[tokio::test]
    async fn an_input_required_answer_leaves_the_proxy_as_an_input_request() -> TestResult {
        let answered = answered_client(plain_client(), |_method| {
            serde_json::to_value(InputRequiredResult::from_request_state("opaque"))
                .unwrap_or_default()
        })
        .await?;
        let client = &answered.client;

        let called = client
            .call_tool_once(CallToolRequestParams::new("nodes"))
            .await;
        let read = client
            .read_resource_once(ReadResourceRequestParams::new(TWO_CONTENT_URI))
            .await;

        for refusal in [format!("{:?}", called.err()), format!("{:?}", read.err())] {
            assert!(
                refusal.contains("InputRequiredResult requires negotiated protocol version"),
                "{refusal}"
            );
        }
        Ok(())
    }
}
