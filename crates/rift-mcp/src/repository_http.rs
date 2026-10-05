//! Repository workspace admission, routing, and idle release.

use std::future::IntoFuture as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, post};
use rift_error::{RiftError, errors};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::ServerConfiguration;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::sync::{Mutex as AsyncMutex, OnceCell, Semaphore};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tower_service::Service as _;

use crate::RiftMcp;
use crate::election::{ElectionGuard, claim};
use crate::http::{
    HttpServer, IdleTracker, MCP_PATH, RequestGate, STOP_PATH, TokenCheck, WORKSPACE_ROOT_HEADER,
    authorize_request, bind_loopback_listener, guard_loopback_boundary, mint_token, watch_idle,
};
use crate::identity::BuildCheckout;
use crate::repository::{ServerConfigurationSelection, select_server_configuration};
use crate::server::{BlockingExecutor, EngineHold};
use crate::storage::WorkspaceStorage;
use crate::validation::IndexSupervisor;

/// Maximum workspace root bytes accepted from one request.
const WORKSPACE_ROOT_BYTES_MAX: usize = 4_096;

/// Interval between idle workspace eviction passes.
const IDLE_EVICTION_TICK: Duration = Duration::from_secs(1);

/// Wall-clock bound one workspace stop spends on its engines, supervisor, and database.
const WORKSPACE_STOP_BOUND: Duration = Duration::from_secs(4);

/// Serves one repository's workspaces through a process-wide bounded executor.
///
/// Each admitted workspace retains its own server and mutable stores.
pub(crate) async fn serve_repository_http(
    authority_root: &Path,
    common_directory: &Path,
    server_configuration: ServerConfiguration,
    shutdown: CancellationToken,
    limits: WorkspaceIndexLimits,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<HttpServer, RiftError> {
    let authority_root = tokio::fs::canonicalize(authority_root)
        .await
        .map_err(|error| {
            errors::mcp::http_serve_failed()
                .operation("repository authority root")
                .source(error)
                .error()
        })?;
    let common_directory = tokio::fs::canonicalize(common_directory)
        .await
        .map_err(|error| {
            errors::mcp::http_serve_failed()
                .operation("repository common directory")
                .source(error)
                .error()
        })?;
    let blocking = BlockingExecutor::for_configuration(&server_configuration)?;
    let identity = crate::identity::product_identity(checkout)
        .await
        .map_err(|error| {
            errors::mcp::http_serve_failed()
                .operation("product identity")
                .source(error)
                .error()
        })?;
    let token = mint_token()?;
    let (port, listener) = bind_loopback_listener(server_configuration.serving_ports())?;
    let stop = shutdown.child_token();
    let idle = Arc::new(IdleTracker::new());
    let registry = Arc::new(RepositoryWorkspaceRegistry {
        authority_root,
        common_directory,
        server_configuration: server_configuration.clone(),
        limits,
        checkout,
        blocking,
        stop: stop.clone(),
        idle_timeout: Duration::from_millis(server_configuration.idle_timeout.milliseconds()),
        admissions: Arc::new(Semaphore::new(
            usize::try_from(server_configuration.num_workers).unwrap_or(1),
        )),
        workspaces_max: usize::try_from(server_configuration.workspaces).unwrap_or(1),
        build_gate: AsyncMutex::new(()),
        workspaces: AsyncMutex::new(std::collections::BTreeMap::new()),
    });
    let router =
        authenticated_repository_router(Arc::clone(&registry), &token, check, &stop, &idle);
    let serving = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(stop.clone().cancelled_owned())
            .into_future(),
    );
    let idle_watch = tokio::spawn(watch_idle(
        Arc::clone(&idle),
        Duration::from_millis(server_configuration.idle_timeout.milliseconds()),
        stop.clone(),
    ));
    let repository_idle_watch =
        tokio::spawn(watch_repository_idle(Arc::clone(&registry), stop.clone()));
    rift_tracing::info!(
        component = "mcp",
        transport = "http",
        port,
        "MCP server ready"
    );
    Ok(HttpServer {
        port,
        token,
        identity,
        server_configuration,
        stop,
        serving,
        idle_watch,
        repository_idle_watch: Some(repository_idle_watch),
        supervisor: None,
        engines: None,
        search_index: None,
        logs: None,
        repository_workspaces: Some(registry),
    })
}

/// One workspace's service and exclusive workspace-election lease.
struct RepositoryWorkspace {
    service: StreamableHttpService<RiftMcp, NeverSessionManager>,
    supervisor: IndexSupervisor,
    engines: Arc<EngineHold>,
    database: Option<Arc<rift_index::WorkspaceDatabase>>,
    vectors: Option<Arc<rift_index::LazyDatabase>>,
    logs: Option<Arc<rift_tracing::LogStore>>,
    stop: CancellationToken,
    activity: Arc<IdleTracker>,
    lease: AsyncMutex<Option<Arc<ElectionGuard>>>,
}

/// One request holds workspace activity until the service returns.
struct RepositoryRequestActivity {
    activity: Arc<IdleTracker>,
}

impl Drop for RepositoryRequestActivity {
    fn drop(&mut self) {
        self.activity.finish();
    }
}

/// Services admitted by one repository server; requests share one bounded worker pool.
pub(crate) struct RepositoryWorkspaceRegistry {
    authority_root: std::path::PathBuf,
    common_directory: std::path::PathBuf,
    server_configuration: ServerConfiguration,
    limits: WorkspaceIndexLimits,
    checkout: BuildCheckout,
    blocking: BlockingExecutor,
    stop: CancellationToken,
    idle_timeout: Duration,
    admissions: Arc<Semaphore>,
    workspaces_max: usize,
    build_gate: AsyncMutex<()>,
    workspaces: AsyncMutex<
        std::collections::BTreeMap<std::path::PathBuf, Arc<OnceCell<RepositoryWorkspace>>>,
    >,
}

impl std::fmt::Debug for RepositoryWorkspaceRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RepositoryWorkspaceRegistry")
            .field("common_directory", &self.common_directory)
            .finish_non_exhaustive()
    }
}

impl RepositoryWorkspaceRegistry {
    async fn route(&self, mut request: Request) -> Response {
        let workspace_root = match self.requested_workspace_root(&request) {
            Ok(root) => root,
            Err((status, message)) => return (status, message).into_response(),
        };
        request.headers_mut().remove(WORKSPACE_ROOT_HEADER);
        let workspace_root = match self.normalize_workspace_root(workspace_root).await {
            Ok(root) => root,
            Err(error) => return error.into_response(),
        };
        let (mut service, _activity) = match self.service_for(&workspace_root).await {
            Ok(service) => service,
            Err((status, message)) => return (status, message).into_response(),
        };
        if std::future::poll_fn(|context| {
            tower_service::Service::<Request>::poll_ready(&mut service, context)
        })
        .await
        .is_err()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "workspace service unavailable",
            )
                .into_response();
        }
        match service.call(request).await {
            Ok(response) => response.map(Body::new),
            Err(never) => match never {},
        }
    }

    fn requested_workspace_root(
        &self,
        request: &Request,
    ) -> Result<std::path::PathBuf, (StatusCode, &'static str)> {
        let path = match request.headers().get(WORKSPACE_ROOT_HEADER) {
            Some(value) if value.as_bytes().len() <= WORKSPACE_ROOT_BYTES_MAX => {
                std::str::from_utf8(value.as_bytes())
                    .map_err(|_| (StatusCode::BAD_REQUEST, "workspace root must be UTF-8"))?
            }
            Some(_) => return Err((StatusCode::BAD_REQUEST, "workspace root exceeds limit")),
            None => self.authority_root.to_str().ok_or((
                StatusCode::BAD_REQUEST,
                "repository authority root must be UTF-8",
            ))?,
        };
        Ok(PathBuf::from(path))
    }

    async fn normalize_workspace_root(
        &self,
        path: PathBuf,
    ) -> Result<PathBuf, (StatusCode, &'static str)> {
        if self.workspaces.lock().await.contains_key(&path) {
            return Ok(path);
        }
        self.blocking
            .run_with_cancellation("workspace.root", self.stop.clone(), move |_| {
                let root = std::fs::canonicalize(path)
                    .map_err(|_| (StatusCode::BAD_REQUEST, "workspace root is unavailable"));
                Ok(root.and_then(|root| {
                    if root.is_dir() {
                        Ok(root)
                    } else {
                        Err((StatusCode::BAD_REQUEST, "workspace root is not a directory"))
                    }
                }))
            })
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "workspace root validation is unavailable",
                )
            })?
    }

    async fn service_for(
        &self,
        root: &Path,
    ) -> Result<
        (
            StreamableHttpService<RiftMcp, NeverSessionManager>,
            RepositoryRequestActivity,
        ),
        (StatusCode, &'static str),
    > {
        let root = root.to_path_buf();
        let existing_cell = self.workspaces.lock().await.get(&root).cloned();
        if let Some(cell) = existing_cell
            && cell.get().is_some()
        {
            return self.active_service(&root, &cell).await;
        }
        if self.stop.is_cancelled() {
            return Err(Self::stopping());
        }
        let started = Instant::now();
        let _admission = self.admit().await?;
        self.validate_workspace_settings(root.clone()).await?;
        let cell = self.workspace_cell(&root).await?;
        let workspace = cell
            .get_or_try_init(|| self.build_workspace(root.clone()))
            .await;
        let elapsed_ms = started.elapsed().as_millis();
        match workspace {
            Ok(_) => {
                rift_tracing::info!(
                    component = "mcp",
                    root = %root.display(),
                    elapsed_ms,
                    "repository workspace ready for its first request"
                );
                self.active_service(&root, &cell).await
            }
            Err(error) => {
                rift_tracing::warn!(
                    component = "mcp",
                    root = %root.display(),
                    elapsed_ms,
                    status = %error.0,
                    refusal = error.1,
                    "repository workspace build refused"
                );
                self.remove_failed_cell(&root, &cell).await;
                Err(error)
            }
        }
    }

    async fn remove_failed_cell(&self, root: &Path, cell: &Arc<OnceCell<RepositoryWorkspace>>) {
        let mut workspaces = self.workspaces.lock().await;
        if workspaces
            .get(root)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
            && cell.get().is_none()
            && Arc::strong_count(cell) == 2
        {
            workspaces.remove(root);
        }
    }

    async fn admit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, (StatusCode, &'static str)> {
        let timeout = Duration::from_millis(
            self.server_configuration
                .worker_queue_timeout
                .milliseconds(),
        );
        tokio::select! {
            () = self.stop.cancelled() => Err(Self::stopping()),
            acquired = tokio::time::timeout(timeout, Arc::clone(&self.admissions).acquire_owned()) => {
                acquired.ok().and_then(Result::ok).ok_or((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "repository admission timed out",
                ))
            }
        }
    }

    fn stopping() -> (StatusCode, &'static str) {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "repository server is stopping",
        )
    }

    async fn validate_workspace_settings(
        &self,
        root: PathBuf,
    ) -> Result<(), (StatusCode, &'static str)> {
        let process_server = self.server_configuration.clone();
        let selection = self
            .blocking
            .run_with_cancellation("workspace.settings", self.stop.clone(), move |_| {
                Ok(select_server_configuration(&root, Some(&process_server)))
            })
            .await
            .map_err(|_| {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "workspace settings unavailable",
                )
            })?
            .map_err(|_| (StatusCode::CONFLICT, "workspace settings are invalid"))?;
        match selection {
            ServerConfigurationSelection::Repository {
                common_directory, ..
            } if common_directory == self.common_directory => Ok(()),
            _ => Err((
                StatusCode::CONFLICT,
                "workspace does not share repository settings",
            )),
        }
    }

    async fn workspace_cell(
        &self,
        root: &Path,
    ) -> Result<Arc<OnceCell<RepositoryWorkspace>>, (StatusCode, &'static str)> {
        let mut workspaces = self.workspaces.lock().await;
        if let Some(cell) = workspaces.get(root) {
            return Ok(Arc::clone(cell));
        }
        if workspaces.len() >= self.workspaces_max {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "repository workspace limit reached",
            ));
        }
        let cell = Arc::new(OnceCell::new());
        workspaces.insert(root.to_path_buf(), Arc::clone(&cell));
        Ok(cell)
    }

    async fn build_workspace(
        &self,
        root: PathBuf,
    ) -> Result<RepositoryWorkspace, (StatusCode, &'static str)> {
        let timeout = Duration::from_millis(
            self.server_configuration
                .worker_queue_timeout
                .milliseconds(),
        );
        let _build = tokio::select! {
            () = self.stop.cancelled() => return Err(Self::stopping()),
            acquired = tokio::time::timeout(timeout, self.build_gate.lock()) => acquired.map_err(|_| {
                (StatusCode::SERVICE_UNAVAILABLE, "workspace build admission timed out")
            })?,
        };
        if self.stop.is_cancelled() {
            return Err(Self::stopping());
        }
        let lease_root = root.clone();
        let lease = self
            .blocking
            .run_with_cancellation("workspace.election", self.stop.clone(), move |_| {
                Ok(claim(&lease_root))
            })
            .await
            .map_err(|_| Self::stopping())?
            .map_err(|_| {
                (
                    StatusCode::CONFLICT,
                    "workspace already has a serving process",
                )
            })?;
        let lease = Arc::new(lease);
        let storage = WorkspaceStorage::open_with_owner(&root, Some(Arc::clone(&lease))).await;
        let database = storage.database();
        let vectors = storage.vectors();
        let logs = storage.logs();
        let server = RiftMcp::build_with_storage_and_executor(
            &root,
            self.limits,
            storage,
            self.checkout,
            self.blocking.clone(),
        )
        .await
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "workspace build failed"))?;
        let settings_changed = server.server_configuration().await != self.server_configuration;
        let supervisor = server.index_supervisor();
        let engines = server.engine_hold();
        let service_stop = self.stop.child_token();
        let activity = server.request_activity();
        let service = StreamableHttpService::new(
            move || Ok(server.clone()),
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true)
                .with_cancellation_token(service_stop.clone()),
        );
        let workspace = RepositoryWorkspace {
            service,
            supervisor,
            engines,
            database,
            vectors,
            logs,
            stop: service_stop,
            activity,
            lease: AsyncMutex::new(Some(lease)),
        };
        if settings_changed {
            // Keep the store lease until the workspace's workers stop, even if shutdown fails.
            if let Err(error) =
                stop_repository_workspace(&workspace, Instant::now() + WORKSPACE_STOP_BOUND).await
            {
                rift_tracing::warn!(component = "mcp", %error, "changed workspace settings shutdown failed");
            }
        }
        Ok(workspace)
    }

    async fn active_service(
        &self,
        root: &Path,
        cell: &Arc<OnceCell<RepositoryWorkspace>>,
    ) -> Result<
        (
            StreamableHttpService<RiftMcp, NeverSessionManager>,
            RepositoryRequestActivity,
        ),
        (StatusCode, &'static str),
    > {
        let workspaces = self.workspaces.lock().await;
        if !workspaces
            .get(root)
            .is_some_and(|current| Arc::ptr_eq(current, cell))
        {
            return Err((StatusCode::SERVICE_UNAVAILABLE, "workspace is stopping"));
        }
        let workspace = cell
            .get()
            .ok_or((StatusCode::SERVICE_UNAVAILABLE, "workspace is starting"))?;
        if workspace.stop.is_cancelled() {
            return Err(Self::stopping());
        }
        workspace.activity.start();
        let activity = RepositoryRequestActivity {
            activity: Arc::clone(&workspace.activity),
        };
        Ok((workspace.service.clone(), activity))
    }

    /// Stops every retained workspace before the repository server leaves.
    ///
    /// # Errors
    ///
    /// Returns a workspace shutdown failure.
    pub(crate) async fn shutdown(&self, deadline: Instant) -> Result<(), RiftError> {
        let workspaces = std::mem::take(&mut *self.workspaces.lock().await);
        for cell in workspaces.values() {
            if let Some(workspace) = cell.get() {
                workspace.stop.cancel();
            }
        }
        let mut outcome = Ok(());
        for (_, cell) in workspaces {
            let Some(workspace) = cell.get() else {
                continue;
            };
            if let Err(error) = stop_repository_workspace(workspace, deadline).await {
                outcome = Err(error);
            }
            let retired = cell;
            drop(tokio::task::spawn_blocking(move || drop(retired)));
        }
        outcome
    }

    async fn evict_idle(&self) {
        // A failed initializer can leave waiters that retry through the same cell.
        // Remove empty cells only after every caller has released its reference.
        self.workspaces
            .lock()
            .await
            .retain(|_, cell| cell.get().is_some() || Arc::strong_count(cell) > 1);
        let expired = self
            .workspaces
            .lock()
            .await
            .iter()
            .filter(|(_, cell)| {
                cell.get().is_some_and(|workspace| {
                    workspace
                        .activity
                        .idle_deadline(self.idle_timeout)
                        .is_some_and(|deadline| deadline <= Instant::now())
                })
            })
            .map(|(root, cell)| (root.clone(), Arc::clone(cell)))
            .collect::<Vec<_>>();
        for (root, cell) in expired {
            let workspaces = self.workspaces.lock().await;
            let Some(workspace) = cell.get() else {
                continue;
            };
            let expired = workspace
                .activity
                .idle_deadline(self.idle_timeout)
                .is_some_and(|deadline| deadline <= Instant::now());
            if !expired {
                continue;
            }
            workspace.stop.cancel();
            drop(workspaces);
            let started = Instant::now();
            let deadline = started + WORKSPACE_STOP_BOUND;
            rift_tracing::info!(
                component = "mcp",
                root = %root.display(),
                "idle workspace shutdown started"
            );
            if let Err(error) = stop_repository_workspace(workspace, deadline).await {
                rift_tracing::warn!(
                    component = "mcp",
                    %error,
                    root = %root.display(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "idle workspace shutdown failed"
                );
                continue;
            }
            self.workspaces.lock().await.remove(&root);
            rift_tracing::info!(
                component = "mcp",
                root = %root.display(),
                elapsed_ms = started.elapsed().as_millis(),
                "idle workspace released"
            );
            drop(tokio::task::spawn_blocking(move || drop(cell)));
        }
    }
}

async fn stop_repository_workspace(
    workspace: &RepositoryWorkspace,
    deadline: Instant,
) -> Result<(), RiftError> {
    workspace.stop.cancel();
    let engines = tokio::time::timeout_at(deadline, workspace.engines.shutdown())
        .await
        .map_err(|error| {
            errors::mcp::http_serve_failed()
                .operation("workspace engines shutdown")
                .source(error)
                .error()
        });
    let supervisor = workspace.supervisor.shutdown(deadline).await;
    let (index, vectors) = tokio::join!(
        async {
            match workspace.database.as_ref() {
                Some(database) => database.shutdown(deadline).await,
                None => Ok(()),
            }
        },
        async {
            match workspace.vectors.as_ref() {
                Some(vectors) => vectors.shutdown(deadline).await,
                None => Ok(()),
            }
        }
    );
    let database = index.and(vectors).map_err(|error| {
        errors::mcp::http_serve_failed()
            .operation("SQLite worker shutdown")
            .cause(error)
            .error()
    });
    let logs = crate::http::close_logs(workspace.logs.as_deref(), deadline).await;
    let outcome = engines.and(supervisor).and(database).and(logs);
    if outcome.is_ok() {
        drop(workspace.lease.lock().await.take());
    }
    outcome
}

async fn watch_repository_idle(
    registry: Arc<RepositoryWorkspaceRegistry>,
    stop: CancellationToken,
) {
    let mut tick = tokio::time::interval(IDLE_EVICTION_TICK);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = tick.tick() => registry.evict_idle().await,
        }
    }
}

fn authenticated_repository_router(
    registry: Arc<RepositoryWorkspaceRegistry>,
    token: &str,
    check: TokenCheck,
    stop: &CancellationToken,
    idle: &Arc<IdleTracker>,
) -> Router {
    let gate = RequestGate {
        token: Arc::from(token),
        check,
        idle: Arc::clone(idle),
    };
    Router::new()
        .route(MCP_PATH, any(repository_mcp_request))
        .route(STOP_PATH, post(repository_stop_server))
        .with_state(RepositoryRouterState {
            registry,
            stop: stop.clone(),
        })
        .layer(middleware::from_fn_with_state(gate, authorize_request))
        .layer(middleware::from_fn(guard_loopback_boundary))
}

async fn repository_mcp_request(
    State(state): State<RepositoryRouterState>,
    request: Request,
) -> Response {
    state.registry.route(request).await
}

#[derive(Clone)]
struct RepositoryRouterState {
    registry: Arc<RepositoryWorkspaceRegistry>,
    stop: CancellationToken,
}

async fn repository_stop_server(State(state): State<RepositoryRouterState>) -> StatusCode {
    state.stop.cancel();
    StatusCode::ACCEPTED
}

#[cfg(test)]
mod tests {
    use super::serve_repository_http;
    use crate::http::{HttpServer, TokenCheck, WORKSPACE_ROOT_HEADER};
    use crate::identity::BuildCheckout;
    use reqwest::header::{HeaderName, HeaderValue};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::lock::ServerLock;
    use rmcp::ServiceExt as _;
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    /// Diagnostics for PR #533: idle eviction events reach the test output.
    fn diagnostic_log() -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_env_filter("rift_mcp::repository_http=info")
                .with_ansi(false)
                .with_test_writer()
                .finish(),
        )
    }

    /// The retained workspace roots, and for PR #533 diagnostics each one's idle state,
    /// read under one registry lock.
    async fn retained_workspaces(
        registry: &super::RepositoryWorkspaceRegistry,
    ) -> (Vec<std::path::PathBuf>, String) {
        let now = tokio::time::Instant::now();
        let workspaces = registry.workspaces.lock().await;
        let roots = workspaces.keys().cloned().collect::<Vec<_>>();
        let states = workspaces
            .iter()
            .map(|(root, cell)| match cell.get() {
                None => format!("{}: no workspace in the cell", root.display()),
                Some(workspace) => {
                    let deadline = workspace.activity.idle_deadline(registry.idle_timeout);
                    format!(
                        "{}: request active {}, stop cancelled {}, idle deadline passed by {:?}, \
                         idle deadline ahead by {:?}",
                        root.display(),
                        deadline.is_none(),
                        workspace.stop.is_cancelled(),
                        deadline.and_then(|deadline| now.checked_duration_since(deadline)),
                        deadline.and_then(|deadline| deadline.checked_duration_since(now)),
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        (roots, states)
    }

    /// Keeps `kept` active until it is the only retained workspace and has outlived its own
    /// idle deadline by one eviction tick. Fails once the release bound passes: after the
    /// last answer from an expired root, the idle timeout, one eviction tick, and one stop
    /// bound per expired root.
    async fn keep_one_until_others_release(
        server: &HttpServer,
        kept: &Path,
        symbol: &str,
        expired_roots: u32,
        others_answered: tokio::time::Instant,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let registry = server
            .repository_workspaces
            .as_ref()
            .ok_or("repository server must retain its workspace registry")?;
        let kept_root = std::fs::canonicalize(kept)?;
        let kept_outlived =
            tokio::time::Instant::now() + registry.idle_timeout + super::IDLE_EVICTION_TICK;
        let released_by = others_answered
            + registry.idle_timeout
            + super::IDLE_EVICTION_TICK
            + super::WORKSPACE_STOP_BOUND * expired_roots;
        rift_tracing::info!("keep-alive requests started");
        loop {
            let _ = repository_symbol(server, kept, symbol).await?;
            let (retained_roots, states) = retained_workspaces(registry).await;
            let now = tokio::time::Instant::now();
            if retained_roots.as_slice() == std::slice::from_ref(&kept_root) && now >= kept_outlived
            {
                return Ok(());
            }
            if now >= released_by.max(kept_outlived) {
                return Err(format!(
                    "retained {retained_roots:?} past the release bound\n{states}"
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    async fn repository_symbol(
        server: &HttpServer,
        root: &Path,
        name: &str,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let mut config = StreamableHttpClientTransportConfig::with_uri(format!(
            "http://127.0.0.1:{}/api/mcp",
            server.port()
        ));
        config.auth_header = Some(server.token().to_owned());
        config.custom_headers.insert(
            HeaderName::from_static(WORKSPACE_ROOT_HEADER),
            HeaderValue::from_str(&std::fs::canonicalize(root)?.to_string_lossy())?,
        );
        let service = ().serve(StreamableHttpClientTransport::from_config(config)).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let response = loop {
            let response = service
                .call_tool(
                    CallToolRequestParams::new("get_symbol").with_arguments(
                        serde_json::json!({"name": name})
                            .as_object()
                            .cloned()
                            .ok_or("tool arguments must be an object")?,
                    ),
                )
                .await?;
            if response
                .structured_content
                .as_ref()
                .and_then(|content| content["hits"].as_array())
                .is_some_and(|hits| !hits.is_empty())
                || tokio::time::Instant::now() >= deadline
            {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        service.cancel().await?;
        response
            .structured_content
            .ok_or_else(|| "structured content must be served".into())
    }

    #[tokio::test]
    async fn failed_initializer_keeps_a_waiters_successful_workspace_registered()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = std::fs::canonicalize(directory.path())?;
        crate::server::hermetic_workspace(&root, "")?;
        std::fs::write(root.join("lib.rs"), "pub fn amber() {}\n")?;
        rift_history::fixture::init(&root);
        rift_history::fixture::commit_all(&root, "add workspace source");
        let repository = rift_history::Repository::open(&root)?;
        let shutdown = CancellationToken::new();
        let server = serve_repository_http(
            &root,
            repository.common_directory(),
            crate::validation::ConfigurationState::accept(&root).server_configuration(),
            shutdown.clone(),
            WorkspaceIndexLimits::default(),
            TokenCheck::Required,
            BuildCheckout::Unversioned,
        )
        .await?;
        let registry = Arc::clone(
            server
                .repository_workspaces
                .as_ref()
                .ok_or("workspace registry exists")?,
        );
        let cell = registry
            .workspace_cell(&root)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let retry_cell = Arc::clone(&cell);
        let (first_started, is_first_started) = tokio::sync::oneshot::channel();
        let (fail, failure_allowed) = tokio::sync::oneshot::channel();
        let first_registry = Arc::clone(&registry);
        let first_root = root.clone();
        let first = tokio::spawn(async move {
            let result = cell
                .get_or_try_init(|| async {
                    let _ = first_started.send(());
                    let _ = failure_allowed.await;
                    Err((axum::http::StatusCode::CONFLICT, "fixture startup failure"))
                })
                .await;
            assert!(result.is_err());
            first_registry.remove_failed_cell(&first_root, &cell).await;
        });
        is_first_started.await?;
        let (retry_started, is_retry_started) = tokio::sync::oneshot::channel();
        let (succeed, success_allowed) = tokio::sync::oneshot::channel();
        let retry_registry = Arc::clone(&registry);
        let retry_root = root.clone();
        let retry = tokio::spawn(async move {
            retry_cell
                .get_or_try_init(|| async {
                    let _ = retry_started.send(());
                    let _ = success_allowed.await;
                    retry_registry.build_workspace(retry_root.clone()).await
                })
                .await?;
            let _service = retry_registry
                .active_service(&retry_root, &retry_cell)
                .await?;
            Ok::<_, (axum::http::StatusCode, &'static str)>(())
        });
        fail.send(())
            .map_err(|()| "first initializer still waits")?;
        first.await?;
        is_retry_started.await?;
        assert!(
            registry.workspaces.lock().await.contains_key(&root),
            "failed request cannot remove a retrying caller's cell"
        );
        succeed
            .send(())
            .map_err(|()| "retry initializer still waits")?;
        retry
            .await?
            .map_err(|error| format!("retry must serve through retained cell: {error:?}"))?;
        assert!(
            crate::election::claim(&root).is_err(),
            "successful retry owns the workspace store"
        );
        shutdown.cancel();
        let (_deadline, outcome) = server.stopped(Duration::from_secs(5)).await;
        outcome?;
        assert!(
            crate::election::claim(&root).is_ok(),
            "clean shutdown releases the workspace store"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repository_http_routes_each_workspace_root() -> Result<(), Box<dyn std::error::Error>>
    {
        let _log = diagnostic_log();
        let directory = tempfile::tempdir()?;
        let authority = directory.path();
        let idle_configuration = "[server]\nidle_timeout = \"10s\"\n";
        crate::server::hermetic_workspace(authority, idle_configuration)?;
        let fixtures = [
            ("first", "amber"),
            ("second", "cedar"),
            ("third", "indigo"),
            ("fourth", "quartz"),
        ];
        let mut roots = Vec::new();
        for (directory_name, symbol) in fixtures {
            let root = authority.join(directory_name);
            std::fs::create_dir_all(&root)?;
            crate::server::hermetic_workspace(&root, idle_configuration)?;
            std::fs::write(root.join("lib.rs"), format!("pub fn {symbol}() {{}}\n"))?;
            roots.push(root);
        }
        rift_history::fixture::init(authority);
        rift_history::fixture::commit_all(authority, "add workspace sources");
        let repository = rift_history::Repository::open(authority)?;
        let common_directory = repository.common_directory().to_path_buf();
        let shutdown = CancellationToken::new();
        let server = serve_repository_http(
            authority,
            &common_directory,
            crate::validation::ConfigurationState::accept(authority).server_configuration(),
            shutdown.clone(),
            WorkspaceIndexLimits::default(),
            TokenCheck::Required,
            BuildCheckout::Unversioned,
        )
        .await?;

        let amber = repository_symbol(&server, &roots[0], "amber").await?;
        assert!(
            crate::election::claim(&roots[0]).is_err(),
            "standalone workspace server cannot share one workspace store"
        );
        let cedar = repository_symbol(&server, &roots[1], "cedar").await?;
        let indigo = repository_symbol(&server, &roots[2], "indigo").await?;
        let others_answered = tokio::time::Instant::now();
        let quartz = repository_symbol(&server, &roots[3], "quartz").await?;
        assert_eq!(amber["hits"][0]["symbol"]["name"], "amber", "{amber}");
        assert_eq!(cedar["hits"][0]["symbol"]["name"], "cedar", "{cedar}");
        assert_eq!(indigo["hits"][0]["symbol"]["name"], "indigo", "{indigo}");
        assert_eq!(quartz["hits"][0]["symbol"]["name"], "quartz", "{quartz}");
        let expired_roots = u32::try_from(roots.len() - 1)?;
        keep_one_until_others_release(&server, &roots[3], "quartz", expired_roots, others_answered)
            .await?;
        let released_lease = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(lease) = crate::election::claim(&roots[0]) {
                    break lease;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        drop(released_lease);
        assert!(crate::election::claim(&roots[3]).is_err());
        let lock = ServerLock {
            port: server.port(),
            token: server.token().to_owned(),
            pid: std::process::id(),
            identity: server.identity.clone(),
            server: None,
        };
        crate::http::request_stop(&lock)
            .await
            .expect("repository stop request must be accepted");
        assert!(
            server.stop.is_cancelled(),
            "accepted stop request cancels serving"
        );
        assert!(
            !shutdown.is_cancelled(),
            "serving stop leaves caller token alone"
        );
        let (_, stopped) = server.stopped(Duration::from_secs(20)).await;
        stopped?;
        let lease = crate::election::claim(&roots[3])?;
        drop(lease);
        Ok(())
    }

    /// Failure bound on one step; never a way to order two events.
    const STEP_MAX: Duration = Duration::from_secs(10);

    /// A workspace at `root` holding one committed source file, and its canonical root.
    fn committed_workspace(root: &Path) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
        let root = std::fs::canonicalize(root)?;
        crate::server::hermetic_workspace(&root, "")?;
        std::fs::write(root.join("lib.rs"), "pub fn amber() {}\n")?;
        rift_history::fixture::init(&root);
        rift_history::fixture::commit_all(&root, "add workspace source");
        Ok(root)
    }

    /// The registry a repository server on `root` builds, with no server around it, so no
    /// idle watch evicts a workspace on its own.
    fn unserved_registry(
        root: &Path,
    ) -> Result<super::RepositoryWorkspaceRegistry, Box<dyn std::error::Error>> {
        let server_configuration =
            crate::validation::ConfigurationState::accept(root).server_configuration();
        Ok(super::RepositoryWorkspaceRegistry {
            authority_root: root.to_path_buf(),
            common_directory: root.to_path_buf(),
            blocking: crate::server::BlockingExecutor::for_configuration(&server_configuration)?,
            idle_timeout: Duration::from_millis(server_configuration.idle_timeout.milliseconds()),
            admissions: Arc::new(tokio::sync::Semaphore::new(1)),
            workspaces_max: 1,
            server_configuration,
            limits: WorkspaceIndexLimits::default(),
            checkout: BuildCheckout::Unversioned,
            stop: CancellationToken::new(),
            build_gate: tokio::sync::Mutex::new(()),
            workspaces: tokio::sync::Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    /// A workspace whose index database was refused has no database to stop, and its stop
    /// still succeeds and releases the store lease.
    #[tokio::test]
    async fn a_workspace_without_an_index_database_stops_and_releases_its_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = committed_workspace(directory.path())?;
        let state_directory = root.join(".rift");
        std::fs::create_dir_all(rift_index::DatabaseName::Index.path(&state_directory))?;
        let registry = unserved_registry(&root)?;
        let workspace = registry
            .build_workspace(root.clone())
            .await
            .map_err(|error| format!("{error:?}"))?;
        assert!(
            workspace.database.is_none(),
            "a directory is not a database"
        );

        let deadline = tokio::time::Instant::now() + STEP_MAX;
        super::stop_repository_workspace(&workspace, deadline).await?;

        assert!(
            workspace.lease.lock().await.is_none(),
            "a stop that succeeded releases the store lease"
        );
        Ok(())
    }

    /// A stop whose vectors database is still in its first open when the deadline passes
    /// fails as a SQLite worker shutdown, and keeps the store lease. The open is polled once
    /// and never again, and the test holds the migration lock it waits on, so the open stays
    /// in flight past the deadline on every run.
    #[tokio::test]
    async fn a_workspace_stop_that_outlasts_the_vectors_first_open_keeps_the_lease()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::task::{Context, Waker};

        let directory = tempfile::tempdir()?;
        let root = committed_workspace(directory.path())?;
        let registry = unserved_registry(&root)?;
        let workspace = registry
            .build_workspace(root.clone())
            .await
            .map_err(|error| format!("{error:?}"))?;
        let supervisor_deadline = tokio::time::Instant::now() + STEP_MAX;
        workspace.supervisor.shutdown(supervisor_deadline).await?;
        let lock_path = rift_index::DatabaseName::Vectors.migration_lock_path(&root.join(".rift"));
        let migration_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        migration_lock.try_lock()?;
        let vectors = workspace
            .vectors
            .as_ref()
            .ok_or("the vectors handle exists")?;
        let mut opening = Box::pin(vectors.resolve(rift_index::DatabasePool::new(4, 60_000)));
        let first_poll =
            std::future::Future::poll(opening.as_mut(), &mut Context::from_waker(Waker::noop()));
        assert!(
            first_poll.is_pending(),
            "the held migration lock keeps the open waiting"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        let refusal = super::stop_repository_workspace(&workspace, deadline)
            .await
            .expect_err("an open in flight outlasts the stop deadline");

        assert_eq!(
            refusal.slug(),
            rift_error::errors::mcp::http_serve_failed::SLUG
        );
        let rendered = refusal.to_string();
        assert!(rendered.contains("SQLite worker shutdown"), "{rendered}");
        assert!(
            workspace.lease.lock().await.is_some(),
            "a failed stop keeps the store lease"
        );
        drop(opening);
        drop(migration_lock);
        Ok(())
    }

    /// An idle workspace whose stop fails stays retained, keeps its store lease, and
    /// records the failure. A task that never finishes is parked where the supervisor's
    /// own task runs, so the supervisor misses the stop deadline on every run; the paused
    /// clock carries the workspace past its idle deadline and the stop past its own.
    #[tokio::test]
    async fn an_idle_workspace_whose_stop_fails_stays_retained_and_records_the_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        let root = committed_workspace(directory.path())?;
        let registry = unserved_registry(&root)?;
        let cell = registry
            .workspace_cell(&root)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let workspace = cell
            .get_or_try_init(|| registry.build_workspace(root.clone()))
            .await
            .map_err(|error| format!("{error:?}"))?;
        let validation = &workspace.supervisor.validation;
        let stuck = tokio::spawn(std::future::pending::<()>());
        let supervisor_task = validation.task.lock().await.replace(stuck);
        validation.cancellation.cancel();
        if let Some(task) = supervisor_task {
            tokio::time::timeout(STEP_MAX, task).await??;
        }
        let idle_deadline = workspace
            .activity
            .idle_deadline(registry.idle_timeout)
            .ok_or("no request is active")?;
        let (sink, mut drain) = rift_tracing::log_capture();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));

        tokio::time::pause();
        tokio::time::advance(idle_deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await;
        registry.evict_idle().await;

        assert!(
            registry.workspaces.lock().await.contains_key(&root),
            "a workspace whose stop failed stays retained"
        );
        assert!(
            workspace.lease.lock().await.is_some(),
            "a workspace whose stop failed keeps its store lease"
        );
        let mut failures = Vec::new();
        let mut released = false;
        while let Ok(record) = drain.try_recv_record() {
            match record.message() {
                "idle workspace shutdown failed" => failures.push(record),
                "idle workspace released" => released = true,
                _ => {}
            }
        }
        assert!(!released, "a workspace whose stop failed is not released");
        assert_eq!(failures.len(), 1, "one failed stop records one failure");
        assert_eq!(failures[0].level(), "warn");
        assert!(
            failures[0].fields().contains("index supervisor shutdown"),
            "the record names the stage that failed: {}",
            failures[0].fields()
        );
        Ok(())
    }
}
