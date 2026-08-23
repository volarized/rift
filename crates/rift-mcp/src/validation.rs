//! Current-index validation: filesystem observation, serialized rebuilds,
//! and atomic publication of the workspace snapshot.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock as SyncRwLock};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use rift_core::constants::{WORKSPACE_CONFIGURATION_FILE, WORKSPACE_IGNORED_DIRECTORIES};
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{
    LexicalSearchIndex, WorkspaceFingerprint, WorkspaceIndexLimits, WorkspaceSourcePolicy,
};
use rift_protocol::configuration::{
    SearchConfiguration, ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_server::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ReadError, ReadFault, ReadService,
    load_configuration,
};
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::failure::WireFailure;
use crate::server::{BlockingExecutor, ChangeLane};

/// Filesystem events coalesced while one rebuild is pending.
pub(crate) const INDEX_INVALIDATIONS_MAX: usize = 1;
/// Delay collecting one bounded filesystem-event batch.
pub(crate) const INDEX_DEBOUNCE: Duration = Duration::from_millis(50);
/// Deadline for one request to obtain a validated current snapshot.
pub(crate) const INDEX_FRESHNESS_TIMEOUT: Duration = Duration::from_secs(30);
/// Deadline for joining the index supervisor during shutdown.
pub(crate) const INDEX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Complete capture retries while the tree keeps moving.
pub(crate) const INDEX_CAPTURE_ATTEMPTS_MAX: usize = 3;

/// Read index and configuration policy published as one immutable value.
#[derive(Debug)]
pub(crate) struct PublishedWorkspace {
    pub(crate) reads: Arc<ReadService>,
    pub(crate) configuration: ConfigurationState,
    pub(crate) fingerprint: WorkspaceFingerprint,
    pub(crate) source_policy: Arc<WorkspaceSourcePolicy>,
    pub(crate) epoch: u64,
}

/// Published workspace plus failure for latest observed epoch.
#[derive(Debug)]
pub(crate) struct IndexState {
    pub(crate) current: Arc<PublishedWorkspace>,
    pub(crate) failure: Option<(u64, Arc<ReadError>)>,
}

impl IndexState {
    /// Clones one validated publication and its latest failure.
    pub(crate) fn snapshot(&self) -> (Arc<PublishedWorkspace>, Option<(u64, Arc<ReadError>)>) {
        (Arc::clone(&self.current), self.failure.clone())
    }

    /// Publishes candidate only while its observation remains current.
    pub(crate) fn publish(
        &mut self,
        candidate: Arc<PublishedWorkspace>,
        observed_epoch: u64,
    ) -> bool {
        if candidate.epoch != observed_epoch {
            return false;
        }
        self.current = candidate;
        self.failure = None;
        true
    }

    /// Records failure only while its observation remains current.
    pub(crate) fn record_failure(
        &mut self,
        epoch: u64,
        observed_epoch: u64,
        error: ReadError,
    ) -> bool {
        if epoch != observed_epoch {
            return false;
        }
        self.failure = Some((epoch, Arc::new(error)));
        true
    }
}

/// Filesystem observation and supervisor ownership shared with handlers.
#[derive(Debug)]
pub(crate) struct IndexValidation {
    pub(crate) observed_epoch: Arc<AtomicU64>,
    pub(crate) watch_failed: Arc<AtomicBool>,
    pub(crate) invalidations: mpsc::Sender<()>,
    pub(crate) changed: Arc<Notify>,
    pub(crate) publication_lane: SyncMutex<()>,
    pub(crate) source_policy: SyncRwLock<Option<Arc<WorkspaceSourcePolicy>>>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) task: AsyncMutex<Option<JoinHandle<()>>>,
}

/// Owned shutdown handle for the workspace index supervisor.
#[derive(Debug, Clone)]
pub(crate) struct IndexSupervisor {
    pub(crate) validation: Arc<IndexValidation>,
}

/// Rebuild dependencies owned by index supervisor task.
pub(crate) struct IndexSupervisorContext {
    pub(crate) root: PathBuf,
    pub(crate) limits: WorkspaceIndexLimits,
    pub(crate) published: Arc<RwLock<IndexState>>,
    pub(crate) change_lane: Arc<ChangeLane>,
    pub(crate) validation: Arc<IndexValidation>,
    pub(crate) blocking: BlockingExecutor,
    /// The lexical search database, absent when it could not be opened at startup.
    pub(crate) lexical: Option<Arc<LexicalSearchIndex>>,
}

/// The last acceptance of the workspace's `rift.toml`, kept with the file
/// state it was read from so an edited file is re-accepted on the next
/// request and an unchanged one is not re-parsed per call.
#[derive(Debug, Clone)]
pub(crate) struct ConfigurationState {
    pub(crate) accepted: Result<WorkspaceConfiguration, Arc<ConfigurationError>>,
    pub(crate) fingerprint: ConfigurationFingerprint,
}

impl ConfigurationState {
    /// Accepts the workspace's current `rift.toml`.
    pub(crate) fn accept(root: &Path) -> Self {
        let fingerprint = configuration_fingerprint(root);
        Self {
            accepted: load_configuration(root).map_err(Arc::new),
            fingerprint,
        }
    }

    /// The acceptance's outcome as one request sees it: the configuration to
    /// serve under, or the typed refusal naming what to fix.
    pub(crate) fn accepted(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<WorkspaceConfiguration, ErrorData> {
        match &self.accepted {
            Ok(configuration) => Ok(configuration.clone()),
            Err(error) => Err(error.tool_error(phase)),
        }
    }

    /// The `[server]` table from the last acceptance, or the default table
    /// while `rift.toml` is invalid.
    pub(crate) fn server_configuration(&self) -> ServerConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.server.clone())
            .unwrap_or_default()
    }

    /// The `[source]` policy from the last acceptance, or the default policy
    /// while `rift.toml` is invalid.
    pub(crate) fn source_visibility(&self) -> SourceVisibility {
        self.accepted.as_ref().map_or_else(
            |_| SourceVisibility::default(),
            |configuration| SourceVisibility::from(&configuration.source),
        )
    }

    /// The `[search.text]` inclusion from the last acceptance, or the default inclusion while
    /// `rift.toml` is invalid.
    pub(crate) fn text_inclusion(&self) -> TextFileInclusion {
        self.accepted.as_ref().map_or_else(
            |_| TextFileInclusion::default(),
            |configuration| TextFileInclusion::from(&configuration.search),
        )
    }

    /// The `[search]` table from the last acceptance, or the default table
    /// while `rift.toml` is invalid.
    pub(crate) fn search_configuration(&self) -> SearchConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.search.clone())
            .unwrap_or_default()
    }
}

/// Exact bounded identity of the configuration policy source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigurationFingerprint {
    /// No readable configuration file exists.
    MissingOrUnreadable,
    /// File bytes within the accepted bound.
    Content([u8; 32]),
    /// File is already invalid by size; its contents cannot change policy.
    Oversized(u64),
}

/// The current `rift.toml` file state, or null when the file is absent or
/// unreadable — either way the next acceptance decides what that means.
pub(crate) fn configuration_fingerprint(root: &Path) -> ConfigurationFingerprint {
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

impl IndexValidation {
    /// Creates one bounded invalidation stream and its receiver.
    pub(crate) fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
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
    pub(crate) fn observe(&self) -> Result<u64, ReadError> {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self.observe_locked();
        drop(publication);
        result
    }

    /// Marks watcher unhealthy and records invalidation in one critical section.
    pub(crate) fn observe_watch_failure(&self) -> Result<u64, ReadError> {
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
    pub(crate) fn observed_epoch(&self) -> u64 {
        self.observed_epoch.load(Ordering::SeqCst)
    }

    /// Installs event inclusion policy under publication linearization.
    #[cfg(test)]
    fn install_source_policy(&self, policy: Arc<WorkspaceSourcePolicy>) {
        let publication = self
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.replace_source_policy_locked(policy);
        drop(publication);
    }

    /// Replaces event inclusion policy while caller owns publication lane.
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

    /// Returns whether current policy includes one source event path.
    fn source_path_is_relevant(&self, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.as_ref().is_none_or(|policy| policy.includes(path))
    }

    /// Returns whether current policy can include source below one directory.
    fn source_directory_is_relevant(&self, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current
            .as_ref()
            .is_none_or(|policy| policy.may_include_descendant(path))
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

impl Drop for IndexValidation {
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
        self.validation.cancellation.cancel();
        let Some(mut task) = self.validation.task.lock().await.take() else {
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
pub(crate) fn workspace_watcher(
    root: &Path,
    validation: &Arc<IndexValidation>,
) -> Result<notify::RecommendedWatcher, ReadError> {
    let watched_root = std::fs::canonicalize(root)
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    let event_root = watched_root.clone();
    let validation = Arc::clone(validation);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        report_watch_outcome(&event_root, &validation, result);
    })
    .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    watcher
        .watch(&watched_root, RecursiveMode::Recursive)
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    Ok(watcher)
}

/// Observes one watcher callback: a delivered event enters the inclusion filter, and a
/// backend failure marks the watch unhealthy.
pub(crate) fn report_watch_outcome(
    root: &Path,
    validation: &IndexValidation,
    outcome: notify::Result<Event>,
) {
    let Ok(event) = outcome else {
        let _ = validation.observe_watch_failure();
        tracing::warn!(
            component = "index",
            operation = "watch.receive",
            "index watch backend reported failure"
        );
        return;
    };
    if validation.observe_event(root, &event).is_err() {
        tracing::error!(
            component = "index",
            operation = "watch.observe",
            "index watch failed"
        );
    }
}

/// Whether one native event can change visible Rust source or its policy.
pub(crate) fn relevant_watch_event(
    root: &Path,
    validation: &IndexValidation,
    event: &Event,
) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event
        .paths
        .iter()
        .filter(|path| hard_floor_includes_watch_path(root, path))
        .any(|path| watch_kind_reaches_path(root, validation, event.kind, path))
}

/// Rejects paths below Rift's hard-floor directories.
pub(crate) fn hard_floor_includes_watch_path(root: &Path, path: &Path) -> bool {
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
pub(crate) fn watch_kind_reaches_path(
    root: &Path,
    validation: &IndexValidation,
    kind: EventKind,
    path: &Path,
) -> bool {
    let workspace_configuration = validation.workspace_configuration_is_relevant(root, path);
    let gitignore = path.file_name() == Some(std::ffi::OsStr::new(".gitignore"))
        && path
            .parent()
            .is_some_and(|parent| validation.source_directory_is_relevant(parent));
    let policy_file = workspace_configuration || gitignore;
    let source_file = validation.source_path_is_relevant(path);
    let directory_event = matches!(
        kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
    ) && validation.source_directory_is_relevant(path);
    let possible_directory =
        path.extension().is_none() && validation.source_directory_is_relevant(path);
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
pub(crate) async fn initial_workspace(
    root: &Path,
    limits: WorkspaceIndexLimits,
    validation: &IndexValidation,
    blocking: &BlockingExecutor,
) -> Result<Arc<PublishedWorkspace>, ReadError> {
    initial_workspace_with(
        root,
        limits,
        validation,
        blocking,
        build_workspace_candidate,
    )
    .await
}

/// Runs the bounded capture loop over an injectable capture, so tests can
/// force each retry arm deterministically instead of racing the filesystem.
pub(crate) async fn initial_workspace_with(
    root: &Path,
    limits: WorkspaceIndexLimits,
    validation: &IndexValidation,
    blocking: &BlockingExecutor,
    capture: impl Fn(&Path, WorkspaceIndexLimits, u64) -> Result<WorkspaceCandidate, ReadError>
    + Clone
    + Send
    + 'static,
) -> Result<Arc<PublishedWorkspace>, ReadError> {
    for attempt in 1..=INDEX_CAPTURE_ATTEMPTS_MAX {
        let epoch = validation.observed_epoch();
        let build_root = root.to_path_buf();
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            trigger = "startup",
            epoch,
            attempt
        );
        let attempt_capture = capture.clone();
        let built = blocking
            .run("initial index build", move || {
                attempt_capture(&build_root, limits, epoch)
            })
            .instrument(span)
            .await?;
        let WorkspaceCandidate::Stable(built) = built else {
            continue;
        };
        let stable_epoch = validation.observed_epoch() == epoch;
        let watch_healthy = !validation.watch_failed.load(Ordering::Acquire);
        if stable_epoch && watch_healthy {
            let publication = validation
                .publication_lane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if validation.observed_epoch() != epoch
                || validation.watch_failed.load(Ordering::Acquire)
            {
                drop(publication);
                continue;
            }
            validation.replace_source_policy_locked(Arc::clone(&built.source_policy));
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
pub(crate) enum WorkspaceCandidate {
    /// Index, configuration, and source policy share one stable capture.
    Stable(Arc<PublishedWorkspace>),
    /// Configuration moved during capture.
    ConfigurationChanged,
}

/// Builds one snapshot candidate and verifies configuration around its scan.
pub(crate) fn build_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    epoch: u64,
) -> Result<WorkspaceCandidate, ReadError> {
    let configuration = ConfigurationState::accept(root);
    let visibility = configuration.source_visibility();
    let text_inclusion = configuration.text_inclusion();
    let reads = ReadService::build(root, limits, &visibility, &text_inclusion)?;
    let source_policy = WorkspaceSourcePolicy::build(root, limits, &visibility, &text_inclusion)
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

/// Derives `published`'s lexical units and replaces the lexical search database's whole
/// content with them, stamped with the same tree revision `published`'s wire answers
/// report — the exact string a search request compares its query-time lexical revision
/// against before trusting that tier's matches.
///
/// Population failure is a warning, never a request failure: the search-time revision guard
/// keeps served results honest whether or not this population landed, and the next
/// successful rebuild repopulates from scratch regardless.
///
/// # Cancel safety
///
/// `replace_all` runs inside one `SQLite` transaction. Dropping this future before that
/// transaction commits leaves the previously indexed units and tree-revision stamp fully
/// intact — never partially overwritten. That stamp then no longer names the tree revision
/// this call was populating for, so the search-time revision guard serves identifier-only
/// results until the next successful publication repopulates the lexical tier.
pub(crate) async fn populate_lexical(lexical: &LexicalSearchIndex, published: &PublishedWorkspace) {
    for (path, chunks) in published.reads.chunked_text_files() {
        tracing::warn!(
            component = "search",
            operation = "lexical.populate",
            path = %path.as_str(),
            chunks,
            "a [search.text] file exceeds max_chunk and was indexed in chunks; exclude it in \
             [source], or drop its extension from [search.text].extensions, to avoid this"
        );
    }
    let units = published.reads.lexical_units();
    let tree_revision = published.reads.tree_revision();
    if let Err(error) = lexical.replace_all(&units, tree_revision).await {
        tracing::warn!(
            component = "search",
            operation = "lexical.populate",
            tree_revision,
            error = %error,
            "lexical search index population failed; identifier search continues to serve \
             results until the next successful rebuild repopulates it"
        );
    }
}

/// Outcome of one background reconciliation attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebuildOutcome {
    /// Candidate became current and was published.
    Published,
    /// New observation invalidated candidate before publication.
    Superseded,
}

/// Owns native watcher and reconciles coalesced invalidations until shutdown.
pub(crate) async fn run_index_supervisor(
    _watcher: notify::RecommendedWatcher,
    mut invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
) {
    let IndexSupervisorContext {
        root,
        limits,
        published,
        change_lane,
        validation,
        blocking,
        lexical,
    } = context;
    loop {
        let received = tokio::select! {
            () = validation.cancellation.cancelled() => false,
            received = invalidations.recv() => received.is_some(),
        };
        if !received {
            return;
        }
        tokio::select! {
            () = validation.cancellation.cancelled() => return,
            () = tokio::time::sleep(INDEX_DEBOUNCE) => {}
        }
        let epoch = validation.observed_epoch();
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
            Arc::clone(&validation),
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
        match result {
            Ok(RebuildOutcome::Published) => {
                if let Some(lexical) = lexical.as_ref() {
                    let (current, _) = published.read().await.snapshot();
                    populate_lexical(lexical, &current).await;
                }
            }
            Ok(RebuildOutcome::Superseded) => {}
            Err(error) => {
                tracing::warn!(
                    component = "index",
                    operation = "index.build",
                    epoch,
                    error_code = error.descriptor().code(),
                    "index rebuild failed"
                );
                let failed_state = Arc::clone(&published);
                let failed_validation = Arc::clone(&validation);
                let recorded = blocking
                    .run("index failure publication", move || {
                        Ok(record_rebuild_failure(
                            &failed_state,
                            &failed_validation,
                            epoch,
                            error,
                        ))
                    })
                    .await;
                if recorded.is_err() {
                    let _ = validation.observe_watch_failure();
                }
                validation.changed.notify_waiters();
            }
        }
    }
}

/// Rebuilds and atomically publishes only a still-current candidate.
pub(crate) async fn rebuild_workspace(
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<IndexState>>,
    change_lane: Arc<ChangeLane>,
    validation: Arc<IndexValidation>,
    blocking: BlockingExecutor,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    blocking
        .run("filesystem index rebuild", move || {
            rebuild_workspace_blocking(&root, limits, &published, &change_lane, &validation, epoch)
        })
        .await
}

/// Runs one serialized filesystem rebuild on blocking executor.
pub(crate) fn rebuild_workspace_blocking(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    change_lane: &ChangeLane,
    validation: &IndexValidation,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    change_lane.run(|| rebuild_workspace_serialized(root, limits, published, validation, epoch))
}

/// Rebuilds workspace while mutation lane is held.
pub(crate) fn rebuild_workspace_serialized(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    epoch: u64,
) -> Result<RebuildOutcome, ReadError> {
    rebuild_workspace_serialized_with(
        root,
        limits,
        published,
        validation,
        epoch,
        build_workspace_candidate,
    )
}

/// Runs one serialized rebuild over an injectable capture, so tests can
/// force each superseded arm deterministically instead of racing the scan.
pub(crate) fn rebuild_workspace_serialized_with(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    epoch: u64,
    capture: impl FnOnce(&Path, WorkspaceIndexLimits, u64) -> Result<WorkspaceCandidate, ReadError>,
) -> Result<RebuildOutcome, ReadError> {
    if !accept_rebuild(validation, epoch)? {
        return Ok(RebuildOutcome::Superseded);
    }
    let candidate = capture(root, limits, epoch)?;
    let WorkspaceCandidate::Stable(candidate) = candidate else {
        let _ = validation.observe();
        return Ok(RebuildOutcome::Superseded);
    };
    let outcome = publish_rebuild(published, validation, candidate);
    if outcome == RebuildOutcome::Published {
        trace_publication(epoch);
        validation.changed.notify_waiters();
    }
    Ok(outcome)
}

/// Refuses rebuild when watcher failed or candidate epoch already moved.
pub(crate) fn accept_rebuild(validation: &IndexValidation, epoch: u64) -> Result<bool, ReadError> {
    if validation.watch_failed.load(Ordering::Acquire) {
        return Err(ReadFault::unavailable(
            "filesystem index rebuild",
            "filesystem watcher failed",
        ));
    }
    Ok(validation.observed_epoch() == epoch)
}

/// Atomically publishes candidate when observation still matches.
pub(crate) fn publish_rebuild(
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: Arc<PublishedWorkspace>,
) -> RebuildOutcome {
    publish_rebuild_after(published, validation, candidate, || {})
}

/// Publishes under observation lane; hook enables deterministic overlap tests.
pub(crate) fn publish_rebuild_after(
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: Arc<PublishedWorkspace>,
    after_state_lock: impl FnOnce(),
) -> RebuildOutcome {
    let publication = validation
        .publication_lane
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = published.blocking_write();
    after_state_lock();
    let observed_epoch = validation.observed_epoch();
    let source_policy = Arc::clone(&candidate.source_policy);
    // IndexState::publish owns the still-current check, so a superseded
    // candidate is rejected in exactly one place.
    let published = state.publish(candidate, observed_epoch);
    if published {
        validation.replace_source_policy_locked(source_policy);
    }
    drop(state);
    drop(publication);
    if published {
        RebuildOutcome::Published
    } else {
        RebuildOutcome::Superseded
    }
}

/// Records failure under same observation linearization as publication.
pub(crate) fn record_rebuild_failure(
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    epoch: u64,
    error: ReadError,
) -> bool {
    let publication = validation
        .publication_lane
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let observed_epoch = validation.observed_epoch();
    let recorded = published
        .blocking_write()
        .record_failure(epoch, observed_epoch, error);
    drop(publication);
    recorded
}

/// Emits one path-free filesystem publication event.
pub(crate) fn trace_publication(epoch: u64) {
    tracing::info!(
        component = "index",
        operation = "index.publish",
        trigger = "filesystem",
        epoch,
        "index snapshot published"
    );
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Barrier as ThreadBarrier};
    use std::time::Duration;

    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    use notify::{Event, EventKind};
    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::{
        LexicalIndexLimits, LexicalSearchIndex, WorkspaceIndexLimits, WorkspaceSourcePolicy,
    };
    use rift_server::ReadFault;
    use tokio::sync::{Barrier as AsyncBarrier, RwLock};

    use super::{
        ConfigurationFingerprint, ConfigurationState, IndexState, IndexValidation,
        PublishedWorkspace, RebuildOutcome, WorkspaceCandidate, build_workspace_candidate,
        populate_lexical, publish_rebuild, publish_rebuild_after, record_rebuild_failure,
        relevant_watch_event,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn stable_candidate(root: &std::path::Path, epoch: u64) -> TestResult<Arc<PublishedWorkspace>> {
        match build_workspace_candidate(root, WorkspaceIndexLimits::default(), epoch)? {
            WorkspaceCandidate::Stable(candidate) => Ok(candidate),
            WorkspaceCandidate::ConfigurationChanged => {
                Err("fixture configuration must remain stable".into())
            }
        }
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
        let invalid = ConfigurationState::accept(directory.path());
        assert!(invalid.accepted.is_err());
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
        let (validation, mut invalidations) = IndexValidation::new();
        let barrier = Arc::new(AsyncBarrier::new(OBSERVATIONS + 1));
        let mut tasks = Vec::with_capacity(OBSERVATIONS);
        for _ in 0..OBSERVATIONS {
            let validation = Arc::clone(&validation);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                validation.observe().map_err(|error| error.to_string())
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
        let (closed, receiver) = IndexValidation::new();
        drop(receiver);
        assert!(closed.observe().is_err());
        assert!(closed.watch_failed.load(Ordering::Acquire));

        let (exhausted, _receiver) = IndexValidation::new();
        exhausted.observed_epoch.store(u64::MAX, Ordering::Release);
        assert!(exhausted.observe().is_err());
        assert!(exhausted.watch_failed.load(Ordering::Acquire));

        let (failed, _receiver) = IndexValidation::new();
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
            &TextFileInclusion::default(),
        )?;
        let (validation, _invalidations) = IndexValidation::new();
        validation.install_source_policy(Arc::new(policy));
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));

        assert!(relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "src/lib.rs")
        ));
        assert!(
            relevant_watch_event(
                &watched_root,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), "src/guide.md")
            ),
            "an external edit to an included [search.text] extension must trigger a rebuild, \
             the same as a source file"
        );
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "src/generated/code.rs")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "src/ignored.rs")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Remove(RemoveKind::Folder), "src")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Create(CreateKind::Folder), "examples")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Remove(RemoveKind::Folder), "src/generated")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), ".gitignore")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "examples/.gitignore")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "src/rift.toml")
        ));
        assert!(!relevant_watch_event(
            &watched_root,
            &validation,
            &event(EventKind::Modify(ModifyKind::Any), "target/.gitignore")
        ));
        assert!(relevant_watch_event(
            &watched_root,
            &validation,
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
        validation: Arc<IndexValidation>,
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
        let (validation, invalidations) = IndexValidation::new();
        validation.install_source_policy(Arc::clone(&before.source_policy));
        fs::write(directory.path().join("lib.rs"), "pub fn after() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = false\n",
        )?;
        let epoch = validation.observe()?;
        let after = stable_candidate(directory.path(), epoch)?;
        Ok(PublicationFixture {
            _directory: directory,
            before,
            after,
            state,
            validation,
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
            .validation
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
        let publisher_validation = Arc::clone(&fixture.validation);
        let published_candidate = Arc::clone(&fixture.after);
        let locked = Arc::clone(&publication_locked);
        let released = Arc::clone(&publication_released);
        let publisher = std::thread::spawn(move || {
            publish_rebuild_after(
                &publisher_state,
                &publisher_validation,
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
        let observer_validation = Arc::clone(&fixture.validation);
        let observer_started = Arc::clone(&observation_started);
        let observer = std::thread::spawn(move || {
            observer_started.wait();
            observer_validation.observe()
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
        assert_eq!(fixture.validation.observed_epoch(), 2);
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
        let (validation, _invalidations) = IndexValidation::new();
        validation.install_source_policy(Arc::clone(&before.source_policy));
        assert_eq!(validation.observe()?, 1);
        assert_eq!(validation.observe()?, 2);
        assert_eq!(
            publish_rebuild(&state, &validation, Arc::clone(&after)),
            RebuildOutcome::Superseded
        );
        let state_snapshot = state.blocking_read();
        assert_workspace_identity(&state_snapshot.current, &before);
        drop(state_snapshot);
        let source_policy = validation
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
            &validation,
            1,
            ReadFault::unavailable("test rebuild", "superseded failure")
        ));
        assert!(state.blocking_read().failure.is_none());
        Ok(())
    }

    #[test]
    fn watch_backend_failure_marks_the_watch_unhealthy() {
        let (validation, _receiver) = IndexValidation::new();
        let root = std::path::Path::new("/rift-workspace");
        super::report_watch_outcome(
            root,
            &validation,
            Err(notify::Error::generic("test backend failure")),
        );
        assert!(validation.watch_failed.load(Ordering::Acquire));
    }

    #[test]
    fn watch_event_after_supervisor_loss_marks_the_watch_unhealthy() {
        let (validation, receiver) = IndexValidation::new();
        drop(receiver);
        let root = std::path::Path::new("/rift-workspace");
        let event = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.join("rift.toml"));
        super::report_watch_outcome(root, &validation, Ok(event));
        assert!(validation.watch_failed.load(Ordering::Acquire));
    }

    #[test]
    fn access_events_never_reach_inclusion() {
        use notify::event::AccessKind;
        let (validation, _receiver) = IndexValidation::new();
        let root = std::path::Path::new("/rift-workspace");
        let path = root.join("lib.rs");
        let event = Event::new(EventKind::Access(AccessKind::Any)).add_path(path.clone());
        assert!(!relevant_watch_event(root, &validation, &event));
        assert!(!super::watch_kind_reaches_path(
            root,
            &validation,
            EventKind::Access(AccessKind::Any),
            &path
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_configuration_fingerprints_as_missing() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rift.toml");
        fs::write(&path, "[providers.history]\nenabled = true\n")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
        let fingerprint = super::configuration_fingerprint(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert_eq!(fingerprint, ConfigurationFingerprint::MissingOrUnreadable);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_configuration_bytes_fingerprint_as_missing() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("rift.toml"))?;
        assert_eq!(
            super::configuration_fingerprint(directory.path()),
            ConfigurationFingerprint::MissingOrUnreadable
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_aborts_a_supervisor_that_misses_its_deadline() -> TestResult {
        let (validation, _receiver) = IndexValidation::new();
        let stuck = tokio::spawn(std::future::pending::<()>());
        *validation.task.lock().await = Some(stuck);
        let supervisor = super::IndexSupervisor {
            validation: Arc::clone(&validation),
        };
        let error = supervisor
            .shutdown()
            .await
            .expect_err("a stuck supervisor must miss the shutdown deadline");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        supervisor
            .shutdown()
            .await
            .map_err(|error| format!("second shutdown must be idempotent: {error:?}"))?;
        Ok(())
    }

    #[test]
    fn rebuild_is_superseded_when_epoch_already_moved() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn before() {}\n")?;
        let (validation, _receiver) = IndexValidation::new();
        let candidate = stable_candidate(directory.path(), 0)?;
        let state = RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let outcome = super::rebuild_workspace_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            7,
        )?;
        assert_eq!(outcome, RebuildOutcome::Superseded);
        Ok(())
    }

    #[test]
    fn rebuild_is_superseded_when_configuration_moves_during_capture() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn before() {}\n")?;
        let (validation, _receiver) = IndexValidation::new();
        let candidate = stable_candidate(directory.path(), 0)?;
        let state = RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let outcome = super::rebuild_workspace_serialized_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            0,
            |_, _, _| Ok(WorkspaceCandidate::ConfigurationChanged),
        )?;
        assert_eq!(outcome, RebuildOutcome::Superseded);
        assert_eq!(
            validation.observed_epoch(),
            1,
            "a moved configuration must trigger another observation"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_marks_the_watch_unhealthy_when_blocking_work_is_gone() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, invalidations) = IndexValidation::new();
        let current = stable_candidate(directory.path(), 0)?;
        let published = Arc::new(RwLock::new(IndexState {
            current,
            failure: None,
        }));
        let watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;
        let blocking = crate::server::BlockingExecutor::isolated(1, 60_000);
        blocking.operations.close();
        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            super::IndexSupervisorContext {
                root: directory.path().to_path_buf(),
                limits: WorkspaceIndexLimits::default(),
                published: Arc::clone(&published),
                change_lane: Arc::new(crate::server::ChangeLane::default()),
                validation: Arc::clone(&validation),
                blocking,
                lexical: None,
            },
        ));
        let notified = validation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        validation
            .observe()
            .map_err(|error| format!("observation must land: {error:?}"))?;
        notified.as_mut().await;
        assert!(
            validation.watch_failed.load(Ordering::Acquire),
            "a supervisor that cannot run blocking work must mark the watch unhealthy"
        );
        validation.cancellation.cancel();
        supervisor.await?;
        Ok(())
    }

    #[test]
    fn rebuild_acceptance_fails_after_watcher_failure() {
        let (validation, receiver) = IndexValidation::new();
        drop(receiver);
        let _ = validation.observe_watch_failure();
        let error = super::accept_rebuild(&validation, 0)
            .expect_err("a failed watcher must refuse rebuild acceptance");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_records_rebuild_failure_and_notifies_waiters() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, invalidations) = IndexValidation::new();
        let current = stable_candidate(directory.path(), 0)?;
        let published = Arc::new(RwLock::new(IndexState {
            current,
            failure: None,
        }));
        let watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;
        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            super::IndexSupervisorContext {
                root: directory.path().join("vanished"),
                limits: WorkspaceIndexLimits::default(),
                published: Arc::clone(&published),
                change_lane: Arc::new(crate::server::ChangeLane::default()),
                validation: Arc::clone(&validation),
                blocking: crate::server::BlockingExecutor::isolated(2, 60_000),
                lexical: None,
            },
        ));
        let notified = validation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let epoch = validation
            .observe()
            .map_err(|error| format!("observation must land: {error:?}"))?;
        notified.as_mut().await;
        let state = published.read().await;
        let (_, failure) = state.snapshot();
        drop(state);
        let (failed_epoch, error) = failure.ok_or("rebuild failure must be recorded")?;
        assert_eq!(failed_epoch, epoch);
        // A vanished root refuses at canonical-root resolution.
        assert_eq!(error.descriptor().code(), "configuration_invalid");
        validation.cancellation.cancel();
        supervisor.await?;
        Ok(())
    }

    #[tokio::test]
    async fn initial_capture_fails_after_bounded_attempts_of_epoch_movement() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) = IndexValidation::new();
        let blocking = crate::server::BlockingExecutor::isolated(2, 60_000);
        let moving = Arc::clone(&validation);
        let error = super::initial_workspace_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &validation,
            &blocking,
            move |root, limits, epoch| {
                // Every capture observes one more filesystem event, so no
                // attempt ever sees a stable epoch.
                moving.observe()?;
                super::build_workspace_candidate(root, limits, epoch)
            },
        )
        .await
        .expect_err("movement during every capture must exhaust bounded attempts");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        Ok(())
    }

    #[tokio::test]
    async fn initial_capture_fails_while_configuration_keeps_moving() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) = IndexValidation::new();
        let blocking = crate::server::BlockingExecutor::isolated(2, 60_000);
        let error = super::initial_workspace_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &validation,
            &blocking,
            |_, _, _| Ok(WorkspaceCandidate::ConfigurationChanged),
        )
        .await
        .expect_err("a configuration that keeps moving must exhaust capture attempts");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        Ok(())
    }

    #[tokio::test]
    async fn populate_lexical_persists_every_chunk_of_an_oversized_text_file() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // The enforced minimum `max_chunk` against a several-kilobyte guide forces the file
        // into more than one lexical chunk.
        let rift_toml = directory.path().join("rift.toml");
        fs::write(rift_toml, "[search.text]\nmax_chunk = \"1kb\"\n")?;
        fs::write(directory.path().join("guide.md"), "word ".repeat(1000))?;
        let published = stable_candidate(directory.path(), 0)?;

        let chunked = published.reads.chunked_text_files();
        let chunk_count = chunked
            .iter()
            .find(|(path, _)| path.as_str() == "guide.md")
            .map(|(_, count)| *count)
            .ok_or("guide.md must be reported as chunked before population runs")?;
        assert!(
            chunk_count > 1,
            "the oversized guide must split into more than one chunk: {chunk_count}"
        );

        let units = published.reads.lexical_units();
        let guide_units = units
            .iter()
            .filter(|unit| unit.path().as_str() == "guide.md")
            .count();
        assert!(
            guide_units > 1,
            "the oversized file must contribute more than one lexical unit: {guide_units}"
        );

        let lexical = LexicalSearchIndex::open(
            &directory.path().join("lexical.db"),
            LexicalIndexLimits::default(),
        )
        .await?;
        populate_lexical(&lexical, &published).await;

        let stamped = lexical.tree_revision().await?;
        assert_eq!(
            stamped.as_deref(),
            Some(published.reads.tree_revision()),
            "population must succeed and stamp the published tree revision, not merely warn"
        );
        for unit in &units {
            let content = lexical.content(unit.identity()).await?;
            assert!(
                content.is_some(),
                "every chunk unit must have been persisted: identity={}",
                unit.identity()
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn supervisor_dispatches_superseded_when_epoch_moves_before_admission() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, invalidations) = IndexValidation::new();
        let current = stable_candidate(directory.path(), 0)?;
        let published = Arc::new(RwLock::new(IndexState {
            current,
            failure: None,
        }));
        let watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;

        // One blocking slot, held by a placeholder so the supervisor's own rebuild for
        // epoch 1 is forced to queue behind it — a deterministic gate between the
        // supervisor capturing that epoch and `accept_rebuild` checking it, exactly where a
        // second observation must land to supersede the rebuild.
        let blocking = crate::server::BlockingExecutor::isolated(1, 60_000);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::sync_channel::<()>(0);
        let held_blocking = blocking.clone();
        let held = tokio::spawn(async move {
            held_blocking
                .run("held placeholder", move || {
                    let _ = started_sender.send(());
                    release_receiver
                        .recv()
                        .expect("test must release the held placeholder");
                    Ok::<(), rift_server::ReadError>(())
                })
                .await
        });
        started_receiver
            .await
            .expect("held placeholder must occupy the one blocking slot");

        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            super::IndexSupervisorContext {
                root: directory.path().to_path_buf(),
                limits: WorkspaceIndexLimits::default(),
                published: Arc::clone(&published),
                change_lane: Arc::new(crate::server::ChangeLane::default()),
                validation: Arc::clone(&validation),
                blocking: blocking.clone(),
                lexical: None,
            },
        ));

        let first_epoch = validation
            .observe()
            .map_err(|error| format!("first observation must land: {error:?}"))?;
        assert_eq!(first_epoch, 1);
        // Gives the supervisor's debounce (50ms) real wall-clock time to elapse and its
        // rebuild to reach and queue behind the held placeholder. The held slot — not this
        // wait — is what guarantees the rebuild cannot be accepted until released; a
        // slower scheduler just makes this wait less generous, never wrong.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Moves the epoch again with no invalidation signal, so no second rebuild cycle is
        // ever triggered — only the already-queued rebuild for epoch 1 observes this move.
        let moved = validation.observed_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(moved, 2);

        release_sender
            .send(())
            .expect("held placeholder must accept release");
        held.await??;

        // A sentinel operation through the same one-slot executor only completes once the
        // supervisor's own queued rebuild has acquired, run, and released that slot —
        // proving its `accept_rebuild` check (and therefore the Superseded verdict) already
        // landed, without hoping a fixed sleep was long enough.
        blocking
            .run("sentinel", || Ok::<(), rift_server::ReadError>(()))
            .await?;

        let state = published.read().await;
        let (snapshot, failure) = state.snapshot();
        assert_eq!(
            snapshot.epoch, 0,
            "a superseded rebuild must publish nothing"
        );
        assert!(
            failure.is_none(),
            "a superseded rebuild must not record a failure either"
        );
        drop(state);

        validation.cancellation.cancel();
        supervisor.await?;
        Ok(())
    }
}
