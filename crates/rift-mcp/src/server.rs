use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock as SyncRwLock};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use rift_core::constants::{WORKSPACE_CONFIGURATION_FILE, WORKSPACE_IGNORED_DIRECTORIES};
use rift_core::{ErrorName, Fault, SourceVisibility};
use rift_index::{WorkspaceFingerprint, WorkspaceIndexLimits, WorkspaceSourcePolicy};
use rift_protocol::change::{
    ChangeResult, ChangeSummary, GuaranteeEvidence, InsertSymbolParams, PatchParams,
    ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::configuration::{
    CommandHook, SERVER_BLOCKING_SLOTS_MAX, ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_protocol::read::{
    DiagnosticCode, GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, SearchParams,
    SearchResult,
};
use rift_server::{
    CONFIGURATION_FILE_BYTES_MAX, ChangeService, ConfigurationError, HookRun, HookStatus,
    ReadError, ReadFault, ReadService, load_configuration, run_hooks,
};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ErrorCode, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

/// JSON-RPC error code every Rift operating failure travels under: the
/// first code of the server-defined range (-32000 to -32099), which rmcp
/// exports no constant for — its constants name only MCP-defined codes. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
const ERROR_CAUSES_MAX: usize = 8;

/// Filesystem events coalesced while one rebuild is pending.
const INDEX_INVALIDATIONS_MAX: usize = 1;
/// Delay collecting one bounded filesystem-event batch.
const INDEX_DEBOUNCE: Duration = Duration::from_millis(50);
/// Deadline for one request to obtain a coherent current snapshot.
const INDEX_FRESHNESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for joining the index supervisor during shutdown.
const INDEX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Complete capture retries while the tree keeps moving.
const INDEX_CAPTURE_ATTEMPTS_MAX: usize = 3;

/// Bounded Tokio admission for blocking filesystem and parser work.
#[derive(Clone, Debug)]
struct BlockingExecutor {
    operations: Arc<Semaphore>,
    queue_timeout_ms: u64,
}

impl BlockingExecutor {
    /// Sizes the workspace's pool and queue wait from one admitted
    /// `[server]` table.
    fn for_configuration(server: &ServerConfiguration) -> Self {
        // Admission bounds the value to 1..=SERVER_BLOCKING_SLOTS_MAX, so
        // the clamp only guards the usize conversion.
        let slots = usize::try_from(server.blocking_slots.min(SERVER_BLOCKING_SLOTS_MAX))
            .unwrap_or(1)
            .max(1);
        Self {
            operations: Arc::new(Semaphore::new(slots)),
            queue_timeout_ms: server.blocking_queue_timeout.milliseconds(),
        }
    }

    /// Creates an isolated executor for deterministic capacity tests.
    #[cfg(test)]
    fn isolated(operations_max: usize, queue_timeout_ms: u64) -> Self {
        assert!(
            operations_max > 0,
            "blocking operation capacity must be positive: operations_max={operations_max}"
        );
        assert!(
            queue_timeout_ms > 0,
            "blocking queue timeout must be positive: queue_timeout_ms={queue_timeout_ms}"
        );
        Self {
            operations: Arc::new(Semaphore::new(operations_max)),
            queue_timeout_ms,
        }
    }

    /// Runs one blocking operation after queued, bounded admission.
    async fn run<Output>(
        &self,
        operation: &'static str,
        work: impl FnOnce() -> Result<Output, ReadError> + Send + 'static,
    ) -> Result<Output, ReadError>
    where
        Output: Send + 'static,
    {
        let acquire = Arc::clone(&self.operations).acquire_owned();
        let permit = tokio::time::timeout(Duration::from_millis(self.queue_timeout_ms), acquire)
            .await
            .map_err(|_| ReadFault::capacity_timeout(operation, self.queue_timeout_ms))?
            .map_err(|error| ReadFault::task(operation, error.to_string()))?;
        tokio::task::spawn_blocking(move || {
            let result = work();
            // Explicit success-path release; unwinding also drops the owned permit.
            drop(permit);
            result
        })
        .await
        .map_err(|error| ReadFault::task(operation, error.to_string()))?
    }
}

/// Serializes workspace mutations and snapshot publication.
#[derive(Debug, Default)]
struct ChangeLane {
    admission: AsyncMutex<()>,
}

impl ChangeLane {
    /// Runs one operation after FIFO admission to the workspace lane.
    fn run<Output>(&self, operation: impl FnOnce() -> Output) -> Output {
        let guard = self.admission.blocking_lock();
        let output = operation();
        // Filesystem mutation and snapshot publication finish before release.
        drop(guard);
        output
    }

    /// Verifies occupied admission, reports it, then uses the production lane.
    #[cfg(test)]
    fn run_after_contention<Output>(
        &self,
        on_contention: impl FnOnce(),
        operation: impl FnOnce() -> Output,
    ) -> Output {
        assert!(
            self.admission.try_lock().is_err(),
            "contention witness requires an occupied change lane"
        );
        on_contention();
        self.run(operation)
    }
}

/// Rust workspace MCP server: reads serve an immutable snapshot, changes
/// write the workspace and swap in a fresh snapshot.
#[derive(Debug)]
pub struct RiftMcp {
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<IndexState>>,
    coherence: Arc<IndexCoherence>,
    changes: Arc<ChangeService>,
    change_lane: Arc<ChangeLane>,
    blocking: BlockingExecutor,
    tool_router: ToolRouter<Self>,
}

impl Drop for RiftMcp {
    fn drop(&mut self) {
        self.coherence.cancellation.cancel();
    }
}

/// Read index and configuration policy published as one immutable value.
#[derive(Debug)]
struct PublishedWorkspace {
    reads: Arc<ReadService>,
    configuration: ConfigurationState,
    fingerprint: WorkspaceFingerprint,
    source_policy: Arc<WorkspaceSourcePolicy>,
    epoch: u64,
}

/// Published workspace plus failure for latest observed epoch.
#[derive(Debug)]
struct IndexState {
    current: Arc<PublishedWorkspace>,
    failure: Option<(u64, Arc<ReadError>)>,
}

impl IndexState {
    /// Clones one coherent publication and its latest failure.
    fn snapshot(&self) -> (Arc<PublishedWorkspace>, Option<(u64, Arc<ReadError>)>) {
        (Arc::clone(&self.current), self.failure.clone())
    }

    /// Publishes candidate only while its observation remains current.
    fn publish(&mut self, candidate: Arc<PublishedWorkspace>, observed_epoch: u64) -> bool {
        if candidate.epoch != observed_epoch {
            return false;
        }
        self.current = candidate;
        self.failure = None;
        true
    }

    /// Records failure only while its observation remains current.
    fn record_failure(&mut self, epoch: u64, observed_epoch: u64, error: ReadError) -> bool {
        if epoch != observed_epoch {
            return false;
        }
        self.failure = Some((epoch, Arc::new(error)));
        true
    }
}

/// Filesystem observation and supervisor ownership shared with handlers.
#[derive(Debug)]
struct IndexCoherence {
    observed_epoch: Arc<AtomicU64>,
    watch_failed: Arc<AtomicBool>,
    invalidations: mpsc::Sender<()>,
    changed: Arc<Notify>,
    publication_lane: SyncMutex<()>,
    source_policy: SyncRwLock<Option<Arc<WorkspaceSourcePolicy>>>,
    cancellation: CancellationToken,
    task: AsyncMutex<Option<JoinHandle<()>>>,
}

/// Owned shutdown handle for the workspace index supervisor.
#[derive(Debug, Clone)]
pub(crate) struct IndexSupervisor {
    coherence: Arc<IndexCoherence>,
}

/// Rebuild dependencies owned by index supervisor task.
struct IndexSupervisorContext {
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<IndexState>>,
    change_lane: Arc<ChangeLane>,
    coherence: Arc<IndexCoherence>,
    blocking: BlockingExecutor,
}

/// The last admission of the workspace's `rift.toml`, kept with the file
/// state it was read from so an edited file is re-admitted on the next
/// request and an unchanged one is not re-parsed per call.
#[derive(Debug, Clone)]
struct ConfigurationState {
    admitted: Result<WorkspaceConfiguration, Arc<ConfigurationError>>,
    fingerprint: ConfigurationFingerprint,
}

impl ConfigurationState {
    /// Admits the workspace's current `rift.toml`.
    fn admit(root: &Path) -> Self {
        let fingerprint = configuration_fingerprint(root);
        Self {
            admitted: load_configuration(root).map_err(Arc::new),
            fingerprint,
        }
    }

    /// The admission's outcome as one request sees it: the configuration to
    /// serve under, or the typed refusal naming what to fix.
    fn admitted(&self, phase: wire::ErrorPhase) -> Result<WorkspaceConfiguration, ErrorData> {
        match &self.admitted {
            Ok(configuration) => Ok(configuration.clone()),
            Err(error) => Err(error.tool_error(phase)),
        }
    }

    /// The `[server]` table from the last admission, or the default table
    /// while `rift.toml` is invalid.
    fn server_configuration(&self) -> ServerConfiguration {
        self.admitted
            .as_ref()
            .map(|configuration| configuration.server.clone())
            .unwrap_or_default()
    }

    /// The `[source]` policy from the last admission, or the default policy
    /// while `rift.toml` is invalid.
    fn source_visibility(&self) -> SourceVisibility {
        self.admitted.as_ref().map_or_else(
            |_| SourceVisibility::default(),
            |configuration| SourceVisibility::from(&configuration.source),
        )
    }
}

/// Exact bounded identity of the configuration policy source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigurationFingerprint {
    /// No readable configuration file exists.
    MissingOrUnreadable,
    /// File bytes within the admitted bound.
    Content([u8; 32]),
    /// File is already invalid by size; its contents cannot change policy.
    Oversized(u64),
}

/// The current `rift.toml` file state, or null when the file is absent or
/// unreadable — either way the next admission decides what that means.
fn configuration_fingerprint(root: &Path) -> ConfigurationFingerprint {
    let path = root.join(WORKSPACE_CONFIGURATION_FILE);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return ConfigurationFingerprint::MissingOrUnreadable;
    };
    if metadata.len() > CONFIGURATION_FILE_BYTES_MAX {
        return ConfigurationFingerprint::Oversized(metadata.len());
    }
    let Ok(file) = std::fs::File::open(path) else {
        return ConfigurationFingerprint::MissingOrUnreadable;
    };
    let mut raw = Vec::new();
    if file
        .take(CONFIGURATION_FILE_BYTES_MAX + 1)
        .read_to_end(&mut raw)
        .is_err()
    {
        return ConfigurationFingerprint::MissingOrUnreadable;
    }
    if raw.len() as u64 > CONFIGURATION_FILE_BYTES_MAX {
        return ConfigurationFingerprint::Oversized(raw.len() as u64);
    }
    ConfigurationFingerprint::Content(Sha256::digest(raw).into())
}

impl IndexCoherence {
    /// Creates one bounded invalidation stream and its receiver.
    fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (invalidations, receiver) = mpsc::channel(INDEX_INVALIDATIONS_MAX);
        (
            Arc::new(Self {
                observed_epoch: Arc::new(AtomicU64::new(0)),
                watch_failed: Arc::new(AtomicBool::new(false)),
                invalidations,
                changed: Arc::new(Notify::new()),
                publication_lane: SyncMutex::new(()),
                source_policy: SyncRwLock::new(None),
                cancellation: CancellationToken::new(),
                task: AsyncMutex::new(None),
            }),
            receiver,
        )
    }

    /// Records one invalidation before coalescing its rebuild signal.
    fn observe(&self) -> Result<u64, ReadError> {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self.observe_locked();
        drop(publication);
        result
    }

    /// Marks watcher unhealthy and records invalidation in one critical section.
    fn observe_watch_failure(&self) -> Result<u64, ReadError> {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.watch_failed.store(true, Ordering::Release);
        let result = self.observe_locked();
        drop(publication);
        result
    }

    /// Records one invalidation while caller owns publication lane.
    fn observe_locked(&self) -> Result<u64, ReadError> {
        let previous = self
            .observed_epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            })
            .map_err(|_| {
                self.watch_failed.store(true, Ordering::Release);
                ReadFault::unavailable("index observation", "filesystem event epoch exhausted")
            })?;
        let epoch = previous + 1;
        match self.invalidations.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.watch_failed.store(true, Ordering::Release);
                return Err(ReadFault::unavailable(
                    "index observation",
                    "index supervisor is not running",
                ));
            }
        }
        Ok(epoch)
    }

    /// Returns latest filesystem-event epoch.
    fn observed_epoch(&self) -> u64 {
        self.observed_epoch.load(Ordering::SeqCst)
    }

    /// Installs event admission policy under publication linearization.
    #[cfg(test)]
    fn install_source_policy(&self, policy: Arc<WorkspaceSourcePolicy>) {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.replace_source_policy_locked(policy);
        drop(publication);
    }

    /// Replaces event admission policy while caller owns publication lane.
    fn replace_source_policy_locked(&self, policy: Arc<WorkspaceSourcePolicy>) {
        let mut current = self
            .source_policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Some(policy);
    }

    /// Filters and observes event within same publication critical section.
    fn observe_event(&self, root: &Path, event: &Event) -> Result<Option<u64>, ReadError> {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = if relevant_watch_event(root, self, event) {
            self.observe_locked().map(Some)
        } else {
            Ok(None)
        };
        drop(publication);
        result
    }

    /// Returns whether current policy admits one source event path.
    fn source_path_is_relevant(&self, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.as_ref().is_none_or(|policy| policy.admits(path))
    }

    /// Returns whether current policy can admit source below one directory.
    fn source_directory_is_relevant(&self, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current
            .as_ref()
            .is_none_or(|policy| policy.may_admit_descendant(path))
    }

    /// Returns whether path is current root workspace configuration.
    fn workspace_configuration_is_relevant(&self, root: &Path, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.as_ref().map_or_else(
            || path == root.join(WORKSPACE_CONFIGURATION_FILE),
            |policy| policy.is_workspace_configuration(path),
        )
    }
}

impl Drop for IndexCoherence {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl IndexSupervisor {
    /// Cancels and joins the workspace index supervisor.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the task panics or misses its shutdown deadline.
    ///
    /// # Cancel safety
    ///
    /// Cancellation is requested before the join begins. Dropping this future
    /// after it takes task ownership detaches that terminating task.
    pub(crate) async fn shutdown(&self) -> Result<(), ReadError> {
        self.coherence.cancellation.cancel();
        let Some(mut task) = self.coherence.task.lock().await.take() else {
            return Ok(());
        };
        if let Ok(result) = tokio::time::timeout(INDEX_SHUTDOWN_TIMEOUT, &mut task).await {
            result.map_err(|error| ReadFault::task("index supervisor shutdown", error.to_string()))
        } else {
            task.abort();
            let _ = task.await;
            Err(ReadFault::unavailable(
                "index supervisor shutdown",
                "shutdown deadline elapsed",
            ))
        }
    }
}

/// Creates one native watcher rooted before the initial index scan.
fn workspace_watcher(
    root: &Path,
    coherence: &Arc<IndexCoherence>,
) -> Result<notify::RecommendedWatcher, ReadError> {
    let watched_root = std::fs::canonicalize(root)
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    let event_root = watched_root.clone();
    let coherence = Arc::clone(coherence);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        if let Ok(event) = result {
            if coherence.observe_event(&event_root, &event).is_err() {
                tracing::error!(
                    component = "index",
                    operation = "watch.observe",
                    "index watch failed"
                );
            }
        } else {
            let _ = coherence.observe_watch_failure();
            tracing::warn!(
                component = "index",
                operation = "watch.receive",
                "index watch backend reported failure"
            );
        }
    })
    .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    watcher
        .watch(&watched_root, RecursiveMode::Recursive)
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    Ok(watcher)
}

/// Whether one native event can change visible Rust source or its policy.
fn relevant_watch_event(root: &Path, coherence: &IndexCoherence, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .filter(|path| hard_floor_admits_watch_path(root, path))
        .any(|path| watch_kind_reaches_path(root, coherence, event.kind, path))
}

/// Rejects paths below Rift's hard-floor directories.
fn hard_floor_admits_watch_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    !relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| WORKSPACE_IGNORED_DIRECTORIES.contains(&name))
    })
}

/// Applies event-kind filtering without trusting editor-specific event shapes.
fn watch_kind_reaches_path(
    root: &Path,
    coherence: &IndexCoherence,
    kind: EventKind,
    path: &Path,
) -> bool {
    let workspace_configuration = coherence.workspace_configuration_is_relevant(root, path);
    let gitignore = path.file_name() == Some(std::ffi::OsStr::new(".gitignore"))
        && path
            .parent()
            .is_some_and(|parent| coherence.source_directory_is_relevant(parent));
    let policy_file = workspace_configuration || gitignore;
    let source_file = coherence.source_path_is_relevant(path);
    let directory_event = matches!(
        kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
    ) && coherence.source_directory_is_relevant(path);
    let possible_directory =
        path.extension().is_none() && coherence.source_directory_is_relevant(path);
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => {
            policy_file || source_file || directory_event
        }
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any | EventKind::Other => {
            policy_file || source_file || possible_directory
        }
        EventKind::Modify(_) => policy_file || source_file,
        EventKind::Access(_) => false,
    }
}

/// Builds the first snapshot while rejecting concurrent filesystem movement.
async fn initial_workspace(
    root: &Path,
    limits: WorkspaceIndexLimits,
    coherence: &IndexCoherence,
    blocking: &BlockingExecutor,
) -> Result<Arc<PublishedWorkspace>, ReadError> {
    for attempt in 1..=INDEX_CAPTURE_ATTEMPTS_MAX {
        let epoch = coherence.observed_epoch();
        let build_root = root.to_path_buf();
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            trigger = "startup",
            epoch,
            attempt
        );
        let built = blocking
            .run("initial index build", move || {
                build_workspace_candidate(&build_root, limits, epoch)
            })
            .instrument(span)
            .await?;
        let WorkspaceCandidate::Stable(built) = built else {
            continue;
        };
        let stable_epoch = coherence.observed_epoch() == epoch;
        let watch_healthy = !coherence.watch_failed.load(Ordering::Acquire);
        if stable_epoch && watch_healthy {
            let publication = coherence
                .publication_lane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if coherence.observed_epoch() != epoch || coherence.watch_failed.load(Ordering::Acquire)
            {
                drop(publication);
                continue;
            }
            coherence.replace_source_policy_locked(Arc::clone(&built.source_policy));
            drop(publication);
            tracing::info!(
                component = "index",
                operation = "index.publish",
                trigger = "startup",
                epoch,
                "index snapshot published"
            );
            return Ok(built);
        }
    }
    Err(ReadFault::unavailable(
        "initial index build",
        "workspace kept changing across bounded capture attempts",
    ))
}

/// Result of one bounded workspace candidate capture.
enum WorkspaceCandidate {
    /// Index, configuration, and source policy share one stable capture.
    Stable(Arc<PublishedWorkspace>),
    /// Configuration moved during capture.
    ConfigurationChanged,
}

/// Builds one snapshot candidate and verifies configuration around its scan.
fn build_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    epoch: u64,
) -> Result<WorkspaceCandidate, ReadError> {
    let configuration = ConfigurationState::admit(root);
    let visibility = configuration.source_visibility();
    let reads = ReadService::build(root, limits, &visibility)?;
    let source_policy = WorkspaceSourcePolicy::build(root, limits, &visibility)
        .map_err(|error| ReadError::from(ReadFault::Index(error)))?;
    if configuration.fingerprint != configuration_fingerprint(root) {
        return Ok(WorkspaceCandidate::ConfigurationChanged);
    }
    Ok(WorkspaceCandidate::Stable(Arc::new(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy: Arc::new(source_policy),
        epoch,
    })))
}

/// Outcome of one background reconciliation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebuildOutcome {
    /// Candidate became current and was published.
    Published,
    /// New observation invalidated candidate before publication.
    Superseded,
}

/// Owns native watcher and reconciles coalesced invalidations until shutdown.
async fn run_index_supervisor(
    _watcher: notify::RecommendedWatcher,
    mut invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
) {
    let IndexSupervisorContext {
        root,
        limits,
        published,
        change_lane,
        coherence,
        blocking,
    } = context;
    loop {
        let received = tokio::select! {
            () = coherence.cancellation.cancelled() => false,
            received = invalidations.recv() => received.is_some(),
        };
        if !received {
            return;
        }
        tokio::select! {
            () = coherence.cancellation.cancelled() => return,
            () = tokio::time::sleep(INDEX_DEBOUNCE) => {}
        }
        let epoch = coherence.observed_epoch();
        tracing::debug!(
            component = "index",
            operation = "watch.batch",
            epoch,
            "filesystem invalidations coalesced"
        );
        let result = rebuild_workspace(
            root.clone(),
            limits,
            Arc::clone(&published),
            Arc::clone(&change_lane),
            Arc::clone(&coherence),
            blocking.clone(),
            epoch,
        )
        .instrument(tracing::info_span!(
            "index.build",
            component = "index",
            trigger = "filesystem",
            epoch
        ))
        .await;
        if let Err(error) = result {
            tracing::warn!(
                component = "index",
                operation = "index.build",
                epoch,
                error_code = error.descriptor().code(),
                "index rebuild failed"
            );
            let failed_state = Arc::clone(&published);
            let failed_coherence = Arc::clone(&coherence);
            let recorded = blocking
                .run("index failure publication", move || {
                    Ok(record_rebuild_failure(
                        &failed_state,
                        &failed_coherence,
                        epoch,
                        error,
                    ))
                })
                .await;
            if recorded.is_err() {
                let _ = coherence.observe_watch_failure();
            }
            coherence.changed.notify_waiters();
        }
    }
}

/// Rebuilds and atomically publishes only a still-current candidate.
async fn rebuild_workspace(
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<IndexState>>,
    change_lane: Arc<ChangeLane>,
    coherence: Arc<IndexCoherence>,
    blocking: BlockingExecutor,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    blocking
        .run("filesystem index rebuild", move || {
            rebuild_workspace_blocking(&root, limits, &published, &change_lane, &coherence, epoch)
        })
        .await
}

/// Runs one serialized filesystem rebuild on blocking executor.
fn rebuild_workspace_blocking(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    change_lane: &ChangeLane,
    coherence: &IndexCoherence,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    change_lane.run(|| rebuild_workspace_serialized(root, limits, published, coherence, epoch))
}

/// Rebuilds workspace while mutation lane is held.
fn rebuild_workspace_serialized(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    coherence: &IndexCoherence,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    if !admit_rebuild(coherence, epoch)? {
        return Ok(RebuildOutcome::Superseded);
    }
    let candidate = build_workspace_candidate(root, limits, epoch)?;
    let WorkspaceCandidate::Stable(candidate) = candidate else {
        let _ = coherence.observe();
        return Ok(RebuildOutcome::Superseded);
    };
    let outcome = publish_rebuild(published, coherence, candidate);
    if outcome == RebuildOutcome::Published {
        trace_publication(epoch);
        coherence.changed.notify_waiters();
    }
    Ok(outcome)
}

/// Refuses rebuild when watcher failed or candidate epoch already moved.
fn admit_rebuild(coherence: &IndexCoherence, epoch: u64) -> Result<bool, ReadError> {
    if coherence.watch_failed.load(Ordering::Acquire) {
        return Err(ReadFault::unavailable(
            "filesystem index rebuild",
            "filesystem watcher failed",
        ));
    }
    Ok(coherence.observed_epoch() == epoch)
}

/// Atomically publishes candidate when observation still matches.
fn publish_rebuild(
    published: &RwLock<IndexState>,
    coherence: &IndexCoherence,
    candidate: Arc<PublishedWorkspace>,
) -> RebuildOutcome {
    publish_rebuild_after(published, coherence, candidate, || {})
}

/// Publishes under observation lane; hook enables deterministic overlap tests.
fn publish_rebuild_after(
    published: &RwLock<IndexState>,
    coherence: &IndexCoherence,
    candidate: Arc<PublishedWorkspace>,
    after_state_lock: impl FnOnce(),
) -> RebuildOutcome {
    let publication = coherence
        .publication_lane
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = published.blocking_write();
    after_state_lock();
    let observed_epoch = coherence.observed_epoch();
    if candidate.epoch != observed_epoch {
        drop(state);
        drop(publication);
        return RebuildOutcome::Superseded;
    }
    coherence.replace_source_policy_locked(Arc::clone(&candidate.source_policy));
    let published = state.publish(candidate, observed_epoch);
    drop(state);
    drop(publication);
    if published {
        RebuildOutcome::Published
    } else {
        RebuildOutcome::Superseded
    }
}

/// Records failure under same observation linearization as publication.
fn record_rebuild_failure(
    published: &RwLock<IndexState>,
    coherence: &IndexCoherence,
    epoch: u64,
    error: ReadError,
) -> bool {
    let publication = coherence
        .publication_lane
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let observed_epoch = coherence.observed_epoch();
    let recorded = published
        .blocking_write()
        .record_failure(epoch, observed_epoch, error);
    drop(publication);
    recorded
}

/// Emits one path-free filesystem publication event.
fn trace_publication(epoch: u64) {
    tracing::info!(
        component = "index",
        operation = "index.publish",
        trigger = "filesystem",
        epoch,
        "index snapshot published"
    );
}

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl RiftMcp {
    /// Builds server from one direct-workspace snapshot, applying the
    /// admitted `rift.toml`'s `[source]` policy to the initial index and its
    /// `[server]` table to the blocking pool. While `rift.toml` is invalid,
    /// the initial index still builds under the default policies; every
    /// request then fails as `configuration_invalid` until the file is
    /// fixed.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when workspace cannot be indexed within bounds.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future discards construction. An admitted blocking scan
    /// finishes in the bounded executor before releasing its capacity permit.
    pub async fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        let root = root.to_path_buf();
        let admission_root = root.clone();
        let startup_configuration =
            tokio::task::spawn_blocking(move || ConfigurationState::admit(&admission_root))
                .await
                .map_err(|error| ReadFault::task("configuration admission", error.to_string()))?;
        let blocking =
            BlockingExecutor::for_configuration(&startup_configuration.server_configuration());
        let (coherence, invalidations) = IndexCoherence::new();
        let watch_root = root.clone();
        let watch_coherence = Arc::clone(&coherence);
        let watcher = blocking
            .run("workspace watch setup", move || {
                workspace_watcher(&watch_root, &watch_coherence)
            })
            .instrument(tracing::info_span!(
                "index.watch",
                component = "index",
                operation = "watch.setup"
            ))
            .await?;
        let published = initial_workspace(&root, limits, &coherence, &blocking).await?;
        let published = Arc::new(RwLock::new(IndexState {
            current: published,
            failure: None,
        }));
        let change_lane = Arc::new(ChangeLane::default());
        let supervisor_task = tokio::spawn(run_index_supervisor(
            watcher,
            invalidations,
            IndexSupervisorContext {
                root: root.clone(),
                limits,
                published: Arc::clone(&published),
                change_lane: Arc::clone(&change_lane),
                coherence: Arc::clone(&coherence),
                blocking: blocking.clone(),
            },
        ));
        let mut task = coherence.task.lock().await;
        *task = Some(supervisor_task);
        drop(task);
        Ok(Self {
            root: root.clone(),
            limits,
            published,
            coherence,
            changes: Arc::new(ChangeService::new(&root)),
            change_lane,
            blocking,
            tool_router: Self::tool_router(),
        })
    }

    /// Returns owned supervisor shutdown access for transport adapters.
    pub(crate) fn index_supervisor(&self) -> IndexSupervisor {
        IndexSupervisor {
            coherence: Arc::clone(&self.coherence),
        }
    }

    /// Finds Rust declarations and their source by exact symbol name. Each hit
    /// carries the declaration and its source excerpt; `include_body: false` omits
    /// both. `rev` serves the lookup from a version-control revision instead of
    /// the current tree. Use `search` when the name is not exactly known.
    #[tool]
    async fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<Json<GetSymbolResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.get_symbol(&params))
            .await
    }

    /// Searches indexed Rust declarations and source lines by lexical `query`.
    /// `rev` searches a version-control revision instead of the current tree.
    /// Use `get_symbol` when the declaration name is known.
    #[tool]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.search(&params)).await
    }

    /// Lists the syntax nodes covering one UTF-8 byte position in one file,
    /// outermost first. Each identity carries a witness, so an address taken
    /// from this listing refuses cleanly once the file's bytes drift. `rev`
    /// lists the nodes as of a version-control revision instead of the
    /// current tree.
    #[tool]
    async fn nodes(
        &self,
        Parameters(params): Parameters<NodesParams>,
    ) -> Result<Json<NodesResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.nodes(params)).await
    }

    /// Replaces one declaration addressed by symbol. The whole declaration
    /// includes its attached outer attributes and doc comments. The parser
    /// derives the span, so the caller supplies no offsets; a refusal
    /// names the failed precondition and leaves the workspace untouched.
    #[tool]
    async fn replace_symbol(
        &self,
        Parameters(params): Parameters<ReplaceSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.replace_symbol(reads, &params))
            .await
    }

    /// Inserts a new declaration beside an anchor symbol, or content at a file
    /// target. Anchored insertions land beside the anchor's whole declaration,
    /// its attached outer attributes and doc comments included. A file target
    /// lands the body verbatim at the file's start or end, creating it first
    /// when `create_missing` is set and it is missing. A refusal names the
    /// failed precondition and leaves the workspace untouched.
    #[tool]
    async fn insert_symbol(
        &self,
        Parameters(params): Parameters<InsertSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.insert_symbol(reads, &params))
            .await
    }

    /// Replaces one syntax node through a witnessed address from `nodes`.
    /// The server recomputes the witness before writing and refuses when the
    /// bytes drifted, so a stale address never splices into moved code.
    #[tool]
    async fn replace_node(
        &self,
        Parameters(params): Parameters<ReplaceNodeParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.replace_node(reads, &params))
            .await
    }

    /// Applies unified-diff hunks to workspace files atomically. Hunk
    /// context guards the change; header line numbers are hints, as with
    /// `git apply`. A `/dev/null` header creates or deletes the file.
    #[tool]
    async fn patch(
        &self,
        Parameters(params): Parameters<PatchParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.patch(reads, &params))
            .await
    }

    /// Runs one read against the tree the request names — the current
    /// snapshot, or a snapshot built at the request's version-control
    /// revision — behind the admission gate every request passes.
    ///
    /// A revision snapshot is built per request from the workspace's git
    /// objects, under the same `[source]` policy and bounds as the current
    /// one; `[providers.history] enabled = false` refuses it.
    async fn read_at<Answer>(
        &self,
        rev: Option<rift_protocol::read::RevisionId>,
        operation: impl FnOnce(&ReadService) -> Result<Answer, ReadError> + Send + 'static,
    ) -> Result<Json<Answer>, ErrorData>
    where
        Answer: Send + 'static,
    {
        let published = self.published_workspace(wire::ErrorPhase::Read).await?;
        let configuration = published.configuration.admitted(wire::ErrorPhase::Read)?;
        let read_error = |error: ReadError| error.tool_error(wire::ErrorPhase::Read);
        let Some(rev) = rev else {
            let reads = Arc::clone(&published.reads);
            return self
                .blocking
                .run("current workspace read", move || operation(&reads))
                .await
                .map(Json)
                .map_err(read_error);
        };
        if !configuration.providers.history.enabled {
            return Err(read_error(ReadError::from(ReadFault::Unsupported {
                capability: "revision reads (providers.history disabled)",
            })));
        }
        let visibility = SourceVisibility::from(&configuration.source);
        let root = self.root.clone();
        let limits = self.limits;
        self.blocking
            .run("revision workspace read", move || {
                let reads = ReadService::at_revision(&root, &rev, limits, &visibility)?;
                operation(&reads)
            })
            .await
            .map(Json)
            .map_err(read_error)
    }

    /// Returns one atomically published index and configuration policy.
    async fn published_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        match tokio::time::timeout(INDEX_FRESHNESS_TIMEOUT, self.reconcile_workspace(phase)).await {
            Ok(result) => result,
            Err(_) => Err(ReadFault::unavailable(
                "current workspace read",
                "index freshness deadline elapsed",
            )
            .tool_error(phase)),
        }
    }

    /// Reconciles native observations with an exact request-time fingerprint.
    async fn reconcile_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        for _attempt in 0..INDEX_CAPTURE_ATTEMPTS_MAX {
            let current = self.await_current_workspace(phase).await?;
            let root = self.root.clone();
            let limits = self.limits;
            let visibility = current.configuration.source_visibility();
            let capture = self
                .blocking
                .run("workspace fingerprint", move || {
                    let fingerprint = WorkspaceFingerprint::capture(&root, limits, &visibility)
                        .map_err(|error| ReadError::from(ReadFault::Index(error)))?;
                    Ok((fingerprint, configuration_fingerprint(&root)))
                })
                .instrument(tracing::debug_span!(
                    "index.reconcile",
                    component = "index",
                    operation = "fingerprint.capture",
                    epoch = current.epoch
                ))
                .await;
            let (fingerprint, configuration_fingerprint) = match capture {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = self.coherence.observe();
                    return Err(error.tool_error(phase));
                }
            };
            let configuration_matches =
                current.configuration.fingerprint == configuration_fingerprint;
            let epoch_matches = current.epoch == self.coherence.observed_epoch();
            if fingerprint == current.fingerprint && configuration_matches && epoch_matches {
                current.configuration.admitted(phase)?;
                return Ok(current);
            }
            self.coherence
                .observe()
                .map_err(|error| error.tool_error(phase))?;
        }
        Err(ReadFault::unavailable(
            "current workspace read",
            "workspace changed across bounded reconciliation attempts",
        )
        .tool_error(phase))
    }

    /// Waits until published and observed epochs agree or latest build failed.
    async fn await_current_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        loop {
            let changed = self.coherence.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let observed_epoch = self.coherence.observed_epoch();
            let state = self.published.read().await;
            let (current, failure) = state.snapshot();
            drop(state);
            if self.coherence.watch_failed.load(Ordering::Acquire) {
                return Err(ReadFault::unavailable(
                    "current workspace read",
                    "filesystem watcher failed",
                )
                .tool_error(phase));
            }
            if current.epoch == observed_epoch {
                return Ok(current);
            }
            if let Some((failed_epoch, error)) = failure
                && failed_epoch == observed_epoch
            {
                return Err(error.tool_error(phase));
            }
            changed.as_mut().await;
        }
    }

    /// Runs one change against the current snapshot and, when it lands,
    /// runs the workspace's hooks in the changed tree and swaps in a
    /// snapshot of the changed workspace.
    ///
    /// Hooks observe an already-applied change: their verdicts ride the
    /// result and never roll the change back. The snapshot is rebuilt after
    /// they ran, so reads also serve whatever a hook wrote into the tree,
    /// under the `[source]` policy this call already admitted. A rebuild
    /// failure after a landed change rides the result as a diagnostic
    /// rather than failing the call: the write happened, and the caller
    /// must not be told otherwise.
    ///
    /// Dropping this future after blocking work starts does not cancel that
    /// work. The serialized operation finishes through snapshot publication
    /// before releasing its lane.
    async fn change(
        &self,
        operation: impl FnOnce(&ReadService, &ChangeService) -> Result<ChangeResult, ReadError>
        + Send
        + 'static,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.published_workspace(wire::ErrorPhase::Change).await?;
        let root = self.root.clone();
        let limits = self.limits;
        let published = Arc::clone(&self.published);
        let coherence = Arc::clone(&self.coherence);
        let changes = Arc::clone(&self.changes);
        let change_lane = Arc::clone(&self.change_lane);
        self.blocking
            .run("workspace change", move || {
                change_lane.run(|| {
                    Self::change_serialized(
                        &root, limits, &published, &coherence, &changes, operation,
                    )
                })
            })
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?
    }

    /// Runs one already-serialized change through coherent publication.
    fn change_serialized(
        root: &Path,
        limits: WorkspaceIndexLimits,
        published: &RwLock<IndexState>,
        coherence: &IndexCoherence,
        changes: &ChangeService,
        operation: impl FnOnce(&ReadService, &ChangeService) -> Result<ChangeResult, ReadError>,
    ) -> Result<Result<Json<ChangeResult>, ErrorData>, ReadError> {
        let state = published.blocking_read();
        let (current, _) = state.snapshot();
        drop(state);
        if current.epoch != coherence.observed_epoch() {
            return Ok(Err(ReadFault::unavailable(
                "workspace change",
                "index changed before operation admission",
            )
            .tool_error(wire::ErrorPhase::Change)));
        }
        let configuration = match current.configuration.admitted(wire::ErrorPhase::Change) {
            Ok(configuration) => configuration,
            Err(error) => return Ok(Err(error)),
        };
        let mut result = operation(&current.reads, changes)?;
        if let ChangeResult::Applied { summary } = &mut result {
            let epoch = match coherence.observe() {
                Ok(epoch) => epoch,
                Err(error) => {
                    summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                    return Ok(Ok(Json(result)));
                }
            };
            Self::attach_hook_verdicts(root, &configuration.hooks, summary);
            let visibility = SourceVisibility::from(&configuration.source);
            match ReadService::build(root, limits, &visibility) {
                Ok(rebuilt) => {
                    let fingerprint = rebuilt.workspace_fingerprint().clone();
                    let source_policy =
                        match WorkspaceSourcePolicy::build(root, limits, &visibility) {
                            Ok(policy) => Arc::new(policy),
                            Err(error) => {
                                let error = ReadError::from(ReadFault::Index(error));
                                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                                return Ok(Ok(Json(result)));
                            }
                        };
                    if current.configuration.fingerprint != configuration_fingerprint(root) {
                        let error = ReadFault::unavailable(
                            "workspace change",
                            "configuration changed during snapshot rebuild",
                        );
                        let _ = coherence.observe();
                        summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                        return Ok(Ok(Json(result)));
                    }
                    let next = Arc::new(PublishedWorkspace {
                        reads: Arc::new(rebuilt),
                        configuration: current.configuration.clone(),
                        fingerprint,
                        source_policy,
                        epoch,
                    });
                    if publish_rebuild(published, coherence, next) == RebuildOutcome::Published {
                        tracing::info!(
                            component = "index",
                            operation = "index.publish",
                            trigger = "rift_change",
                            epoch,
                            "index snapshot published"
                        );
                        coherence.changed.notify_waiters();
                    }
                }
                Err(error) => summary.diagnostics.push(stale_snapshot_diagnostic(&error)),
            }
        }
        Ok(Ok(Json(result)))
    }

    /// Runs the configured hooks over one applied change and attaches what
    /// they established: a passing hook's configured guarantees become
    /// evidence, and every other outcome becomes an error finding.
    fn attach_hook_verdicts(root: &Path, hooks: &[CommandHook], summary: &mut ChangeSummary) {
        if hooks.is_empty() {
            return;
        }
        let runs = run_hooks(hooks, root, &summary.paths);
        for (hook, run) in hooks.iter().zip(&runs) {
            if run.status == HookStatus::Passed {
                summary
                    .guarantees
                    .extend(hook.guarantees.iter().map(|guarantee| GuaranteeEvidence {
                        kind: guarantee.kind,
                        scope: guarantee.scope.clone(),
                        hook: hook.id.clone(),
                        detail: guarantee.detail.clone(),
                    }));
            } else {
                summary.diagnostics.push(hook_failure_diagnostic(hook, run));
            }
        }
    }
}

/// Bytes of each captured hook stream a failure finding quotes. The finding
/// also states the full sizes, so a truncated quote stays distinguishable
/// from a short log.
const HOOK_FINDING_STREAM_BYTES_MAX: usize = 1_024;

/// The finding an applied change carries for one hook that did not pass:
/// what ended the run, then each non-empty stream's size and bounded quote.
fn hook_failure_diagnostic(hook: &CommandHook, run: &HookRun) -> rift_protocol::read::Diagnostic {
    let account = match &run.status {
        HookStatus::Passed => unreachable!(
            "a passing hook contributes guarantees, not findings: hook={:?}",
            hook.id
        ),
        HookStatus::Failed => match run.exit_code {
            Some(code) => format!("exited {code}"),
            None => "exited nonzero".to_owned(),
        },
        HookStatus::TimedOut => format!("killed after {}ms", hook.timeout_ms),
        HookStatus::Error(message) => message.clone(),
    };
    let mut message = format!("hook {} did not pass: {account}", hook.id);
    for (stream_name, stream) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        if stream.total_bytes == 0 {
            continue;
        }
        let quoted = bounded_prefix(&stream.text, HOOK_FINDING_STREAM_BYTES_MAX);
        let _ = write!(
            message,
            "; {stream_name} ({} of {} bytes): {quoted}",
            quoted.len(),
            stream.total_bytes,
        );
    }
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Error,
        code: Some(DiagnosticCode::HookFailed.code()),
        message,
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

/// The longest prefix of `text` within `bytes_max` that ends on a character
/// boundary. The walk back is bounded by UTF-8 itself: at most three steps.
fn bounded_prefix(text: &str, bytes_max: usize) -> &str {
    if text.len() <= bytes_max {
        return text;
    }
    let mut end = bytes_max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Finding carried when follow-up snapshot cannot rebuild. Current-tree
/// reads refuse that dirty epoch until one can.
fn stale_snapshot_diagnostic(error: &ReadError) -> rift_protocol::read::Diagnostic {
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Warning,
        code: Some(DiagnosticCode::SnapshotStale.code()),
        message: format!(
            "the change landed, and the read snapshot could not refresh; \
             current-tree reads wait for a successful workspace reindex: {error}"
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RiftMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read and edit the current workspace: get_symbol and search find \
                 declarations, nodes lists witnessed syntax nodes at a byte position, \
                 and replace_symbol, insert_symbol, replace_node, and patch change \
                 code atomically behind verified preconditions.",
            )
    }
}

/// Boundary view of a read failure: the projection a tool handler serves as
/// the JSON-RPC error object the design documents — code `-32000`, the
/// rendered failure line as `message`, and the typed [`wire::ErrorData`] as
/// `data`.
trait WireFailure {
    /// The JSON-RPC error object for this failure, naming the phase it
    /// stopped in.
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData;

    /// The typed wire payload for this failure.
    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData;

    /// The failure's source chain as bounded `causes` entries, outermost
    /// first. Each level inherits the outer classification, which the read
    /// error already resolved through the concrete failure it wraps.
    fn wire_causes(&self) -> Vec<wire::ErrorCause>;
}

impl<K: Fault> WireFailure for rift_core::Error<K> {
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData {
        let message = self.to_string();
        let data = serde_json::to_value(self.wire_error(phase)).ok();
        ErrorData::new(RIFT_ERROR_CODE, message, data)
    }

    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData {
        let descriptor = self.descriptor();
        wire::ErrorData {
            code: wire_code(descriptor.name()),
            message: self.to_string(),
            retry: descriptor.retry(),
            phase,
            diagnostics: Vec::new(),
            limit: None,
            causes: self.wire_causes(),
        }
    }

    fn wire_causes(&self) -> Vec<wire::ErrorCause> {
        let descriptor = self.descriptor();
        bounded_causes(
            wire_code(descriptor.name()),
            descriptor.retry(),
            std::error::Error::source(self),
        )
    }
}

/// Walks one source chain into bounded `causes` entries, outermost first.
/// Every level inherits the classification and retry guidance passed in,
/// which the failure already resolved through the concrete fault it wraps.
fn bounded_causes(
    code: wire::ErrorCode,
    retry: wire::RetryDirective,
    outermost: Option<&(dyn std::error::Error + 'static)>,
) -> Vec<wire::ErrorCause> {
    let mut causes = Vec::new();
    let mut source = outermost;
    while let Some(current) = source {
        if causes.len() == ERROR_CAUSES_MAX {
            break;
        }
        causes.push(wire::ErrorCause {
            code,
            message: current.to_string(),
            retry,
        });
        source = current.source();
    }
    causes
}

/// The wire code for one registry identity. The registry composes the wire
/// enum, so this is a projection, not a mapping; a CLI-only identity never
/// reaches this boundary, and classifies as `internal_error` if one does.
fn wire_code(name: ErrorName) -> wire::ErrorCode {
    match name {
        ErrorName::Wire(code) => code,
        ErrorName::Cli(_) => wire::ErrorCode::InternalError,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier as ThreadBarrier};
    use std::time::Duration;

    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    use notify::{Event, EventKind};
    use rift_core::{CliCode, ErrorName, SourceVisibility};
    use rift_index::{WorkspaceIndexLimits, WorkspaceSourcePolicy};
    use rift_protocol::error as wire;
    use rift_protocol::read::GetSymbolResult;
    use rift_server::{ChangeService, ConfigurationFault, ReadError, ReadFault, ReadService};

    use super::WireFailure;
    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;
    use tokio::sync::{Barrier as AsyncBarrier, RwLock};

    use super::{
        BlockingExecutor, ChangeLane, ConfigurationFingerprint, ConfigurationState, IndexCoherence,
        IndexState, Parameters, PublishedWorkspace, RebuildOutcome, RiftMcp, WorkspaceCandidate,
        build_workspace_candidate, publish_rebuild, publish_rebuild_after, record_rebuild_failure,
        relevant_watch_event,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    async fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        Ok((directory, server))
    }

    async fn get_symbol(server: &RiftMcp, name: &str) -> Result<GetSymbolResult, rmcp::ErrorData> {
        let params = serde_json::from_value(json!({"name": name}))
            .expect("test symbol parameters must deserialize");
        server
            .get_symbol(Parameters(params))
            .await
            .map(|result| result.0)
    }

    fn arguments(
        value: &serde_json::Value,
    ) -> TestResult<serde_json::Map<String, serde_json::Value>> {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| "tool arguments must be an object".into())
    }

    fn stable_candidate(
        root: &std::path::Path,
        epoch: u64,
    ) -> TestResult<Arc<super::PublishedWorkspace>> {
        match build_workspace_candidate(root, WorkspaceIndexLimits::default(), epoch)? {
            WorkspaceCandidate::Stable(candidate) => Ok(candidate),
            WorkspaceCandidate::ConfigurationChanged => {
                Err("fixture configuration must remain stable".into())
            }
        }
    }

    #[tokio::test]
    async fn build_propagates_workspace_index_failure() {
        let directory = tempfile::tempdir().expect("fixture must exist");
        fs::write(directory.path().join("invalid.rs"), [0xff])
            .expect("invalid source fixture must write");
        let error = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())
            .await
            .expect_err("invalid source must fail");
        assert!(matches!(error.fault(), ReadFault::Index(_)));
    }

    #[test]
    fn configuration_capture_covers_content_invalid_policy_and_oversize() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = false\n",
        )?;
        assert!(matches!(
            super::configuration_fingerprint(directory.path()),
            ConfigurationFingerprint::Content(_)
        ));

        fs::write(directory.path().join("rift.toml"), "invalid = true\n")?;
        let invalid = ConfigurationState::admit(directory.path());
        assert!(invalid.admitted.is_err());
        assert_eq!(invalid.source_visibility(), SourceVisibility::default());

        fs::write(
            directory.path().join("rift.toml"),
            vec![
                b'x';
                usize::try_from(rift_server::CONFIGURATION_FILE_BYTES_MAX)
                    .expect("configuration bound must fit usize")
                    + 1
            ],
        )?;
        assert!(matches!(
            super::configuration_fingerprint(directory.path()),
            ConfigurationFingerprint::Oversized(_)
        ));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_observations_are_monotonic_and_coalesce_one_signal() -> TestResult {
        const OBSERVATIONS: usize = 32;
        let (coherence, mut invalidations) = IndexCoherence::new();
        let barrier = Arc::new(AsyncBarrier::new(OBSERVATIONS + 1));
        let mut tasks = Vec::with_capacity(OBSERVATIONS);
        for _ in 0..OBSERVATIONS {
            let coherence = Arc::clone(&coherence);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                coherence.observe().map_err(|error| error.to_string())
            }));
        }
        barrier.wait().await;
        let mut epochs = Vec::with_capacity(OBSERVATIONS);
        for task in tasks {
            epochs.push(task.await??);
        }
        epochs.sort_unstable();
        assert_eq!(epochs, (1..=OBSERVATIONS as u64).collect::<Vec<_>>());
        assert_eq!(invalidations.try_recv(), Ok(()));
        assert!(matches!(
            invalidations.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[test]
    fn observation_refuses_closed_channel_and_exhausted_epoch() {
        let (closed, receiver) = IndexCoherence::new();
        drop(receiver);
        assert!(closed.observe().is_err());
        assert!(closed.watch_failed.load(Ordering::Acquire));

        let (exhausted, _receiver) = IndexCoherence::new();
        exhausted.observed_epoch.store(u64::MAX, Ordering::Release);
        assert!(exhausted.observe().is_err());
        assert!(exhausted.watch_failed.load(Ordering::Acquire));

        let (failed, _receiver) = IndexCoherence::new();
        assert_eq!(failed.observe_watch_failure().expect("failure epoch"), 1);
        assert!(failed.watch_failed.load(Ordering::Acquire));
    }

    #[test]
    fn watcher_events_follow_source_policy_gitignore_and_hard_floor() -> TestResult {
        let directory = tempfile::tempdir()?;
        let watched_root = directory.path().join(".");
        let event_root = directory.path().canonicalize()?;
        fs::create_dir_all(directory.path().join("src/generated"))?;
        fs::create_dir_all(directory.path().join("examples"))?;
        fs::create_dir_all(directory.path().join("target"))?;
        fs::write(directory.path().join(".gitignore"), "src/ignored.rs\n")?;
        let visibility = SourceVisibility::new(
            vec!["src/**".to_owned()],
            vec!["src/generated/**".to_owned()],
            true,
        );
        let policy = WorkspaceSourcePolicy::build(
            &watched_root,
            WorkspaceIndexLimits::default(),
            &visibility,
        )?;
        let (coherence, _invalidations) = IndexCoherence::new();
        coherence.install_source_policy(Arc::new(policy));
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));

        assert!(relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "src/lib.rs")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "src/generated/code.rs")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "src/ignored.rs")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Remove(RemoveKind::Folder), "src")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Create(CreateKind::Folder), "examples")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Remove(RemoveKind::Folder), "src/generated")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), ".gitignore")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "examples/.gitignore")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "src/rift.toml")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "target/.gitignore")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &coherence,
            &event(EventKind::Modify(ModifyKind::Any), "rift.toml")
        ));
        Ok(())
    }

    fn assert_workspace_identity(
        actual: &super::PublishedWorkspace,
        expected: &super::PublishedWorkspace,
    ) {
        assert_eq!(actual.epoch, expected.epoch);
        assert_eq!(actual.fingerprint, expected.fingerprint);
        assert_eq!(
            actual.configuration.fingerprint,
            expected.configuration.fingerprint
        );
        assert_eq!(
            actual.configuration.source_visibility().respect_gitignore(),
            expected
                .configuration
                .source_visibility()
                .respect_gitignore()
        );
        assert!(Arc::ptr_eq(&actual.source_policy, &expected.source_policy));
    }

    struct PublicationFixture {
        _directory: tempfile::TempDir,
        before: Arc<super::PublishedWorkspace>,
        after: Arc<super::PublishedWorkspace>,
        state: Arc<RwLock<IndexState>>,
        coherence: Arc<IndexCoherence>,
        _invalidations: tokio::sync::mpsc::Receiver<()>,
    }

    fn publication_fixture() -> TestResult<PublicationFixture> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn before() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = true\n",
        )?;
        let before = stable_candidate(directory.path(), 0)?;
        let state = Arc::new(RwLock::new(IndexState {
            current: Arc::clone(&before),
            failure: None,
        }));
        let (coherence, invalidations) = IndexCoherence::new();
        coherence.install_source_policy(Arc::clone(&before.source_policy));
        fs::write(directory.path().join("lib.rs"), "pub fn after() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = false\n",
        )?;
        let epoch = coherence.observe()?;
        let after = stable_candidate(directory.path(), epoch)?;
        Ok(PublicationFixture {
            _directory: directory,
            before,
            after,
            state,
            coherence,
            _invalidations: invalidations,
        })
    }

    fn spawn_snapshot_readers(
        state: &Arc<RwLock<IndexState>>,
        expected: &Arc<super::PublishedWorkspace>,
        readers_count: usize,
    ) -> (Arc<ThreadBarrier>, Vec<std::thread::JoinHandle<()>>) {
        let capture_barrier = Arc::new(ThreadBarrier::new(readers_count + 1));
        let mut readers = Vec::with_capacity(readers_count);
        for _ in 0..readers_count {
            let state = Arc::clone(state);
            let reader_barrier = Arc::clone(&capture_barrier);
            let expected = Arc::clone(expected);
            readers.push(std::thread::spawn(move || {
                let state = state.blocking_read();
                let (snapshot, failure) = state.snapshot();
                drop(state);
                reader_barrier.wait();
                assert!(failure.is_none());
                assert_workspace_identity(&snapshot, &expected);
            }));
        }
        (capture_barrier, readers)
    }

    fn spawn_blocked_snapshot_readers(
        state: &Arc<RwLock<IndexState>>,
        expected: &Arc<super::PublishedWorkspace>,
        readers_count: usize,
    ) -> (Arc<ThreadBarrier>, Vec<std::thread::JoinHandle<()>>) {
        let ready_barrier = Arc::new(ThreadBarrier::new(readers_count + 1));
        let mut readers = Vec::with_capacity(readers_count);
        for _ in 0..readers_count {
            let state = Arc::clone(state);
            let reader_barrier = Arc::clone(&ready_barrier);
            let expected = Arc::clone(expected);
            readers.push(std::thread::spawn(move || {
                reader_barrier.wait();
                let state = state.blocking_read();
                let (snapshot, failure) = state.snapshot();
                drop(state);
                assert!(failure.is_none());
                assert_workspace_identity(&snapshot, &expected);
            }));
        }
        (ready_barrier, readers)
    }

    fn assert_published_fixture(fixture: &PublicationFixture) {
        let state = fixture.state.blocking_read();
        assert_workspace_identity(&state.current, &fixture.after);
        drop(state);
        let policy = fixture
            .coherence
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(Arc::ptr_eq(
            policy.as_ref().expect("policy must be published"),
            &fixture.after.source_policy
        ));
    }

    #[test]
    fn parallel_reads_span_atomic_publication_without_mixed_identity() -> TestResult {
        const READERS: usize = 8;
        let fixture = publication_fixture()?;
        let (prepublication_captures, prepublication_readers) =
            spawn_snapshot_readers(&fixture.state, &fixture.before, READERS);
        prepublication_captures.wait();
        let publication_locked = Arc::new(ThreadBarrier::new(2));
        let publication_released = Arc::new(ThreadBarrier::new(2));
        let publisher_state = Arc::clone(&fixture.state);
        let publisher_coherence = Arc::clone(&fixture.coherence);
        let published_candidate = Arc::clone(&fixture.after);
        let locked = Arc::clone(&publication_locked);
        let released = Arc::clone(&publication_released);
        let publisher = std::thread::spawn(move || {
            publish_rebuild_after(
                &publisher_state,
                &publisher_coherence,
                published_candidate,
                || {
                    locked.wait();
                    released.wait();
                },
            )
        });
        publication_locked.wait();

        let (blocked_readers_ready, blocked_readers) =
            spawn_blocked_snapshot_readers(&fixture.state, &fixture.after, READERS);
        blocked_readers_ready.wait();
        let observation_started = Arc::new(ThreadBarrier::new(2));
        let observer_coherence = Arc::clone(&fixture.coherence);
        let observer_started = Arc::clone(&observation_started);
        let observer = std::thread::spawn(move || {
            observer_started.wait();
            observer_coherence.observe()
        });
        observation_started.wait();
        publication_released.wait();
        assert_eq!(
            publisher.join().expect("publisher thread must not panic"),
            RebuildOutcome::Published
        );
        assert_eq!(observer.join().expect("observer thread must not panic")?, 2);
        for reader in prepublication_readers.into_iter().chain(blocked_readers) {
            reader.join().expect("reader thread must not panic");
        }
        assert_published_fixture(&fixture);
        assert_eq!(fixture.coherence.observed_epoch(), 2);
        Ok(())
    }

    #[test]
    fn superseded_state_updates_are_rejected_and_success_clears_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn before() {}\n")?;
        let before = stable_candidate(directory.path(), 0)?;
        fs::write(directory.path().join("lib.rs"), "pub fn after() {}\n")?;
        let after = stable_candidate(directory.path(), 1)?;
        let mut state = IndexState {
            current: Arc::clone(&before),
            failure: None,
        };

        assert!(!state.publish(Arc::clone(&after), 2));
        assert_workspace_identity(&state.current, &before);
        assert!(!state.record_failure(1, 2, ReadFault::unavailable("test rebuild", "superseded")));
        assert!(state.failure.is_none());
        assert!(state.record_failure(1, 1, ReadFault::unavailable("test rebuild", "failed")));
        assert!(state.failure.is_some());
        assert!(state.publish(Arc::clone(&after), 1));
        assert_workspace_identity(&state.current, &after);
        assert!(state.failure.is_none());

        let state = RwLock::new(IndexState {
            current: Arc::clone(&before),
            failure: None,
        });
        let (coherence, _invalidations) = IndexCoherence::new();
        coherence.install_source_policy(Arc::clone(&before.source_policy));
        assert_eq!(coherence.observe()?, 1);
        assert_eq!(coherence.observe()?, 2);
        assert_eq!(
            publish_rebuild(&state, &coherence, Arc::clone(&after)),
            RebuildOutcome::Superseded
        );
        let state_snapshot = state.blocking_read();
        assert_workspace_identity(&state_snapshot.current, &before);
        drop(state_snapshot);
        let source_policy = coherence
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(Arc::ptr_eq(
            source_policy
                .as_ref()
                .expect("policy must remain installed"),
            &before.source_policy
        ));
        drop(source_policy);
        assert!(!record_rebuild_failure(
            &state,
            &coherence,
            1,
            ReadFault::unavailable("test rebuild", "superseded failure")
        ));
        assert!(state.blocking_read().failure.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn external_create_modify_rename_and_delete_stay_current() -> TestResult {
        let (directory, server) = fixture().await?;
        let created = directory.path().join("external.rs");
        fs::write(&created, "pub fn external_created() {}\n")?;
        let result = get_symbol(&server, "external_created")
            .await
            .map_err(|error| format!("external create must reconcile: {error:?}"))?;
        assert_eq!(result.hits.len(), 1);

        fs::write(&created, "pub fn external_modified() {}\n")?;
        let result = get_symbol(&server, "external_modified")
            .await
            .map_err(|error| format!("external modify must reconcile: {error:?}"))?;
        assert_eq!(result.hits.len(), 1);
        assert!(
            get_symbol(&server, "external_created")
                .await?
                .hits
                .is_empty()
        );

        let renamed = directory.path().join("renamed.rs");
        fs::rename(&created, &renamed)?;
        let result = get_symbol(&server, "external_modified")
            .await
            .map_err(|error| format!("external rename must reconcile: {error:?}"))?;
        let unit = result.hits[0]
            .symbol
            .origin
            .unit
            .as_ref()
            .ok_or("renamed symbol must retain source unit")?;
        assert!(unit.0.ends_with("/renamed.rs"));

        fs::remove_file(renamed)?;
        let result = get_symbol(&server, "external_modified")
            .await
            .map_err(|error| format!("external delete must reconcile: {error:?}"))?;
        assert!(result.hits.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn external_burst_coalesces_without_losing_final_bytes() -> TestResult {
        let (directory, server) = fixture().await?;
        let path = directory.path().join("burst.rs");
        for sequence in 0..32 {
            fs::write(&path, format!("pub fn burst_{sequence}() {{}}\n"))?;
        }
        let result = get_symbol(&server, "burst_31")
            .await
            .map_err(|error| format!("burst final state must reconcile: {error:?}"))?;
        assert_eq!(result.hits.len(), 1);
        assert!(get_symbol(&server, "burst_0").await?.hits.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn external_rebuild_failure_recovers_after_tree_is_valid() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("lib.rs");
        fs::write(&path, "pub fn beacon() {}\n")?;
        let tight =
            WorkspaceIndexLimits::new(4, 60, 60, 4, 100).map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;

        fs::write(
            &path,
            format!("pub fn oversized() {{}}\n{}", " ".repeat(80)),
        )?;
        let error = get_symbol(&server, "oversized")
            .await
            .expect_err("oversized external edit must refuse a current answer");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str);
        assert_eq!(code, Some("limit_exceeded"));

        fs::write(&path, "pub fn recovered() {}\n")?;
        let mut recovered = false;
        for _attempt in 0..100 {
            if get_symbol(&server, "recovered")
                .await
                .is_ok_and(|result| result.hits.len() == 1)
            {
                recovered = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(recovered, "valid external edit must recover failed rebuild");
        Ok(())
    }

    #[tokio::test]
    async fn ignore_policy_and_hard_floor_exclude_external_source() -> TestResult {
        let (directory, server) = fixture().await?;
        fs::write(
            directory.path().join("policy.rs"),
            "pub fn policy_hidden() {}\n",
        )?;
        assert_eq!(get_symbol(&server, "policy_hidden").await?.hits.len(), 1);

        fs::write(directory.path().join(".gitignore"), "policy.rs\n")?;
        assert!(get_symbol(&server, "policy_hidden").await?.hits.is_empty());
        fs::remove_file(directory.path().join(".gitignore"))?;
        assert_eq!(get_symbol(&server, "policy_hidden").await?.hits.len(), 1);

        fs::create_dir(directory.path().join("target"))?;
        fs::write(
            directory.path().join("target/ignored.rs"),
            "pub fn hard_floor_hidden() {}\n",
        )?;
        assert!(
            get_symbol(&server, "hard_floor_hidden")
                .await?
                .hits
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn index_supervisor_shutdown_is_joined_and_idempotent() -> TestResult {
        let (_directory, server) = fixture().await?;
        let supervisor = server.index_supervisor();
        drop(server);
        assert!(supervisor.coherence.cancellation.is_cancelled());
        supervisor.shutdown().await?;
        supervisor.shutdown().await?;
        assert!(supervisor.coherence.task.lock().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn server_table_sizes_blocking_pool_and_queue_wait() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let configured = "[server]\nblocking_slots = 2\nblocking_queue_timeout = \"1250ms\"\n";
        fs::write(directory.path().join("rift.toml"), configured)?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert_eq!(server.blocking.queue_timeout_ms, 1_250);
        assert_eq!(server.blocking.operations.available_permits(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn missing_server_table_keeps_default_blocking_policy() -> TestResult {
        let (_directory, server) = fixture().await?;
        let default_table = rift_protocol::configuration::ServerConfiguration::default();
        assert_eq!(
            server.blocking.queue_timeout_ms,
            default_table.blocking_queue_timeout.milliseconds()
        );
        assert_eq!(
            server.blocking.operations.available_permits() as u64,
            default_table.blocking_slots
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_configuration_builds_default_blocking_policy() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[server]\nblocking_slots = 0\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let default_table = rift_protocol::configuration::ServerConfiguration::default();
        assert_eq!(
            server.blocking.operations.available_permits() as u64,
            default_table.blocking_slots
        );
        Ok(())
    }

    #[tokio::test]
    async fn change_lane_waits_for_active_publication_before_entering() {
        let lane = Arc::new(ChangeLane::default());
        let (first_entered_sender, first_entered_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let first_lane = Arc::clone(&lane);
        let first = tokio::task::spawn_blocking(move || {
            first_lane.run(|| {
                first_entered_sender
                    .send(())
                    .expect("first entry witness must still be listening");
                release_receiver
                    .recv()
                    .expect("test must release first lane operation");
                1_u8
            })
        });
        first_entered_receiver
            .await
            .expect("first operation must enter change lane");

        let second_entered = Arc::new(AtomicBool::new(false));
        let second_flag = Arc::clone(&second_entered);
        let (contended_sender, contended_receiver) = tokio::sync::oneshot::channel();
        let second_lane = Arc::clone(&lane);
        let second = tokio::task::spawn_blocking(move || {
            second_lane.run_after_contention(
                || {
                    contended_sender
                        .send(())
                        .expect("contention witness must still be listening");
                },
                || {
                    second_flag.store(true, Ordering::SeqCst);
                    2_u8
                },
            )
        });
        contended_receiver
            .await
            .expect("second operation must reach occupied admission");
        assert!(
            !second_entered.load(Ordering::SeqCst),
            "second operation must not enter before first publication releases"
        );

        release_sender
            .send(())
            .expect("first lane operation must accept release");
        assert_eq!(first.await.expect("first lane task must join"), 1);
        assert_eq!(second.await.expect("second lane task must join"), 2);
        assert!(
            second_entered.load(Ordering::SeqCst),
            "second operation must proceed after publication releases"
        );
    }

    #[tokio::test]
    async fn blocking_executor_queues_until_capacity_returns() {
        let executor = BlockingExecutor::isolated(1, 1_000);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let held_executor = executor.clone();
        let held = tokio::spawn(async move {
            held_executor
                .run("held operation", move || {
                    let _ = started_sender.send(());
                    release_receiver
                        .recv()
                        .expect("test must release held blocking operation");
                    Ok(1_u8)
                })
                .await
        });
        started_receiver
            .await
            .expect("held blocking operation must start");

        let queued_started = Arc::new(AtomicBool::new(false));
        let queued_flag = Arc::clone(&queued_started);
        let queued_executor = executor.clone();
        let (queued_ready_sender, queued_ready_receiver) = tokio::sync::oneshot::channel();
        let queued = tokio::spawn(async move {
            queued_ready_sender
                .send(())
                .expect("queue witness must still be listening");
            queued_executor
                .run("queued operation", move || {
                    queued_flag.store(true, Ordering::SeqCst);
                    Ok(2_u8)
                })
                .await
        });
        queued_ready_receiver
            .await
            .expect("queued task must reach admission");
        assert!(
            !queued_started.load(Ordering::SeqCst),
            "queued work must not start before capacity returns"
        );

        release_sender
            .send(())
            .expect("held blocking operation must accept release");
        assert_eq!(
            held.await
                .expect("held task must join")
                .expect("held operation must succeed"),
            1
        );
        assert_eq!(
            queued
                .await
                .expect("queued task must join")
                .expect("queued operation must succeed"),
            2
        );
        assert!(
            queued_started.load(Ordering::SeqCst),
            "queued work must start after capacity returns"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn blocking_executor_queue_timeout_is_retryable_and_bounded() {
        const QUEUE_TIMEOUT_MS: u64 = 25;
        let executor = BlockingExecutor::isolated(1, QUEUE_TIMEOUT_MS);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel(0);
        let held_executor = executor.clone();
        let held = tokio::spawn(async move {
            held_executor
                .run("held operation", move || {
                    let _ = started_sender.send(());
                    release_receiver
                        .recv()
                        .expect("test must release held blocking operation");
                    Ok(())
                })
                .await
        });
        started_receiver
            .await
            .expect("held blocking operation must start");
        let queued_executor = executor.clone();
        let (queued_ready_sender, queued_ready_receiver) = tokio::sync::oneshot::channel();
        let queued = tokio::spawn(async move {
            queued_ready_sender
                .send(())
                .expect("timeout witness must still be listening");
            queued_executor.run("queued operation", || Ok(())).await
        });
        queued_ready_receiver
            .await
            .expect("timed operation must reach admission");
        tokio::time::advance(Duration::from_millis(QUEUE_TIMEOUT_MS + 1)).await;
        let error = queued
            .await
            .expect("queued task must join")
            .expect_err("queue wait beyond timeout must fail");
        assert!(matches!(
            error.fault(),
            ReadFault::CapacityTimeout {
                operation: "queued operation",
                timeout_ms: QUEUE_TIMEOUT_MS,
            }
        ));
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        let context = error.context();
        assert_eq!(context[0].value(), "queued operation");
        assert_eq!(context[1].value(), QUEUE_TIMEOUT_MS.to_string());

        release_sender
            .send(())
            .expect("held blocking operation must accept release");
        held.await
            .expect("held task must join")
            .expect("held operation must succeed");
        executor
            .run("operation after timeout", || Ok(()))
            .await
            .expect("timed-out waiter must leave capacity reusable");
    }

    #[tokio::test]
    async fn blocking_executor_preserves_work_error() {
        let executor = BlockingExecutor::isolated(1, 1_000);
        let error = executor
            .run("refused operation", || -> Result<(), ReadError> {
                Err(ReadError::from(ReadFault::Unsupported {
                    capability: "probe",
                }))
            })
            .await
            .expect_err("work refusal must survive blocking executor");
        assert!(matches!(error.fault(), ReadFault::Unsupported { .. }));
    }

    #[tokio::test]
    async fn blocking_executor_classifies_worker_panic_as_join_failure() {
        let executor = BlockingExecutor::isolated(1, 1_000);
        let error = executor
            .run(
                "panicking operation",
                || -> Result<(), rift_server::ReadError> { panic!("test blocking worker panic") },
            )
            .await
            .expect_err("worker panic must become task failure");
        let ReadFault::Task { operation, detail } = error.fault() else {
            panic!("worker panic must classify as task failure: {error:?}");
        };
        assert_eq!(*operation, "panicking operation");
        assert!(detail.contains("panic"), "{detail}");
        executor
            .run("operation after panic", || Ok(()))
            .await
            .expect("panicked worker must release its capacity permit");
    }

    #[tokio::test]
    async fn blocking_executor_classifies_closed_queue() {
        let executor = BlockingExecutor::isolated(1, 1_000);
        executor.operations.close();
        let error = executor
            .run("closed queue operation", || Ok(()))
            .await
            .expect_err("closed semaphore must fail admission");
        assert!(matches!(error.fault(), ReadFault::Task { .. }));
    }

    #[test]
    fn serialized_change_refuses_invalid_configuration_before_operation() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let candidate = stable_candidate(directory.path(), 0)?;
        let configuration_error = rift_core::Error::new(ConfigurationFault::Malformed {
            detail: "test invalid configuration".to_owned(),
        });
        let published = tokio::sync::RwLock::new(IndexState {
            current: Arc::new(PublishedWorkspace {
                reads: Arc::clone(&candidate.reads),
                configuration: ConfigurationState {
                    admitted: Err(Arc::new(configuration_error)),
                    fingerprint: super::configuration_fingerprint(directory.path()),
                },
                fingerprint: candidate.fingerprint.clone(),
                source_policy: Arc::clone(&candidate.source_policy),
                epoch: 0,
            }),
            failure: None,
        });
        let (coherence, _invalidations) = IndexCoherence::new();
        let changes = ChangeService::new(directory.path());
        let operation_called = AtomicBool::new(false);

        let outcome = RiftMcp::change_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &published,
            &coherence,
            &changes,
            |_, _| {
                operation_called.store(true, Ordering::SeqCst);
                panic!("invalid configuration must stop before operation")
            },
        )?;
        let Err(error) = outcome else {
            panic!("invalid configuration must refuse change");
        };
        let data = error.data.expect("Rift error must carry typed data");

        assert_eq!(data["code"], json!("configuration_invalid"));
        assert!(!operation_called.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn client_lists_and_calls_exact_read_only_surface() -> TestResult {
        let (_directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let tools = client.list_all_tools().await?;

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            [
                "get_symbol",
                "insert_symbol",
                "nodes",
                "patch",
                "replace_node",
                "replace_symbol",
                "search"
            ]
        );
        assert!(tools.iter().all(|tool| tool.output_schema.is_some()));

        let symbol = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon"}))?),
            )
            .await?;
        let structured = symbol
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        assert_eq!(structured["hits"][0]["symbol"]["name"], "beacon");
        assert_eq!(structured["next_cursor"], serde_json::Value::Null);

        let search = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": "beacon"}))?),
            )
            .await?;
        assert!(
            !search
                .structured_content
                .ok_or("search must return structured content")?["results"]
                .as_array()
                .ok_or("search results must be an array")?
                .is_empty()
        );

        let nodes = client
            .call_tool(
                CallToolRequestParams::new("nodes")
                    .with_arguments(arguments(&json!({"path": "lib.rs", "position": 8}))?),
            )
            .await?;
        let structured = nodes
            .structured_content
            .ok_or("nodes must return structured content")?;
        let listed = structured["nodes"]
            .as_array()
            .ok_or("nodes must be an array")?;
        assert!(
            !listed.is_empty(),
            "position 8 sits inside `pub fn beacon`, so at least one node covers it"
        );
        let witness_suffix = has_witness_fragment(listed[0]["id"].as_str().unwrap_or_default());
        assert!(
            witness_suffix,
            "every listed node id must end in an eight-hex-character witness: {}",
            listed[0]["id"]
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Reports whether a node id ends in `#` plus eight lowercase hex digits.
    fn has_witness_fragment(id: &str) -> bool {
        id.rsplit_once('#').is_some_and(|(_, witness)| {
            witness.len() == 8
                && witness
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() && !character.is_uppercase())
        })
    }

    #[tokio::test]
    async fn exported_schema_document_matches_served_tools() -> TestResult {
        let (_directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let mut advertised = client.list_all_tools().await?;
        advertised.sort_by(|left, right| left.name.cmp(&right.name));

        let document: serde_json::Value = serde_json::from_str(&crate::schema::schema_document())?;
        let exported = document["tools"]
            .as_array()
            .ok_or("exported document must carry a tools array")?;

        assert_eq!(exported.len(), advertised.len());
        for (entry, tool) in exported.iter().zip(&advertised) {
            assert_eq!(entry["name"], json!(tool.name));
            assert_eq!(entry["description"], json!(tool.description));
            assert_eq!(entry["input_schema"], json!(tool.input_schema));
            assert_eq!(entry["output_schema"], json!(tool.output_schema));
        }

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn client_change_lands_and_reads_serve_the_new_snapshot() -> TestResult {
        let (_directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let change = client
            .call_tool(
                CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
                    "symbol": "rift://symbol/rust/lib.rs/beacon",
                    "body": "pub fn beacon() -> u8 {\n    7\n}"
                }))?),
            )
            .await?;
        let structured = change
            .structured_content
            .ok_or("replace_symbol must return structured content")?;
        assert_eq!(structured["status"], json!("applied"));
        assert_eq!(structured["summary"]["paths"], json!(["lib.rs"]));

        let symbol = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon"}))?),
            )
            .await?;
        let structured = symbol
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        let excerpt = structured["hits"][0]["source"]["text"]
            .as_str()
            .ok_or("hit must carry source text")?;
        assert!(
            excerpt.contains("-> u8"),
            "reads after an applied change must serve the new snapshot: {excerpt}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_inserts_publish_from_serialized_fresh_snapshots() -> TestResult {
        let (directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let first_client = client.peer().clone();
        let second_client = client.peer().clone();
        let first = first_client.call_tool(
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon",
                "position": "after",
                "body": "pub fn first_insert() {}"
            }))?),
        );
        let second = second_client.call_tool(
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon",
                "position": "after",
                "body": "pub fn second_insert() {}"
            }))?),
        );
        let (first, second) = tokio::join!(first, second);
        for result in [first?, second?] {
            let structured = result
                .structured_content
                .ok_or("insert_symbol must return structured content")?;
            assert_eq!(structured["status"], json!("applied"));
        }
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(
            written.contains("first_insert"),
            "first concurrent insert must survive: {written}"
        );
        assert!(
            written.contains("second_insert"),
            "second concurrent insert must survive: {written}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn applied_change_reports_failed_snapshot_rebuild_as_warning() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}
",
        )?;
        let tight = rift_index::WorkspaceIndexLimits::new(4, 60, 60, 4, 100)
            .map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let grown = "/// Grown far beyond the configured workspace byte bound.
pub fn beacon() -> u64 {
    7_000_000_000_000_000_000
}";
        let change = client
            .call_tool(
                CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
                    "symbol": "rift://symbol/rust/lib.rs/beacon",
                    "body": grown
                }))?),
            )
            .await?;
        let structured = change
            .structured_content
            .ok_or("replace_symbol must return structured content")?;
        assert_eq!(structured["status"], json!("applied"));
        let findings = structured["summary"]["diagnostics"]
            .as_array()
            .ok_or("summary must carry diagnostics")?;
        assert!(
            findings.iter().any(|finding| {
                finding["severity"] == json!("warning")
                    && finding["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("could not refresh"))
            }),
            "a failed rebuild after a landed change must ride the result as a \
             warning: {structured:#}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Calls one tool expecting a Rift wire error and returns the JSON-RPC
    /// error object.
    async fn failing_call(
        arguments_value: &serde_json::Value,
        tool: &'static str,
    ) -> TestResult<rmcp::ErrorData> {
        let (_directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let error = client
            .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments(arguments_value)?))
            .await
            .expect_err("the request must be rejected");
        client.cancel().await?;
        server_task.await?;
        let ServiceError::McpError(data) = error else {
            panic!("expected protocol-level McpError, got {error:?}");
        };
        Ok(data)
    }

    #[tokio::test]
    async fn client_rejects_empty_search_query_with_typed_wire_error() -> TestResult {
        let data = failing_call(&json!({"query": ""}), "search").await?;
        assert_eq!(data.code, ErrorCode(-32000));
        assert_eq!(
            data.message.as_ref(),
            "the request does not match the documented form: field query, \
             violation empty; correct the reported field and resend the request"
        );
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("invalid_request"));
        assert_eq!(wire["retry"], json!("never"));
        assert_eq!(wire["phase"], json!("read"));
        assert_eq!(wire["causes"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn client_rejects_zero_result_limit_as_invalid_request() -> TestResult {
        let data = failing_call(&json!({"name": "beacon", "limit": 0}), "get_symbol").await?;
        assert_eq!(data.code, ErrorCode(-32000));
        assert_eq!(
            data.message.as_ref(),
            "the request does not match the documented form: field limit, \
             violation zero; correct the reported field and resend the request"
        );
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("invalid_request"));
        Ok(())
    }

    #[test]
    fn cli_identity_projects_to_internal_error_on_the_wire() {
        assert_eq!(
            super::wire_code(ErrorName::Cli(CliCode::ArtifactStale)),
            wire::ErrorCode::InternalError
        );
    }

    #[derive(Debug)]
    struct Link {
        depth: usize,
        inner: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "link {}", self.depth)
        }
    }

    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.inner
                .as_deref()
                .map(|link| link as &(dyn Error + 'static))
        }
    }

    #[test]
    fn cause_walk_stops_at_the_declared_bound() {
        let mut chained = Link {
            depth: 0,
            inner: None,
        };
        for depth in 1..=super::ERROR_CAUSES_MAX + 2 {
            chained = Link {
                depth,
                inner: Some(Box::new(chained)),
            };
        }
        let causes = super::bounded_causes(
            wire::ErrorCode::StorageFailure,
            wire::RetryDirective::Never,
            Some(&chained),
        );
        assert_eq!(
            causes.len(),
            super::ERROR_CAUSES_MAX,
            "a chain deeper than the bound must truncate at the bound"
        );
    }

    fn probe_hook() -> rift_protocol::configuration::CommandHook {
        use rift_protocol::configuration::{ChangedPaths, Determinism, HookKind, HookType};
        rift_protocol::configuration::CommandHook {
            r#type: HookType::Command,
            id: "tests".to_owned(),
            kind: HookKind::Test,
            program: "cargo".to_owned(),
            arguments: vec!["test".to_owned()],
            changed_paths: ChangedPaths::None,
            working_directory: rift_protocol::read::ProjectPath(String::new()),
            environment: std::collections::BTreeMap::new(),
            timeout_ms: 120_000,
            output_limit_bytes: 4_096,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
    }

    fn silent_run(status: rift_server::HookStatus, exit_code: Option<i32>) -> rift_server::HookRun {
        rift_server::HookRun {
            id: "tests".to_owned(),
            status,
            exit_code,
            stdout: rift_server::CapturedStream::default(),
            stderr: rift_server::CapturedStream::default(),
        }
    }

    #[test]
    fn failed_hook_finding_quotes_exit_code_and_nonempty_streams() {
        use rift_server::{CapturedStream, HookStatus};
        let mut run = silent_run(HookStatus::Failed, Some(1));
        run.stdout = CapturedStream {
            text: "boom".to_owned(),
            captured_bytes: 4,
            total_bytes: 4,
            truncated: false,
        };
        let finding = super::hook_failure_diagnostic(&probe_hook(), &run);
        assert_eq!(finding.severity, rift_protocol::read::Severity::Error);
        assert_eq!(finding.code.as_deref(), Some("rift.hook.failed"));
        assert!(
            finding.message.contains("exited 1")
                && finding.message.contains("stdout (4 of 4 bytes): boom")
                && !finding.message.contains("stderr"),
            "{}",
            finding.message
        );
    }

    #[test]
    #[should_panic(expected = "a passing hook contributes guarantees, not findings")]
    fn passing_hook_finding_is_a_programmer_error() {
        let run = silent_run(rift_server::HookStatus::Passed, Some(0));
        let _ = super::hook_failure_diagnostic(&probe_hook(), &run);
    }

    #[test]
    fn hook_finding_accounts_for_every_non_passing_outcome() {
        use rift_server::HookStatus;
        let cases = [
            (HookStatus::Failed, None, "exited nonzero"),
            (HookStatus::TimedOut, None, "killed after 120000ms"),
            (
                HookStatus::Error("failed to launch: missing".to_owned()),
                None,
                "failed to launch: missing",
            ),
        ];
        for (status, exit_code, expected) in cases {
            let finding =
                super::hook_failure_diagnostic(&probe_hook(), &silent_run(status, exit_code));
            assert!(
                finding.message.contains(expected),
                "{expected} missing from {}",
                finding.message
            );
        }
    }

    #[test]
    fn bounded_prefix_cuts_on_character_boundaries() {
        assert_eq!(super::bounded_prefix("short", 16), "short");
        assert_eq!(super::bounded_prefix("ééé", 3), "é");
        assert_eq!(super::bounded_prefix("ééé", 4), "éé");
    }

    #[test]
    fn stale_snapshot_finding_carries_its_code_and_the_render() {
        let error = rift_server::ReadError::from(ReadFault::Unsupported {
            capability: "probe",
        });
        let finding = super::stale_snapshot_diagnostic(&error);
        assert_eq!(finding.code.as_deref(), Some("rift.snapshot.stale"));
        assert_eq!(finding.severity, rift_protocol::read::Severity::Warning);
        assert!(
            finding.message.contains("the change landed"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn wire_causes_walk_the_source_chain_with_inherited_classification() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("missing root must fail");
        let causes = error.wire_causes();
        assert!(!causes.is_empty(), "sourced failure must yield causes");
        assert!(causes.len() <= super::ERROR_CAUSES_MAX);
        let code = super::wire_code(error.descriptor().name());
        for cause in &causes {
            assert!(!cause.message.is_empty(), "cause message must be rendered");
            assert_eq!(cause.code, code);
        }
    }
}
