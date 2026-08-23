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
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::lock::{SERVER_PORT_MAX, SERVER_PORT_MIN};
use rift_server::ReadError;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::RiftMcp;
use crate::validation::IndexSupervisor;

/// Path the MCP Streamable-HTTP service is mounted at.
pub(crate) const MCP_PATH: &str = "/api/mcp";
/// Path an authorized `POST` stops the server through.
pub(crate) const STOP_PATH: &str = "/api/stop";
/// Bytes of entropy behind one minted bearer token.
const TOKEN_ENTROPY_BYTES: usize = 32;
/// The authentication scheme the `WWW-Authenticate` refusal names.
const BEARER_SCHEME: &str = "Bearer";
/// The scheme-plus-space prefix an accepted `Authorization` value carries.
const BEARER_PREFIX: &str = "Bearer ";

/// Failure while starting or running the Streamable-HTTP MCP transport.
pub type HttpServeError = Error<HttpServeFault>;

/// One HTTP transport failure: what stopped the server from starting or serving.
#[derive(Debug)]
pub enum HttpServeFault {
    /// Workspace snapshot could not be built, or its index supervisor did
    /// not shut down. Boxed to keep this transport error small beside it.
    Workspace(Box<ReadError>),
    /// Every port in the loopback serving range is already bound.
    PortsExhausted {
        /// The lowest port that was tried.
        port_min: u16,
        /// The highest port that was tried.
        port_max: u16,
    },
    /// The listener could not be prepared, or a serving task failed.
    Serve {
        /// The listener or serving operation that failed.
        operation: &'static str,
        /// The underlying failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Fault for HttpServeFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Workspace(source) => source.name(),
            Self::PortsExhausted { .. } => ErrorName::Wire(ErrorCode::TemporarilyUnavailable),
            Self::Serve { .. } => ErrorName::Wire(ErrorCode::InternalError),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Workspace(source) => source.context(),
            Self::PortsExhausted { port_min, port_max } => vec![
                ErrorContext::new("ports", format!("{port_min}..={port_max}")),
                ErrorContext::new(
                    "detail",
                    "every loopback port in the serving range is bound",
                ),
            ],
            Self::Serve { operation, .. } => vec![ErrorContext::new("operation", *operation)],
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Workspace(source) => Some(source.as_ref()),
            Self::PortsExhausted { .. } => None,
            Self::Serve { source, .. } => Some(source.as_ref()),
        }
    }
}

impl HttpServeFault {
    fn workspace(source: ReadError) -> HttpServeError {
        Error::new(Self::Workspace(Box::new(source)))
    }

    fn serve(
        operation: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> HttpServeError {
        Error::new(Self::Serve {
            operation,
            source: Box::new(source),
        })
    }
}

/// Serves the workspace at `root` over authenticated loopback Streamable HTTP.
///
/// The workspace builds exactly as stdio serving does — an invalid
/// `rift.toml` still serves, refusing each request as
/// `configuration_invalid` under default policies — then a bearer token is
/// minted and the first free port in the loopback serving range is bound.
/// The returned handle's listener is already accepting. Serving ends when
/// `shutdown` cancels, an authorized `POST /api/stop` arrives, or the
/// accepted `server.idle_timeout` passes without an authorized request.
///
/// # Errors
///
/// Returns [`HttpServeError`] when the workspace cannot be indexed, entropy
/// for the token is unavailable, every port in the serving range is bound,
/// or the listener cannot register with the runtime.
///
/// # Cancel safety
///
/// Dropping this future discards construction; an accepted initial index
/// scan still finishes in the bounded blocking executor. A returned
/// [`HttpServer`] owns the serving tasks and is driven through
/// [`HttpServer::stopped`].
pub async fn serve_http(
    root: &Path,
    shutdown: CancellationToken,
) -> Result<HttpServer, HttpServeError> {
    tracing::info!(component = "mcp", transport = "http", "MCP server starting");
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default())
        .await
        .map_err(HttpServeFault::workspace)?;
    let idle_timeout = Duration::from_millis(
        server
            .server_configuration()
            .await
            .idle_timeout
            .milliseconds(),
    );
    let supervisor = server.index_supervisor();
    let token = mint_token()?;
    let (port, listener) = bind_loopback_listener()?;
    let stop = shutdown.child_token();
    let idle = Arc::new(IdleTracker::new());
    let router = authenticated_router(server, &token, &stop, &idle);
    let serving = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .into_future(),
    );
    let idle_watch = tokio::spawn(watch_idle(idle, idle_timeout, stop.clone()));
    tracing::info!(
        component = "mcp",
        transport = "http",
        port,
        "MCP server ready"
    );
    Ok(HttpServer {
        port,
        token,
        stop,
        serving,
        idle_watch,
        supervisor,
    })
}

/// One serving HTTP MCP server: its address facts and its serving tasks.
#[derive(Debug)]
pub struct HttpServer {
    port: u16,
    token: String,
    stop: CancellationToken,
    serving: JoinHandle<Result<(), std::io::Error>>,
    idle_watch: JoinHandle<()>,
    supervisor: IndexSupervisor,
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

    /// Waits until the server stopped and its index supervisor shut down.
    ///
    /// Resolves after the serve loop ended — through the external shutdown
    /// token, an authorized `POST /api/stop`, or the idle timeout — and the
    /// workspace index supervisor finished within its shutdown deadline.
    ///
    /// # Errors
    ///
    /// Returns [`HttpServeError`] when the supervisor missed its shutdown
    /// deadline or a serving task failed.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future detaches the serving tasks; a shutdown already
    /// triggered still completes in the background.
    pub async fn stopped(self) -> Result<(), HttpServeError> {
        let serve_outcome = self.serving.await;
        // The serve loop can end on its own I/O error, where nothing has
        // cancelled the token yet; cancelling here unblocks the idle watch
        // on every path.
        self.stop.cancel();
        let idle_outcome = self.idle_watch.await;
        let supervisor_outcome = self
            .supervisor
            .shutdown()
            .await
            .map_err(HttpServeFault::workspace);
        let stopped_cleanly = supervisor_outcome.is_ok() && matches!(serve_outcome, Ok(Ok(())));
        tracing::info!(
            component = "mcp",
            transport = "http",
            outcome = if stopped_cleanly { "ok" } else { "error" },
            "MCP server stopped"
        );
        supervisor_outcome?;
        match serve_outcome {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(HttpServeFault::serve("http serve loop", error)),
            Err(error) => return Err(HttpServeFault::serve("http serve task", error)),
        }
        idle_outcome.map_err(|error| HttpServeFault::serve("idle watch task", error))
    }
}

/// Mints one bearer token: 32 random bytes as unpadded base64url.
///
/// The encoding fixes the length: `TOKEN_ENTROPY_BYTES` entropy bytes
/// spell exactly [`rift_protocol::lock::SERVER_TOKEN_LENGTH`] base64url
/// characters.
fn mint_token() -> Result<String, HttpServeError> {
    let mut entropy = [0_u8; TOKEN_ENTROPY_BYTES];
    getrandom::fill(&mut entropy).map_err(|error| HttpServeFault::serve("token mint", error))?;
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
) -> Result<(u16, Listener), HttpServeError> {
    let (port_min, port_max) = (*ports.start(), *ports.end());
    for port in ports {
        if let Ok(listener) = bind(port) {
            return Ok((port, listener));
        }
    }
    Err(Error::new(HttpServeFault::PortsExhausted {
        port_min,
        port_max,
    }))
}

/// Binds the first free loopback port in the serving range for the runtime.
fn bind_loopback_listener() -> Result<(u16, tokio::net::TcpListener), HttpServeError> {
    let (port, listener) = bind_first_free(SERVER_PORT_MIN..=SERVER_PORT_MAX, |port| {
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| HttpServeFault::serve("listener nonblocking mode", error))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|error| HttpServeFault::serve("listener runtime registration", error))?;
    Ok((port, listener))
}

/// Assembles the bearer-guarded routes over one served workspace.
///
/// Requests run statelessly: the service clones the server per request, and
/// no session is ever created.
fn authenticated_router(
    server: RiftMcp,
    token: &str,
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
        idle: Arc::clone(idle),
    };
    Router::new()
        .nest_service(MCP_PATH, mcp_service)
        .route(STOP_PATH, post(stop_server))
        .with_state(stop.clone())
        .layer(middleware::from_fn_with_state(gate, authorize_request))
}

/// Requests the same shutdown an external cancel performs.
///
/// Repeats answer identically: cancellation is idempotent.
async fn stop_server(State(stop): State<CancellationToken>) -> StatusCode {
    stop.cancel();
    StatusCode::ACCEPTED
}

/// Per-request policy shared by every route: the token requests must
/// present, and the activity instant authorized requests refresh.
#[derive(Clone, Debug)]
struct RequestGate {
    token: Arc<str>,
    idle: Arc<IdleTracker>,
}

/// Refuses requests without the exact bearer token, touching the idle
/// tracker for every request that passes.
///
/// The token separates OS users sharing the machine; the loopback bind is
/// the network boundary.
async fn authorize_request(
    State(gate): State<RequestGate>,
    request: Request,
    next: Next,
) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if !bearer_authorized(authorization, &gate.token) {
        return unauthorized();
    }
    gate.idle.touch();
    next.run(request).await
}

/// Whether an `Authorization` value presents exactly `Bearer` plus `token`.
///
/// Equal-length comparison touches every byte, so a refusal's timing does
/// not reveal how much of the token matched. The earlier returns reveal
/// only the request's own shape — a missing scheme or a wrong length —
/// never a token byte.
fn bearer_authorized(authorization: Option<&str>, token: &str) -> bool {
    let Some(presented) = authorization.and_then(|value| value.strip_prefix(BEARER_PREFIX)) else {
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

/// The instant of the most recent authorized request.
#[derive(Debug)]
struct IdleTracker {
    last_activity: Mutex<Instant>,
}

impl IdleTracker {
    fn new() -> Self {
        Self {
            last_activity: Mutex::new(Instant::now()),
        }
    }

    /// Records an authorized request at the current instant.
    fn touch(&self) {
        *self.lock_last_activity() = Instant::now();
    }

    /// The instant the server has been quiet for `idle_timeout`.
    fn idle_deadline(&self, idle_timeout: Duration) -> Instant {
        *self.lock_last_activity() + idle_timeout
    }

    /// The tracked instant, recovered from a poisoned lock: the stored
    /// value is plain data, valid regardless of a panicked writer.
    fn lock_last_activity(&self) -> MutexGuard<'_, Instant> {
        match self.last_activity.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Cancels `stop` once `idle_timeout` passes with no authorized request.
///
/// Each turn sleeps to the currently tracked deadline; a touch during the
/// sleep moves the deadline, and the next turn waits out the remainder, so
/// turns recur only as often as requests arrive.
///
/// # Cancel safety
///
/// Dropping this future ends the watch without cancelling `stop`.
async fn watch_idle(idle: Arc<IdleTracker>, idle_timeout: Duration, stop: CancellationToken) {
    loop {
        let deadline = idle.idle_deadline(idle_timeout);
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep_until(deadline) => {}
        }
        if Instant::now() >= idle.idle_deadline(idle_timeout) {
            stop.cancel();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::http::{StatusCode, header};
    use rift_protocol::lock::{SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, ServerLock};
    use rift_server::ReadFault;
    use tokio_util::sync::CancellationToken;

    use super::{
        HttpServeFault, IdleTracker, bearer_authorized, bind_first_free, mint_token, unauthorized,
        watch_idle,
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
            version: "0.0.9".to_owned(),
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
        assert!(matches!(
            error.fault(),
            HttpServeFault::PortsExhausted {
                port_min: 4,
                port_max: 6,
            }
        ));
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        let rendered = error.to_string();
        assert!(rendered.contains("4..=6"), "{rendered}");
    }

    #[test]
    fn workspace_fault_keeps_the_read_classification_and_source() {
        let read = ReadFault::unavailable("probe", "detail");
        let expected = read.descriptor();
        let error = HttpServeFault::workspace(read);
        assert_eq!(error.descriptor(), expected);
        assert!(
            std::error::Error::source(&error).is_some(),
            "the wrapped read failure must stay on the source chain"
        );
    }

    #[test]
    fn serve_fault_names_its_operation_and_exposes_the_source() {
        let error = HttpServeFault::serve("http serve loop", std::io::Error::other("socket gone"));
        assert_eq!(error.descriptor().code(), "internal_error");
        let rendered = error.to_string();
        assert!(rendered.contains("http serve loop"), "{rendered}");
        let source = std::error::Error::source(&error).expect("source must be exposed");
        assert_eq!(source.to_string(), "socket gone");
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
    async fn touched_activity_defers_the_idle_stop() {
        let idle = Arc::new(IdleTracker::new());
        let stop = CancellationToken::new();
        let watch = tokio::spawn(watch_idle(
            Arc::clone(&idle),
            Duration::from_secs(5),
            stop.clone(),
        ));
        tokio::time::sleep(Duration::from_secs(3)).await;
        idle.touch();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !stop.is_cancelled(),
            "a touch inside the span must defer the stop"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        watch.await.expect("watch task must join");
        assert!(
            stop.is_cancelled(),
            "the deferred deadline must still stop the server"
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
}
