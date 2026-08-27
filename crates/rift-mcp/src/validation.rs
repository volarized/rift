//! Current-index validation: filesystem observation, serialized rebuilds,
//! and atomic publication of the workspace snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock as SyncRwLock};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use rift_core::ProjectPath;
use rift_core::constants::{
    VCS_IGNORE_FILE, WORKSPACE_CONFIGURATION_FILE, WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{
    ChangeSet, FileDigest, LexicalChange, LexicalUnit, PathChanges, WorkspaceFingerprint,
    WorkspaceIndexLimits, WorkspaceSourcePolicy,
};
use rift_protocol::configuration::{
    EngineConfiguration, HistoryConfiguration, LogsConfiguration, SearchConfiguration,
    ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_search::{Embedding, SearchError, SearchIndex};
use rift_server::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, ReadError, ReadFault, ReadService,
    load_configuration,
};
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::failure::WireFailure;
use crate::server::{BlockingExecutor, ChangeLane};

/// Filesystem events coalesced while one rebuild is pending.
pub(crate) const INDEX_INVALIDATIONS_MAX: usize = 1;
/// Delay collecting one bounded filesystem-event batch.
pub(crate) const INDEX_DEBOUNCE: Duration = Duration::from_millis(50);
/// Deadline for joining the index supervisor during shutdown.
pub(crate) const INDEX_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Complete capture retries while the tree keeps moving.
pub(crate) const INDEX_CAPTURE_ATTEMPTS_MAX: usize = 3;

/// What the next rebuild must cover, accumulated between publications.
///
/// A watcher event and a change tool both name paths, so an ordinary rebuild reads only
/// what moved. An observation that names no trustworthy path set - a watch failure, a
/// `.gitignore` or `rift.toml` write, a directory appearing or disappearing, or more
/// retained paths than the workspace's own file bound allows - asks for the whole
/// workspace instead, and no later path narrows that back down.
#[derive(Debug, Default)]
pub(crate) struct PendingWork {
    paths: BTreeSet<ProjectPath>,
    whole_workspace: bool,
}

impl PendingWork {
    /// The observation a caller makes when it cannot name what moved.
    pub(crate) const fn whole_workspace() -> Self {
        Self {
            paths: BTreeSet::new(),
            whole_workspace: true,
        }
    }

    /// The observation a caller makes when it knows exactly which files moved.
    #[cfg(test)]
    pub(crate) fn naming(paths: impl IntoIterator<Item = ProjectPath>) -> Self {
        Self {
            paths: paths.into_iter().collect(),
            whole_workspace: false,
        }
    }

    /// Whether this observation asks for every visible file to be read again.
    pub(crate) const fn covers_whole_workspace(&self) -> bool {
        self.whole_workspace
    }

    /// The paths this observation retains, in project-path order.
    #[cfg(test)]
    pub(crate) fn paths(&self) -> impl Iterator<Item = &ProjectPath> {
        self.paths.iter()
    }

    /// Retains `paths` for the next rebuild, or escalates to the whole workspace once
    /// more paths are retained than the workspace may hold files.
    fn retain(&mut self, paths: impl IntoIterator<Item = ProjectPath>, paths_max: usize) {
        if self.whole_workspace {
            return;
        }
        self.paths.extend(paths);
        if self.paths.len() > paths_max {
            self.escalate();
        }
    }

    /// Drops the retained paths and asks for the whole workspace.
    fn escalate(&mut self) {
        self.whole_workspace = true;
        self.paths.clear();
    }

    /// Takes back the paths one superseded attempt drained, beside whatever landed while
    /// it ran. Publication is the acknowledgement that lets them be dropped, so an attempt
    /// that never published returns its work here.
    fn absorb(&mut self, other: Self, paths_max: usize) {
        if other.whole_workspace {
            self.escalate();
            return;
        }
        self.retain(other.paths, paths_max);
    }
}

/// One rebuild's inputs: the observation it answers, and the publication it may share
/// unchanged files with.
pub(crate) struct RebuildRequest {
    /// The filesystem-event epoch this rebuild answers for.
    pub(crate) epoch: u64,
    /// What the observation asked for.
    pub(crate) work: PendingWork,
    /// The current publication, absent only at startup, when nothing is published yet.
    pub(crate) previous: Option<Arc<PublishedWorkspace>>,
}

impl RebuildRequest {
    /// The rebuild startup runs: every visible file, with nothing to share.
    pub(crate) const fn initial(epoch: u64) -> Self {
        Self {
            epoch,
            work: PendingWork::whole_workspace(),
            previous: None,
        }
    }

    /// Resolves this observation into the change set the candidate builds from.
    ///
    /// A publication accepted under other configuration bytes cannot lend its files: the
    /// `[source]` policy that selected them may itself have changed, so that rebuild reads
    /// the whole workspace. A path whose bytes cannot be read for any reason other than
    /// its absence does the same, because a whole scan is what decides whether that path
    /// is a refusal or a removal.
    fn change_set(&self, root: &Path, configuration: &ConfigurationState) -> ChangeSet {
        let Some(previous) = self.previous.as_ref() else {
            return ChangeSet::Full;
        };
        if self.work.whole_workspace
            || previous.configuration.fingerprint != configuration.fingerprint
        {
            return ChangeSet::Full;
        }
        let Some(observed) = observed_digests(root, &self.work.paths, &previous.source_policy)
        else {
            return ChangeSet::Full;
        };
        ChangeSet::Incremental(PathChanges::resolve(observed, |path| {
            previous.reads.file_digest(path)
        }))
    }
}

/// Reads each observed path's current bytes into the digest one change set compares
/// against, or nothing when a read failed for a reason other than the path being gone.
///
/// A path the workspace's policy no longer includes reads as absent, so an excluded file
/// leaves the index exactly as a deleted one does.
fn observed_digests(
    root: &Path,
    paths: &BTreeSet<ProjectPath>,
    policy: &WorkspaceSourcePolicy,
) -> Option<Vec<(ProjectPath, Option<FileDigest>)>> {
    let mut observed = Vec::with_capacity(paths.len());
    for path in paths {
        let absolute = root.join(path.as_str());
        if !policy.includes(&absolute) {
            observed.push((path.clone(), None));
            continue;
        }
        match std::fs::read(&absolute) {
            Ok(bytes) => observed.push((path.clone(), Some(FileDigest::of(&bytes)))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                observed.push((path.clone(), None));
            }
            Err(_) => return None,
        }
    }
    Some(observed)
}

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
    /// The publication linearization point, holding the work the next rebuild owes.
    /// Observation and publication both take it, so a path observed between a rebuild's
    /// capture and its publication cannot be lost.
    pub(crate) publication_lane: SyncMutex<PendingWork>,
    /// How many paths one observation may retain before it escalates to the whole
    /// workspace. The workspace's own file bound: retaining more paths than the workspace
    /// may hold files is a whole rebuild by another name.
    paths_max: usize,
    pub(crate) source_policy: SyncRwLock<Option<Arc<WorkspaceSourcePolicy>>>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) task: AsyncMutex<Option<JoinHandle<()>>>,
    /// Whether the index supervisor is still running.
    ///
    /// The supervisor is the only writer of published snapshots. If it ends -
    /// cancelled, or unwound by a panic in a rebuild - the observed epoch keeps
    /// advancing with every filesystem event and nothing ever publishes again,
    /// so every read waits its whole readiness budget and refuses. That was
    /// silent: the flag makes it a named refusal on the first request instead
    /// of a timeout on every one.
    pub(crate) supervisor_running: Arc<AtomicBool>,
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
    /// The workspace's population lane, absent when the search index could not be opened
    /// at startup.
    pub(crate) population: Option<PopulationLane>,
    /// The workspace's lexical lane, absent exactly when the population lane is: with no
    /// index open there is no store to commit to, and `search` reports the tier
    /// unavailable for the life of this server.
    pub(crate) lexical: Option<LexicalLane>,
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

    /// Whether the last acceptance of `rift.toml` succeeded.
    ///
    /// Every table accessor below answers the shipped default while it did not, so a
    /// caller that must tell "the operator asked for this" from "nobody could read what
    /// the operator asked for" reads this first.
    pub(crate) const fn is_accepted(&self) -> bool {
        self.accepted.is_ok()
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

    /// The `[providers.history]` table from the last acceptance, or the
    /// default table while `rift.toml` is invalid.
    pub(crate) fn history_configuration(&self) -> HistoryConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.providers.history.clone())
            .unwrap_or_default()
    }

    /// The `[search]` table from the last acceptance, or the default table
    /// while `rift.toml` is invalid.
    pub(crate) fn search_configuration(&self) -> SearchConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.search.clone())
            .unwrap_or_default()
    }

    /// The `[logs]` table from the last acceptance, or the default table while
    /// `rift.toml` is invalid. A `rift://logs` read is answered under this
    /// table exactly when the file the read is meant to explain is the one that
    /// failed acceptance, so the default has to serve that case.
    pub(crate) fn logs_configuration(&self) -> LogsConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.logs.clone())
            .unwrap_or_default()
    }

    /// The `[engines]` tables from the last acceptance, or no engines while
    /// `rift.toml` is invalid.
    pub(crate) fn engines_configuration(&self) -> BTreeMap<String, EngineConfiguration> {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.engines.clone())
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
/// unreadable - either way the next acceptance decides what that means.
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
    pub(crate) fn new(paths_max: usize) -> (Arc<Self>, mpsc::Receiver<()>) {
        let (invalidations, receiver) = mpsc::channel(INDEX_INVALIDATIONS_MAX);
        (
            Arc::new(Self {
                observed_epoch: Arc::new(AtomicU64::new(0)),
                watch_failed: Arc::new(AtomicBool::new(false)),
                supervisor_running: Arc::new(AtomicBool::new(true)),
                invalidations,
                changed: Arc::new(Notify::new()),
                publication_lane: SyncMutex::new(PendingWork::default()),
                paths_max,
                source_policy: SyncRwLock::new(None),
                cancellation: CancellationToken::new(),
                task: AsyncMutex::new(None),
            }),
            receiver,
        )
    }

    /// Records one observation that names no path, so the next rebuild reads every visible
    /// file.
    pub(crate) fn observe_whole_workspace(&self) -> Result<u64, ReadError> {
        let mut publication = self.locked_pending();
        publication.escalate();
        let result = self.observe_locked(&mut publication);
        drop(publication);
        result
    }

    /// Records one observation naming exactly the paths whose bytes may have moved.
    pub(crate) fn observe_paths(
        &self,
        paths: impl IntoIterator<Item = ProjectPath>,
    ) -> Result<u64, ReadError> {
        let mut publication = self.locked_pending();
        publication.retain(paths, self.paths_max);
        let result = self.observe_locked(&mut publication);
        drop(publication);
        result
    }

    /// Marks watcher unhealthy and records invalidation in one critical section.
    pub(crate) fn observe_watch_failure(&self) -> Result<u64, ReadError> {
        let mut publication = self.locked_pending();
        self.watch_failed.store(true, Ordering::Release);
        publication.escalate();
        let result = self.observe_locked(&mut publication);
        drop(publication);
        result
    }

    /// Takes the work the next rebuild owes, with the epoch it answers for, under the one
    /// lane observation also takes.
    pub(crate) fn take_pending(&self) -> RebuildRequest {
        let mut publication = self.locked_pending();
        let work = std::mem::take(&mut *publication);
        let epoch = self.observed_epoch();
        drop(publication);
        RebuildRequest {
            epoch,
            work,
            previous: None,
        }
    }

    /// Returns one superseded or failed attempt's work, so the next rebuild covers it
    /// beside whatever landed while that attempt ran.
    pub(crate) fn restore_pending(&self, work: PendingWork) {
        let mut publication = self.locked_pending();
        publication.absorb(work, self.paths_max);
        drop(publication);
    }

    /// Enters the publication lane, taking the pending work it guards.
    fn locked_pending(&self) -> std::sync::MutexGuard<'_, PendingWork> {
        self.publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records one invalidation while caller owns publication lane.
    fn observe_locked(&self, pending: &mut PendingWork) -> Result<u64, ReadError> {
        let previous = self
            .observed_epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            })
            .map_err(|_| {
                self.watch_failed.store(true, Ordering::Release);
                pending.escalate();
                ReadFault::unavailable("index observation", "filesystem event epoch exhausted")
            })?;
        let epoch = previous + 1;
        match self.invalidations.try_send(()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.watch_failed.store(true, Ordering::Release);
                pending.escalate();
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
        let publication = self.locked_pending();
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

    /// Classifies and observes one event within the publication critical section, so the
    /// paths it names cannot be lost between the classification and the epoch that
    /// promises to cover them.
    fn observe_event(&self, root: &Path, event: &Event) -> Result<Option<u64>, ReadError> {
        let mut publication = self.locked_pending();
        let result = match watch_event_impact(root, self, event) {
            WatchImpact::None => Ok(None),
            WatchImpact::WholeWorkspace => {
                publication.escalate();
                self.observe_locked(&mut publication).map(Some)
            }
            WatchImpact::Paths(paths) => {
                publication.retain(paths, self.paths_max);
                self.observe_locked(&mut publication).map(Some)
            }
        };
        drop(publication);
        result
    }

    /// The project path one event path names, under the current inclusion policy.
    fn source_project_path(&self, path: &Path) -> Option<ProjectPath> {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.as_ref()?.project_path(path)
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

    /// Whether writing this path changes what the workspace includes.
    ///
    /// Before the first publication installs a policy there is nothing to ask, so only the
    /// root `rift.toml` and a `.gitignore` are taken as inclusion deciders; a published
    /// policy answers for its own root spellings and excluded directories.
    pub(crate) fn decides_inclusion(&self, root: &Path, path: &Path) -> bool {
        let current = self
            .source_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current.as_ref().map_or_else(
            || {
                path == root.join(WORKSPACE_CONFIGURATION_FILE)
                    || path.file_name() == Some(std::ffi::OsStr::new(VCS_IGNORE_FILE))
            },
            |policy| policy.decides_inclusion(path),
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

/// Marks the index supervisor running for as long as it is held.
///
/// Dropping it - on return, on cancellation, or while a panic unwinds the task -
/// clears the flag and records the end, so a request that finds no publication
/// coming can say so instead of waiting.
struct SupervisorRunning {
    running: Arc<AtomicBool>,
}

impl SupervisorRunning {
    fn new(running: Arc<AtomicBool>) -> Self {
        running.store(true, Ordering::Release);
        Self { running }
    }
}

impl Drop for SupervisorRunning {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        tracing::warn!(
            component = "index",
            operation = "index.supervisor",
            "the index supervisor stopped; no further snapshot publishes in this process"
        );
    }
}

/// What one native event tells the supervisor about the next rebuild.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum WatchImpact {
    /// The event cannot change the index.
    #[default]
    None,
    /// The event names visible files, and only those files need reading again.
    Paths(Vec<ProjectPath>),
    /// The event changes what the workspace includes, or reshapes a directory, so the
    /// next rebuild reads every visible file.
    WholeWorkspace,
}

impl WatchImpact {
    /// Folds one path's impact into the event's, keeping the widest one seen.
    fn absorb(self, other: Self) -> Self {
        match (self, other) {
            (Self::WholeWorkspace, _) | (_, Self::WholeWorkspace) => Self::WholeWorkspace,
            (Self::Paths(mut held), Self::Paths(more)) => {
                held.extend(more);
                Self::Paths(held)
            }
            (Self::Paths(paths), Self::None) | (Self::None, Self::Paths(paths)) => {
                Self::Paths(paths)
            }
            (Self::None, Self::None) => Self::None,
        }
    }
}

/// What one native event asks the next rebuild to cover.
pub(crate) fn watch_event_impact(
    root: &Path,
    validation: &IndexValidation,
    event: &Event,
) -> WatchImpact {
    if matches!(event.kind, EventKind::Access(_)) {
        return WatchImpact::None;
    }
    event
        .paths
        .iter()
        .filter(|path| hard_floor_includes_watch_path(root, path))
        .map(|path| watch_path_impact(root, validation, event.kind, path))
        .fold(WatchImpact::None, WatchImpact::absorb)
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

/// What one event path asks for, without trusting editor-specific event shapes.
///
/// A policy file rewrites what the workspace includes and a directory event can add or
/// drop many files at once, so both ask for the whole workspace. Only a path that is
/// itself a visible file narrows the next rebuild to that file.
pub(crate) fn watch_path_impact(
    root: &Path,
    validation: &IndexValidation,
    kind: EventKind,
    path: &Path,
) -> WatchImpact {
    let policy_file = validation.decides_inclusion(root, path);
    let directory_event = matches!(
        kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
    ) && validation.source_directory_is_relevant(path);
    let possible_directory =
        path.extension().is_none() && validation.source_directory_is_relevant(path);
    let reshapes_tree = match kind {
        EventKind::Create(_) | EventKind::Remove(_) => directory_event,
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any | EventKind::Other => {
            possible_directory
        }
        EventKind::Modify(_) | EventKind::Access(_) => false,
    };
    if matches!(kind, EventKind::Access(_)) {
        return WatchImpact::None;
    }
    if policy_file || reshapes_tree {
        return WatchImpact::WholeWorkspace;
    }
    if !validation.source_path_is_relevant(path) {
        return WatchImpact::None;
    }
    validation
        .source_project_path(path)
        .map_or(WatchImpact::WholeWorkspace, |path| {
            WatchImpact::Paths(vec![path])
        })
}

/// Builds the first snapshot while rejecting concurrent filesystem movement, and returns
/// what the lexical index owes for it.
///
/// The write is not committed here: the search database lives under the workspace's own
/// `.rift` directory, which opens only once this scan proves the root real. The caller
/// commits it before it installs the snapshot as current.
pub(crate) async fn initial_workspace(
    root: &Path,
    limits: WorkspaceIndexLimits,
    validation: &IndexValidation,
    blocking: &BlockingExecutor,
) -> Result<(Arc<PublishedWorkspace>, LexicalWrite), ReadError> {
    initial_workspace_with(
        root,
        limits,
        validation,
        blocking,
        build_workspace_candidate,
    )
    .await
}

/// One candidate capture: the whole-workspace scan, or a test's stand-in for it.
pub(crate) trait CaptureWorkspace:
    Fn(&Path, WorkspaceIndexLimits, &RebuildRequest) -> Result<WorkspaceCandidate, ReadError>
{
}

impl<Capture> CaptureWorkspace for Capture where
    Capture:
        Fn(&Path, WorkspaceIndexLimits, &RebuildRequest) -> Result<WorkspaceCandidate, ReadError>
{
}

/// Runs the bounded capture loop over an injectable capture, so tests can
/// force each retry arm deterministically instead of racing the filesystem.
pub(crate) async fn initial_workspace_with(
    root: &Path,
    limits: WorkspaceIndexLimits,
    validation: &IndexValidation,
    blocking: &BlockingExecutor,
    capture: impl CaptureWorkspace + Clone + Send + 'static,
) -> Result<(Arc<PublishedWorkspace>, LexicalWrite), ReadError> {
    for attempt in 1..=INDEX_CAPTURE_ATTEMPTS_MAX {
        let request = validation.take_pending();
        let epoch = request.epoch;
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
                let candidate =
                    attempt_capture(&build_root, limits, &RebuildRequest::initial(epoch))?;
                let WorkspaceCandidate::Stable {
                    published,
                    change_set,
                } = candidate
                else {
                    return Ok(None);
                };
                let write = lexical_write(&published, &change_set);
                Ok(Some((published, write)))
            })
            .instrument(span)
            .await?;
        let Some((built, write)) = built else {
            continue;
        };
        let stable_epoch = validation.observed_epoch() == epoch;
        let watch_healthy = !validation.watch_failed.load(Ordering::Acquire);
        if stable_epoch && watch_healthy {
            let mut publication = validation.locked_pending();
            if validation.observed_epoch() != epoch
                || validation.watch_failed.load(Ordering::Acquire)
            {
                publication.escalate();
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
            return Ok((built, write));
        }
    }
    Err(ReadFault::unavailable(
        "initial index build",
        "workspace kept changing across bounded capture attempts",
    ))
}

/// Result of one bounded workspace candidate capture.
pub(crate) enum WorkspaceCandidate {
    /// Index, configuration, and source policy share one stable capture, alongside the
    /// change set that produced it - which is what the lexical index applies before this
    /// candidate publishes.
    Stable {
        /// The candidate publication.
        published: Arc<PublishedWorkspace>,
        /// What this candidate replaced, in the shape the lexical index consumes.
        change_set: ChangeSet,
    },
    /// Configuration moved during capture.
    ConfigurationChanged,
}

/// Builds one snapshot candidate and verifies configuration around its scan.
///
/// An incremental request reads only the paths its change set names and shares every other
/// file, its `[source]` policy, and its acceptance with the publication it was resolved
/// against; a full request scans the workspace. Either way the acceptance is verified
/// against `rift.toml` after the read, so a candidate built under configuration that moved
/// underneath it is reported as changed rather than published.
pub(crate) fn build_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    request: &RebuildRequest,
) -> Result<WorkspaceCandidate, ReadError> {
    let configuration = ConfigurationState::accept(root);
    let change_set = request.change_set(root, &configuration);
    let candidate = match &change_set {
        ChangeSet::Full => whole_workspace_candidate(root, limits, configuration, request.epoch)?,
        ChangeSet::Incremental(changes) => {
            let previous = request
                .previous
                .as_ref()
                .unwrap_or_else(|| unreachable!("an incremental change set names a publication"));
            shared_workspace_candidate(previous, changes, request.epoch)?
        }
    };
    if candidate.configuration.fingerprint != configuration_fingerprint(root) {
        return Ok(WorkspaceCandidate::ConfigurationChanged);
    }
    Ok(WorkspaceCandidate::Stable {
        published: Arc::new(candidate),
        change_set,
    })
}

/// What one candidate owes the lexical index before it publishes.
///
/// A whole rebuild cannot name the difference against the stored set, so it replaces that
/// set; a change set names exactly the paths whose rows move. Deriving the units is the
/// candidate's own work, so it runs beside the parse rather than inside the commit.
pub(crate) fn lexical_write(
    published: &PublishedWorkspace,
    change_set: &ChangeSet,
) -> LexicalWrite {
    match change_set {
        ChangeSet::Full => LexicalWrite::Whole(published.reads.lexical_units()),
        ChangeSet::Incremental(changes) => {
            LexicalWrite::Change(published.reads.lexical_change(changes))
        }
    }
}

/// Scans every visible file, taking the `[source]` policy `reads` already compiled
/// rather than compiling a second one - one predicate per snapshot, one walk of the
/// tree's `.gitignore` files instead of two.
fn whole_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    configuration: ConfigurationState,
    epoch: u64,
) -> Result<PublishedWorkspace, ReadError> {
    let visibility = configuration.source_visibility();
    let text_inclusion = configuration.text_inclusion();
    let reads = ReadService::build(
        root,
        limits,
        &visibility,
        &text_inclusion,
        configuration.history_configuration(),
    )?;
    let source_policy = reads.source_policy_handle().unwrap_or_else(|| {
        unreachable!("a current-tree read service always compiles its source policy")
    });
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy,
        epoch,
    })
}

/// Replaces the files `changes` names and shares every other file with `previous`.
///
/// The acceptance and the compiled `[source]` policy carry over unchanged: an incremental
/// change set is only resolved when `rift.toml` still holds the bytes `previous` was
/// accepted under, and a `.gitignore` write asks for the whole workspace instead. An empty
/// change set still produces a candidate, because the observation that resolved to it has
/// an epoch that current-tree requests are waiting on.
fn shared_workspace_candidate(
    previous: &PublishedWorkspace,
    changes: &PathChanges,
    epoch: u64,
) -> Result<PublishedWorkspace, ReadError> {
    if changes.is_empty() {
        return Ok(PublishedWorkspace {
            reads: Arc::clone(&previous.reads),
            configuration: previous.configuration.clone(),
            fingerprint: previous.fingerprint.clone(),
            source_policy: Arc::clone(&previous.source_policy),
            epoch,
        });
    }
    let reads = previous.reads.rebuilt(changes)?;
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration: previous.configuration.clone(),
        source_policy: Arc::clone(&previous.source_policy),
        epoch,
    })
}

/// Embeds the declarations `published` describes, so the semantic tier ranks the tree the
/// lexical index already holds.
///
/// The lexical set is not written here. It commits before `published` becomes current, so a
/// request that captured this publication reads rows stamped with its own tree revision;
/// embedding runs after, because one declaration's vector can cost more than the whole
/// freshness wait a request is bounded by.
///
/// `Embedding::Every` establishes the vector set and `Embedding::Missing` trusts what is
/// stored. A store this process found on disk was written by an earlier one, possibly under
/// another model, so the first pass of a run establishes rather than trusts.
///
/// Population failure is a warning, never a request failure: the semantic tier reports its
/// own readiness, and the next successful publication asks for another pass.
///
/// # Cancel safety
///
/// Dropping this future keeps every vector already written, and the next pass embeds what
/// is still missing.
pub(crate) async fn populate_search(
    index: &SearchIndex,
    published: &PublishedWorkspace,
    embedding: Embedding,
) {
    for (path, chunks) in published.reads.chunked_text_files() {
        tracing::warn!(
            component = "search",
            operation = "search.populate",
            path = %path.as_str(),
            chunks,
            "a [search.text] file exceeds max_chunk and was indexed in chunks; exclude it in \
             [source], or drop its extension from [search.text].extensions, to avoid this"
        );
    }
    let units = published.reads.lexical_units();
    let described = published.reads.described_units(&units);
    let tree_revision = published.reads.tree_revision();
    if let Err(error) = index
        .embed_described(&described, embedding, tree_revision)
        .await
    {
        tracing::warn!(
            component = "search",
            operation = "search.populate",
            tree_revision = published.reads.tree_revision(),
            error = %error,
            "the semantic tier could not embed this publication; the full-text tier keeps \
             answering until a later pass lands"
        );
    }
}

/// Deadline for one rebuild to reach the lexical owner and hear its transaction end.
///
/// Fixed, not `[server] readiness_timeout`: a stuck lexical owner is an internal write-side
/// condition independent of how long an operator lets a read wait for the workspace to
/// settle, and coupling the two would let a short readiness budget starve every rebuild's
/// own commit before it has a chance to finish.
pub(crate) const LEXICAL_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Commits the lexical owner accepts before a rebuild waits for capacity.
pub(crate) const LEXICAL_COMMITS_MAX: usize = 4;

/// What one lexical commit writes.
#[derive(Debug)]
pub(crate) enum LexicalWrite {
    /// The whole unit set, replacing whatever is stored.
    Whole(Vec<LexicalUnit>),
    /// Only the units one change set names.
    Change(LexicalChange),
}

impl LexicalWrite {
    /// Whether this write would leave the stored set exactly as it is, so the rebuild owes
    /// the store nothing and publishes without opening a transaction.
    ///
    /// Only an empty change set reaches this: it shares its predecessor's snapshot, so the
    /// stamp already names the tree revision the candidate answers under.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Whole(_) => false,
            Self::Change(change) => change.is_empty(),
        }
    }
}

/// One commit request: what to write, the tree revision to stamp, and where the reply goes.
struct LexicalCommit {
    write: LexicalWrite,
    tree_revision: String,
    reply: tokio::sync::oneshot::Sender<Result<(), SearchError>>,
}

/// The lexical lane: one long-lived task owning every write to the lexical index, and the
/// handle a rebuild commits through.
///
/// The lexical transaction is part of publication, and vector embedding is not: a lexical
/// row costs one delete and one insert batch, while embedding one declaration can run for
/// longer than the freshness wait a request is bounded by. Splitting the two at the commit
/// is what lets a current-tree request wait for the store to hold its tree without also
/// waiting for model inference.
///
/// One owner serializes the writes. The change lane is released before a rebuild awaits its
/// commit, so two rebuilds can reach the store at once, and `SQLite` would otherwise meet
/// them as write contention rather than as an order.
#[derive(Clone, Debug)]
pub(crate) struct LexicalLane {
    commits: mpsc::Sender<LexicalCommit>,
}

impl LexicalLane {
    /// Spawns the lane's task over `index` and returns the handle a rebuild commits on.
    ///
    /// The task ends when the server does, racing the same cancellation token the index
    /// supervisor runs under. A commit in flight when that happens is answered with the
    /// closed-queue failure, so no rebuild waits on an owner that has gone.
    pub(crate) fn spawn(index: Arc<SearchIndex>, cancellation: CancellationToken) -> Self {
        let (commits, mut requests) = mpsc::channel::<LexicalCommit>(LEXICAL_COMMITS_MAX);
        tokio::spawn(async move {
            loop {
                let received = tokio::select! {
                    () = cancellation.cancelled() => return,
                    received = requests.recv() => received,
                };
                let Some(commit) = received else {
                    return;
                };
                let written = match &commit.write {
                    LexicalWrite::Whole(units) => {
                        index.replace_lexical(units, &commit.tree_revision).await
                    }
                    LexicalWrite::Change(change) => {
                        index.apply_lexical(change, &commit.tree_revision).await
                    }
                };
                let _ = commit.reply.send(written);
            }
        });
        Self { commits }
    }

    /// Commits `write`, stamps `tree_revision`, and returns once that transaction has
    /// ended.
    ///
    /// A write that changes nothing returns without a transaction: the stored stamp already
    /// names the tree revision the candidate answers under.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the owner has ended, when the transaction failed, or when
    /// the commit did not reach the store within [`LEXICAL_COMMIT_TIMEOUT`]. Every one of
    /// them leaves the previously indexed units and stamp intact, and the rebuild that
    /// asked does not publish.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future before the reply arrives leaves the owner's transaction to
    /// finish on its own; that transaction either commits whole or rolls back whole.
    pub(crate) async fn commit(
        &self,
        write: LexicalWrite,
        tree_revision: &str,
    ) -> Result<(), ReadError> {
        if write.is_empty() {
            return Ok(());
        }
        let (reply, answered) = tokio::sync::oneshot::channel();
        let commit = LexicalCommit {
            write,
            tree_revision: tree_revision.to_owned(),
            reply,
        };
        let sent = tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT, self.commits.send(commit));
        match sent.await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err(lexical_unavailable("the lexical owner has ended")),
            Err(_) => {
                return Err(lexical_unavailable(
                    "the lexical commit queue stayed full past the freshness deadline",
                ));
            }
        }
        match tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT, answered).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(ReadFault::unavailable(
                "lexical index commit",
                error.to_string(),
            )),
            Ok(Err(_)) => Err(lexical_unavailable("the lexical owner dropped this commit")),
            Err(_) => Err(lexical_unavailable(
                "the lexical transaction did not end within the freshness deadline",
            )),
        }
    }
}

/// One lexical commit that could not reach the store, as the rebuild reports it.
fn lexical_unavailable(detail: &str) -> ReadError {
    ReadFault::unavailable("lexical index commit", detail)
}

/// The population lane: one long-lived task owning every search index population, and the
/// handle a caller hands one publication to.
///
/// Population used to run wherever it was wanted, and the wait was the caller's. Startup
/// awaited its own pass before the server answered anything, which held the first answer
/// for around fifteen seconds on a real workspace, and every change awaited a whole lexical
/// replacement plus the embedding of each new declaration inside the request path. The lane
/// runs one pass per publication on its own task instead, so no request and no startup step
/// awaits a pass.
///
/// Requests coalesce. The channel holds exactly one publication, so a request landing while
/// an earlier one still waits overwrites it, and the lane always runs the newest tree it
/// was handed rather than a backlog of superseded ones.
///
/// An answer computed while a pass is pending needs nothing new: the lexical tier already
/// holds the published tree, and the semantic tier ranks nothing until the pass for that
/// tree publishes its corpus.
#[derive(Clone, Debug)]
pub(crate) struct PopulationLane {
    publications: Arc<watch::Sender<Option<Arc<PublishedWorkspace>>>>,
}

impl PopulationLane {
    /// Spawns the lane's task over `index` and returns the handle a caller requests on.
    ///
    /// The task runs [`Embedding::Every`] first and [`Embedding::Missing`] for every pass
    /// after it: a store this process found on disk was written by an earlier one, possibly
    /// under another model, so the run's first pass establishes the vector set and later
    /// passes trust what is stored.
    ///
    /// The task ends when the server does. It races the same cancellation token the index
    /// supervisor runs under, which the last server clone's drop guard cancels. Cancelling
    /// mid-pass is safe: [`populate_search`] documents what a dropped pass leaves behind.
    pub(crate) fn spawn(index: Arc<SearchIndex>, cancellation: CancellationToken) -> Self {
        let (publications, mut requests) = watch::channel::<Option<Arc<PublishedWorkspace>>>(None);
        tokio::spawn(async move {
            let mut embedding = Embedding::Every;
            loop {
                let received = tokio::select! {
                    () = cancellation.cancelled() => return,
                    received = requests.changed() => received,
                };
                if received.is_err() {
                    return;
                }
                let requested = requests.borrow_and_update().clone();
                let Some(published) = requested else {
                    continue;
                };
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    () = populate_search(&index, &published, embedding) => {}
                }
                embedding = Embedding::Missing;
            }
        });
        Self {
            publications: Arc::new(publications),
        }
    }

    /// Hands `published` to the lane and returns, never awaiting the pass it asks for.
    ///
    /// A closed channel is a server already shutting down: the lane's task ended with the
    /// cancellation token, and no later search will read the store this pass would have
    /// written. That is a debug line rather than a caller's failure, because the work this
    /// publication came from already landed.
    pub(crate) fn request(&self, published: Arc<PublishedWorkspace>) {
        if self.publications.send(Some(published)).is_err() {
            tracing::debug!(
                component = "search",
                operation = "search.populate",
                "the population lane has ended, so this publication is not populated for"
            );
        }
    }

    /// Whether the lane's task has ended, which a cancelled token causes.
    ///
    /// The task holds the channel's only receiver, so releasing it is the one observable
    /// end of the lane. A test that must run without the lane waits on this rather than on
    /// the cancellation it asked for, which the task has not necessarily seen yet.
    #[cfg(test)]
    pub(crate) fn has_ended(&self) -> bool {
        self.publications.receiver_count() == 0
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
///
/// A published rebuild hands its snapshot to the population lane and moves on to the next
/// batch. The supervisor awaiting a pass itself would hold the whole reconciliation loop
/// for as long as that pass ran, and the filesystem does not stop moving meanwhile.
pub(crate) async fn run_index_supervisor(
    _watcher: notify::RecommendedWatcher,
    mut invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
) {
    let validation = Arc::clone(&context.validation);
    let published = Arc::clone(&context.published);
    let blocking = context.blocking.clone();
    let population = context.population.clone();
    // The guard reports the supervisor's end however it comes: a return, a cancellation, or
    // a panic unwinding this task. A reader that meets the readiness deadline needs to know
    // which of those happened, and the process that panicked cannot tell them afterwards.
    let _running = SupervisorRunning::new(Arc::clone(&validation.supervisor_running));
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
        let request = validation.take_pending();
        let epoch = request.epoch;
        tracing::debug!(
            component = "index",
            operation = "watch.batch",
            epoch,
            whole_workspace = request.work.covers_whole_workspace(),
            "filesystem invalidations coalesced"
        );
        let result = rebuild_workspace(&context, request)
            .instrument(tracing::info_span!(
                "index.build",
                component = "index",
                trigger = "filesystem",
                epoch
            ))
            .await;
        match result {
            Ok(RebuildOutcome::Published) => {
                if let Some(lane) = population.as_ref() {
                    let (current, _) = published.read().await.snapshot();
                    lane.request(current);
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

/// Rebuilds, commits the lexical index, and atomically publishes only a still-current
/// candidate.
///
/// The three run as separate operations. The candidate is captured with the change lane
/// held, so no workspace mutation lands inside it. The lane is released before the lexical
/// transaction, because that transaction awaits and no lock guard may cross an await. The
/// final publication check then takes the same linearization point filesystem observation
/// takes, so a candidate superseded while its transaction ran cannot become current.
///
/// A superseded candidate may already have committed its rows. Its workspace never
/// publishes, pending requests keep waiting because the observed epoch still differs from
/// the published one, and the next current change set replaces those rows.
///
/// # Errors
///
/// Returns [`ReadError`] when the capture fails, or when the lexical commit does not reach
/// the store. A failed commit leaves the previous rows and stamp intact and publishes
/// nothing, so a current-tree request meets the recorded rebuild failure rather than an
/// index the store does not hold.
///
/// # Cancel safety
///
/// Dropping this future after the capture starts does not cancel it: the serialized
/// operation finishes before releasing its lane. Dropping it during the commit leaves the
/// owner's transaction to finish, whole or not at all.
pub(crate) async fn rebuild_workspace(
    context: &IndexSupervisorContext,
    request: RebuildRequest,
) -> Result<RebuildOutcome, ReadError> {
    let epoch = request.epoch;
    let root = context.root.clone();
    let limits = context.limits;
    let published = Arc::clone(&context.published);
    let validation = Arc::clone(&context.validation);
    let blocking = context.blocking.clone();
    let captured_state = Arc::clone(&published);
    let captured_validation = Arc::clone(&validation);
    let change_lane = Arc::clone(&context.change_lane);
    let captured = blocking
        .run("filesystem index rebuild", move || {
            capture_rebuild(
                &root,
                limits,
                &captured_state,
                &change_lane,
                &captured_validation,
                request,
            )
        })
        .await?;
    let CapturedRebuild::Candidate {
        published: candidate,
        write,
        work,
    } = captured
    else {
        return Ok(RebuildOutcome::Superseded);
    };
    if let Some(lane) = context.lexical.as_ref()
        && let Err(error) = lane.commit(write, candidate.reads.tree_revision()).await
    {
        validation.restore_pending(work);
        return Err(error);
    }
    blocking
        .run("filesystem index publication", move || {
            Ok(finish_rebuild(
                &published,
                &validation,
                candidate,
                work,
                epoch,
            ))
        })
        .await
}

/// What one capture leaves for the commit and the publication that follow it.
pub(crate) enum CapturedRebuild {
    /// A stable candidate, what it owes the lexical index, and the observation it answers.
    Candidate {
        /// The candidate publication.
        published: Arc<PublishedWorkspace>,
        /// What the lexical index applies before that candidate publishes.
        write: LexicalWrite,
        /// The observation's work, returned to the supervisor when nothing publishes.
        work: PendingWork,
    },
    /// The observation was already superseded, or configuration moved during the capture.
    Superseded,
}

/// Captures one candidate while the mutation lane is held.
pub(crate) fn capture_rebuild(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    change_lane: &ChangeLane,
    validation: &IndexValidation,
    request: RebuildRequest,
) -> Result<CapturedRebuild, ReadError> {
    change_lane.run(|| capture_rebuild_serialized(root, limits, published, validation, request))
}

/// Captures one candidate with the mutation lane already held.
pub(crate) fn capture_rebuild_serialized(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    request: RebuildRequest,
) -> Result<CapturedRebuild, ReadError> {
    capture_rebuild_with(
        root,
        limits,
        published,
        validation,
        request,
        build_workspace_candidate,
    )
}

/// Runs one serialized capture over an injectable candidate builder, so tests can force
/// each superseded arm deterministically instead of racing the scan.
///
/// An attempt that captures nothing returns its observation's work to the supervisor:
/// publication is the acknowledgement that lets those paths be dropped, so a superseded
/// candidate leaves the next rebuild owing exactly what this one owed plus whatever landed
/// while it ran.
pub(crate) fn capture_rebuild_with(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    mut request: RebuildRequest,
    capture: impl FnOnce(
        &Path,
        WorkspaceIndexLimits,
        &RebuildRequest,
    ) -> Result<WorkspaceCandidate, ReadError>,
) -> Result<CapturedRebuild, ReadError> {
    if !accept_rebuild(validation, request.epoch)? {
        validation.restore_pending(request.work);
        return Ok(CapturedRebuild::Superseded);
    }
    request.previous = Some(published.blocking_read().snapshot().0);
    let candidate = match capture(root, limits, &request) {
        Ok(candidate) => candidate,
        Err(error) => {
            validation.restore_pending(request.work);
            return Err(error);
        }
    };
    let WorkspaceCandidate::Stable {
        published: candidate,
        change_set,
    } = candidate
    else {
        let _ = validation.observe_whole_workspace();
        return Ok(CapturedRebuild::Superseded);
    };
    let write = lexical_write(&candidate, &change_set);
    Ok(CapturedRebuild::Candidate {
        published: candidate,
        write,
        work: request.work,
    })
}

/// Publishes one committed candidate, or returns its observation's work when the tree moved
/// underneath it.
pub(crate) fn finish_rebuild(
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: Arc<PublishedWorkspace>,
    work: PendingWork,
    epoch: u64,
) -> RebuildOutcome {
    let outcome = publish_rebuild(published, validation, candidate);
    match outcome {
        RebuildOutcome::Published => {
            trace_publication(epoch);
            validation.changed.notify_waiters();
        }
        RebuildOutcome::Superseded => validation.restore_pending(work),
    }
    outcome
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
    let publication = validation.locked_pending();
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
    let publication = validation.locked_pending();
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
    use rift_index::{LexicalIndexLimits, WorkspaceIndexLimits, WorkspaceSourcePolicy};
    use rift_protocol::configuration::ServerConfiguration;
    use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits, SemanticReadiness};
    use rift_server::ReadFault;
    use tokio::sync::{Barrier as AsyncBarrier, RwLock};
    use tokio_util::sync::CancellationToken;

    use super::{
        BlockingExecutor, ChangeSet, ConfigurationFingerprint, ConfigurationState, IndexState,
        IndexValidation, LexicalLane, PathChanges, PopulationLane, PublishedWorkspace,
        RebuildOutcome, RebuildRequest, WorkspaceCandidate, build_workspace_candidate,
        publish_rebuild, publish_rebuild_after, record_rebuild_failure,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn stable_candidate(root: &std::path::Path, epoch: u64) -> TestResult<Arc<PublishedWorkspace>> {
        match build_workspace_candidate(
            root,
            WorkspaceIndexLimits::default(),
            &RebuildRequest::initial(epoch),
        )? {
            WorkspaceCandidate::Stable { published, .. } => Ok(published),
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
        assert!(
            invalid.engines_configuration().is_empty(),
            "an invalid file serves no engines"
        );

        fs::write(
            directory.path().join("rift.toml"),
            "[engines.ty]\nprogram = \"uvx\"\nlanguages = [\"python\"]\n",
        )?;
        let with_engines = ConfigurationState::accept(directory.path());
        let engines = with_engines.engines_configuration();
        assert_eq!(
            engines.get("ty").map(|engine| engine.program.as_str()),
            Some("uvx"),
            "an accepted [engines] table is served"
        );

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
        let (validation, mut invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let barrier = Arc::new(AsyncBarrier::new(OBSERVATIONS + 1));
        let mut tasks = Vec::with_capacity(OBSERVATIONS);
        for _ in 0..OBSERVATIONS {
            let validation = Arc::clone(&validation);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                validation
                    .observe_whole_workspace()
                    .map_err(|error| error.to_string())
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
        let (closed, receiver) = IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        drop(receiver);
        assert!(closed.observe_whole_workspace().is_err());
        assert!(closed.watch_failed.load(Ordering::Acquire));

        let (exhausted, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        exhausted.observed_epoch.store(u64::MAX, Ordering::Release);
        assert!(exhausted.observe_whole_workspace().is_err());
        assert!(exhausted.watch_failed.load(Ordering::Acquire));

        let (failed, _receiver) = IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_source_policy(Arc::new(policy));
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));

        let source_path = |path: &str| -> TestResult<super::WatchImpact> {
            Ok(super::WatchImpact::Paths(vec![
                rift_core::ProjectPath::new(path)?,
            ]))
        };
        let expectations: Vec<(EventKind, &str, super::WatchImpact, &str)> = vec![
            (
                EventKind::Modify(ModifyKind::Any),
                "src/lib.rs",
                source_path("src/lib.rs")?,
                "a visible source file names itself",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "src/guide.txt",
                source_path("src/guide.txt")?,
                "an included [search.text] extension is read again like a source file",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "src/generated/code.rs",
                super::WatchImpact::None,
                "an excluded path changes nothing the index holds",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "src/ignored.rs",
                super::WatchImpact::None,
                "a path the workspace's .gitignore excludes changes nothing",
            ),
            (
                EventKind::Remove(RemoveKind::Folder),
                "src",
                super::WatchImpact::WholeWorkspace,
                "a visible directory that disappears takes an unknown set of files with it",
            ),
            (
                EventKind::Create(CreateKind::Folder),
                "examples",
                super::WatchImpact::None,
                "a directory outside the visible globs holds nothing to read",
            ),
            (
                EventKind::Remove(RemoveKind::Folder),
                "src/generated",
                super::WatchImpact::None,
                "an excluded directory holds nothing to read",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                ".gitignore",
                super::WatchImpact::WholeWorkspace,
                "the workspace's ignore file decides what is included",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "examples/.gitignore",
                super::WatchImpact::None,
                "an ignore file under an invisible directory decides nothing",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "src/rift.toml",
                source_path("src/rift.toml")?,
                "a nested rift.toml is an ordinary visible source file; only the root \
                 configuration file drives a whole-workspace rebuild",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "target/.gitignore",
                super::WatchImpact::None,
                "the hard floor refuses target/ before any policy is consulted",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "rift.toml",
                super::WatchImpact::WholeWorkspace,
                "the configuration file decides what is included",
            ),
        ];
        for (kind, path, expected, reason) in expectations {
            assert_eq!(
                super::watch_event_impact(&watched_root, &validation, &event(kind, path)),
                expected,
                "{path}: {reason}"
            );
        }
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
        let (validation, invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_source_policy(Arc::clone(&before.source_policy));
        fs::write(directory.path().join("lib.rs"), "pub fn after() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = false\n",
        )?;
        let epoch = validation.observe_whole_workspace()?;
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
            observer_validation.observe_whole_workspace()
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
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_source_policy(Arc::clone(&before.source_policy));
        assert_eq!(validation.observe_whole_workspace()?, 1);
        assert_eq!(validation.observe_whole_workspace()?, 2);
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
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
        let (validation, receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        drop(receiver);
        let root = std::path::Path::new("/rift-workspace");
        let event = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(root.join("rift.toml"));
        super::report_watch_outcome(root, &validation, Ok(event));
        assert!(validation.watch_failed.load(Ordering::Acquire));
    }

    #[test]
    fn a_source_event_names_its_own_path_and_a_policy_event_names_the_workspace() -> TestResult {
        let directory = tempfile::tempdir()?;
        let watched_root = directory.path().join(".");
        let event_root = directory.path().canonicalize()?;
        fs::create_dir_all(directory.path().join("src"))?;
        let policy = WorkspaceSourcePolicy::build(
            &watched_root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_source_policy(Arc::new(policy));
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));

        assert_eq!(
            super::watch_event_impact(
                &watched_root,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), "src/lib.rs")
            ),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new("src/lib.rs")?]),
            "an edited source file names itself, so only it is read again"
        );
        assert_eq!(
            super::watch_event_impact(
                &watched_root,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), ".gitignore")
            ),
            super::WatchImpact::WholeWorkspace,
            "a written ignore file decides what the workspace includes"
        );
        assert_eq!(
            super::watch_event_impact(
                &watched_root,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), "rift.toml")
            ),
            super::WatchImpact::WholeWorkspace,
            "a written configuration file decides what the workspace includes"
        );
        assert_eq!(
            super::watch_event_impact(
                &watched_root,
                &validation,
                &event(EventKind::Remove(RemoveKind::Folder), "src")
            ),
            super::WatchImpact::WholeWorkspace,
            "a directory that disappears takes an unknown set of files with it"
        );
        Ok(())
    }

    #[test]
    fn one_event_carrying_several_paths_names_them_all() -> TestResult {
        let directory = tempfile::tempdir()?;
        let watched_root = directory.path().join(".");
        let event_root = directory.path().canonicalize()?;
        fs::create_dir_all(directory.path().join("src"))?;
        let policy = WorkspaceSourcePolicy::build(
            &watched_root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
        )?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_source_policy(Arc::new(policy));
        let renamed = Event::new(EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Both,
        )))
        .add_path(event_root.join("src/before.rs"))
        .add_path(event_root.join("src/after.rs"));

        assert_eq!(
            super::watch_event_impact(&watched_root, &validation, &renamed),
            super::WatchImpact::Paths(vec![
                rift_core::ProjectPath::new("src/before.rs")?,
                rift_core::ProjectPath::new("src/after.rs")?,
            ]),
            "a rename reports both spellings, and both are read again"
        );
        Ok(())
    }

    #[test]
    fn retained_paths_past_the_workspace_file_bound_become_a_whole_rebuild() -> TestResult {
        const PATHS_MAX: usize = 2;
        let (validation, mut invalidations) = IndexValidation::new(PATHS_MAX);
        validation.observe_paths([
            rift_core::ProjectPath::new("a.rs")?,
            rift_core::ProjectPath::new("b.rs")?,
        ])?;
        let held = validation.take_pending();
        assert!(
            !held.work.covers_whole_workspace(),
            "paths within the bound are retained as themselves"
        );
        validation.restore_pending(held.work);
        validation.observe_paths([rift_core::ProjectPath::new("c.rs")?])?;

        let escalated = validation.take_pending();
        assert!(
            escalated.work.covers_whole_workspace(),
            "retaining more paths than the workspace may hold files reads everything instead"
        );
        assert_eq!(invalidations.try_recv(), Ok(()));
        Ok(())
    }

    #[test]
    fn a_superseded_attempt_returns_its_paths_to_the_next_rebuild() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let candidate = stable_candidate(directory.path(), 0)?;
        let state = RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let request = validation.take_pending();

        // The observation this attempt answers for is already superseded, so it publishes
        // nothing and owes its paths back.
        validation.observe_paths([rift_core::ProjectPath::new("other.rs")?])?;
        let outcome = super::capture_rebuild_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            request,
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Superseded));

        let next = validation.take_pending();
        let paths: Vec<&str> = next
            .work
            .paths()
            .map(rift_core::ProjectPath::as_str)
            .collect();
        assert_eq!(
            paths,
            vec!["lib.rs", "other.rs"],
            "the superseded attempt's paths return beside what landed while it ran"
        );
        Ok(())
    }

    #[test]
    fn a_change_set_naming_unchanged_bytes_shares_the_previous_read_service() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let previous = stable_candidate(directory.path(), 0)?;
        let request = super::RebuildRequest {
            epoch: 1,
            work: super::PendingWork::naming([rift_core::ProjectPath::new("lib.rs")?]),
            previous: Some(Arc::clone(&previous)),
        };

        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), WorkspaceIndexLimits::default(), &request)?
        else {
            return Err("a stable fixture must build a stable candidate".into());
        };
        assert!(
            Arc::ptr_eq(&previous.reads, &candidate.reads),
            "a path whose bytes did not change leaves the snapshot untouched"
        );
        assert_eq!(
            candidate.epoch, 1,
            "the candidate still answers the observation"
        );
        Ok(())
    }

    #[test]
    fn a_change_set_naming_edited_bytes_replaces_only_that_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("other.rs"), "pub fn other() {}\n")?;
        let previous = stable_candidate(directory.path(), 0)?;
        fs::write(directory.path().join("lib.rs"), "pub fn replaced() {}\n")?;
        let request = super::RebuildRequest {
            epoch: 1,
            work: super::PendingWork::naming([rift_core::ProjectPath::new("lib.rs")?]),
            previous: Some(Arc::clone(&previous)),
        };

        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), WorkspaceIndexLimits::default(), &request)?
        else {
            return Err("a stable fixture must build a stable candidate".into());
        };
        assert!(
            !Arc::ptr_eq(&previous.reads, &candidate.reads),
            "an edited file produces a new snapshot"
        );
        assert_ne!(
            previous.reads.tree_revision(),
            candidate.reads.tree_revision(),
            "the replaced file changes the tree revision the answer carries"
        );
        Ok(())
    }

    #[test]
    fn access_events_never_reach_inclusion() {
        use notify::event::AccessKind;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let root = std::path::Path::new("/rift-workspace");
        let path = root.join("lib.rs");
        let event = Event::new(EventKind::Access(AccessKind::Any)).add_path(path.clone());
        assert_eq!(
            super::watch_event_impact(root, &validation, &event),
            super::WatchImpact::None,
            "an access event never reaches the inclusion predicate"
        );
        assert_eq!(
            super::watch_path_impact(root, &validation, EventKind::Access(AccessKind::Any), &path),
            super::WatchImpact::None
        );
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
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let candidate = stable_candidate(directory.path(), 0)?;
        let state = RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let outcome = super::capture_rebuild_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            RebuildRequest::initial(7),
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Superseded));
        Ok(())
    }

    #[test]
    fn rebuild_is_superseded_when_configuration_moves_during_capture() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn before() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let candidate = stable_candidate(directory.path(), 0)?;
        let state = RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let outcome = super::capture_rebuild_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            RebuildRequest::initial(0),
            |_, _, _| Ok(WorkspaceCandidate::ConfigurationChanged),
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Superseded));
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
        let (validation, invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
                population: None,
                lexical: None,
            },
        ));
        let notified = validation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        validation
            .observe_whole_workspace()
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
        let (validation, receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
        let (validation, invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
                population: None,
                lexical: None,
            },
        ));
        let notified = validation.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let epoch = validation
            .observe_whole_workspace()
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
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
                moving.observe_whole_workspace()?;
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
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
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
    async fn a_commit_persists_every_chunk_of_an_oversized_text_file() -> TestResult {
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
        fs::write(directory.path().join("guide.txt"), "word ".repeat(1000))?;
        let published = stable_candidate(directory.path(), 0)?;

        let chunked = published.reads.chunked_text_files();
        let chunk_count = chunked
            .iter()
            .find(|(path, _)| path.as_str() == "guide.txt")
            .map(|(_, count)| *count)
            .ok_or("guide.txt must be reported as chunked before the commit runs")?;
        assert!(
            chunk_count > 1,
            "the oversized guide must split into more than one chunk: {chunk_count}"
        );

        let units = published.reads.lexical_units();
        let guide_units = units
            .iter()
            .filter(|unit| unit.path().as_str() == "guide.txt")
            .count();
        assert!(
            guide_units > 1,
            "the oversized file must contribute more than one lexical unit: {guide_units}"
        );

        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());
        lane.commit(
            super::lexical_write(&published, &ChangeSet::Full),
            published.reads.tree_revision(),
        )
        .await?;

        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(published.reads.tree_revision()),
            "the commit must succeed and stamp the published tree revision, not merely warn"
        );
        let ranked = ranked_at(&index, published.reads.tree_revision(), "word", 64).await?;
        for unit in units
            .iter()
            .filter(|unit| unit.path().as_str() == "guide.txt")
        {
            assert!(
                ranked.iter().any(|one| one.identity() == unit.identity()),
                "every chunk unit must have been persisted: identity={} ranked={ranked:#?}",
                unit.identity()
            );
        }
        cancellation.cancel();
        Ok(())
    }

    /// One rebuild driven the way the supervisor drives it, over its own blocking executor.
    async fn rebuilt_through(
        root: &std::path::Path,
        state: &Arc<RwLock<IndexState>>,
        validation: &Arc<IndexValidation>,
        lexical: Option<LexicalLane>,
    ) -> Result<RebuildOutcome, rift_server::ReadError> {
        let request = validation.take_pending();
        let context = super::IndexSupervisorContext {
            root: root.to_path_buf(),
            limits: WorkspaceIndexLimits::default(),
            published: Arc::clone(state),
            change_lane: Arc::new(crate::server::ChangeLane::default()),
            validation: Arc::clone(validation),
            blocking: BlockingExecutor::for_configuration(&ServerConfiguration::default()),
            population: None,
            lexical,
        };
        super::rebuild_workspace(&context, request).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_published_rebuild_has_already_committed_its_rows() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn firstbeta() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let first = stable_candidate(directory.path(), 0)?;
        let state = Arc::new(RwLock::new(IndexState {
            current: Arc::clone(&first),
            failure: None,
        }));
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());
        lane.commit(
            super::lexical_write(&first, &ChangeSet::Full),
            first.reads.tree_revision(),
        )
        .await?;

        fs::write(directory.path().join("lib.rs"), "pub fn secondgamma() {}\n")?;
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let outcome =
            rebuilt_through(directory.path(), &state, &validation, Some(lane.clone())).await?;

        assert_eq!(outcome, RebuildOutcome::Published);
        let current = Arc::clone(&state.read().await.current);
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(current.reads.tree_revision()),
            "the store holds the published tree the moment that tree becomes current"
        );
        assert!(
            !ranked_at(&index, current.reads.tree_revision(), "secondgamma", 8)
                .await?
                .is_empty(),
            "the published tree's units are searchable as soon as it publishes"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_failed_lexical_commit_leaves_the_previous_publication_current() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn firstbeta() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let first = stable_candidate(directory.path(), 0)?;
        let state = Arc::new(RwLock::new(IndexState {
            current: Arc::clone(&first),
            failure: None,
        }));
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());
        lane.commit(
            super::lexical_write(&first, &ChangeSet::Full),
            first.reads.tree_revision(),
        )
        .await?;

        // The owner ends, so the next commit cannot reach the store at all.
        cancellation.cancel();
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.commits.is_closed() {
                break;
            }
            tokio::time::sleep(LANE_POLL).await;
        }

        fs::write(directory.path().join("lib.rs"), "pub fn secondgamma() {}\n")?;
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let error = rebuilt_through(directory.path(), &state, &validation, Some(lane))
            .await
            .expect_err("a rebuild whose commit cannot land must not publish");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");

        assert!(
            Arc::ptr_eq(&state.read().await.current, &first),
            "the previous publication stays current when the commit failed"
        );
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(first.reads.tree_revision()),
            "the previously stamped revision stays intact"
        );
        let next = validation.take_pending();
        assert_eq!(
            next.work
                .paths()
                .map(rift_core::ProjectPath::as_str)
                .collect::<Vec<_>>(),
            vec!["lib.rs"],
            "the failed attempt returns its paths to the next rebuild"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_whole_commit_stamps_the_published_revision_and_leaves_units_searchable() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());

        let write = super::lexical_write(&published, &ChangeSet::Full);
        lane.commit(write, published.reads.tree_revision()).await?;

        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(published.reads.tree_revision()),
            "the commit stamps the tree revision the candidate answers under"
        );
        assert!(
            !ranked_at(&index, published.reads.tree_revision(), "beacon", 8)
                .await?
                .is_empty(),
            "the commit leaves the published unit set searchable"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_change_commit_replaces_one_path_and_keeps_every_other_unit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("kept.rs"), "pub fn keptalpha() {}\n")?;
        fs::write(directory.path().join("moved.rs"), "pub fn firstbeta() {}\n")?;
        let first = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());
        lane.commit(
            super::lexical_write(&first, &ChangeSet::Full),
            first.reads.tree_revision(),
        )
        .await?;

        fs::write(
            directory.path().join("moved.rs"),
            "pub fn secondgamma() {}\n",
        )?;
        let request = super::RebuildRequest {
            epoch: 1,
            work: super::PendingWork::naming([rift_core::ProjectPath::new("moved.rs")?]),
            previous: Some(Arc::clone(&first)),
        };
        let WorkspaceCandidate::Stable {
            published: second,
            change_set,
        } = build_workspace_candidate(directory.path(), WorkspaceIndexLimits::default(), &request)?
        else {
            return Err("a stable fixture must build a stable candidate".into());
        };
        lane.commit(
            super::lexical_write(&second, &change_set),
            second.reads.tree_revision(),
        )
        .await?;

        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(second.reads.tree_revision()),
            "the change commit stamps the revision its candidate answers under"
        );
        assert!(
            !ranked_at(&index, second.reads.tree_revision(), "secondgamma", 8)
                .await?
                .is_empty(),
            "the changed path's new units are searchable"
        );
        assert!(
            ranked_at(&index, second.reads.tree_revision(), "firstbeta", 8)
                .await?
                .is_empty(),
            "the changed path's previous units are gone"
        );
        assert!(
            !ranked_at(&index, second.reads.tree_revision(), "keptalpha", 8)
                .await?
                .is_empty(),
            "a path the change set never named keeps its units"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_change_commit_leaves_the_stored_revision_as_it_was() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());

        let empty =
            super::lexical_write(&published, &ChangeSet::Incremental(PathChanges::default()));
        assert!(
            empty.is_empty(),
            "a change set naming nothing writes nothing"
        );
        lane.commit(empty, published.reads.tree_revision()).await?;
        assert_eq!(
            index.tree_revision().await?,
            None,
            "a write that changes nothing opens no transaction"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_commit_after_the_lexical_owner_ended_refuses_rather_than_waiting() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(Arc::clone(&index), cancellation.clone());
        cancellation.cancel();
        // The owner holds the channel's only receiver, so a closed channel is the one
        // observable end of the lane.
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.commits.is_closed() {
                break;
            }
            tokio::time::sleep(LANE_POLL).await;
        }

        let error = lane
            .commit(
                super::lexical_write(&published, &ChangeSet::Full),
                published.reads.tree_revision(),
            )
            .await
            .expect_err("a commit to an ended owner must refuse");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        assert_eq!(
            index.tree_revision().await?,
            None,
            "a refused commit leaves the store as it was"
        );
        Ok(())
    }

    /// Polls one lane pass a test waits on before it gives up: three seconds, at
    /// [`LANE_POLL`] each.
    const LANE_ATTEMPTS_MAX: usize = 60;
    /// Wait between two reads of a store the lane has not stamped yet.
    const LANE_POLL: Duration = Duration::from_millis(50);

    /// Polls `index` until it carries `revision`, which the lane's pass stamps.
    ///
    /// # Errors
    ///
    /// Returns the stamp the store still carried once the bound runs out.
    /// One index whose semantic tier is enabled but holds no model, so a pass records the
    /// declaration count it was handed as its readiness rather than embedding anything.
    ///
    /// That count is the population lane's observable: with the tier disabled a pass leaves
    /// no trace at all, and the lexical stamp now belongs to the lexical lane.
    async fn counting_index(database: &std::path::Path) -> TestResult<SearchIndex> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default()).build();
        let index = SearchIndex::open(database, limits).await?;
        assert_eq!(
            index.readiness(),
            SemanticReadiness::Preparing {
                prepared: 0,
                total: 0
            }
        );
        Ok(index)
    }

    /// The units one revision-qualified search ranked, refusing an answer the store could
    /// not place under `tree_revision`.
    async fn ranked_at(
        index: &SearchIndex,
        tree_revision: &str,
        query: &str,
        limit: u32,
    ) -> TestResult<Vec<rift_search::RankedUnit>> {
        match index.search(tree_revision, query, limit).await? {
            RevisionScoped::Matched(ranked) => Ok(ranked),
            other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
        }
    }

    /// Waits until the lane's readiness names `total` declarations.
    async fn described_within_bound(index: &SearchIndex, total: u64) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if index.readiness() == (SemanticReadiness::Preparing { prepared: 0, total }) {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err(format!(
            "the lane never recorded {total} declarations; readiness is {:?}",
            index.readiness()
        )
        .into())
    }

    #[tokio::test]
    async fn the_lane_runs_the_pass_one_request_asks_for() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(counting_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = PopulationLane::spawn(Arc::clone(&index), cancellation.clone());

        let units = published.reads.lexical_units();
        let described = published.reads.described_units(&units).len() as u64;
        lane.request(Arc::clone(&published));
        described_within_bound(&index, described).await?;

        cancellation.cancel();
        Ok(())
    }

    /// Two requests with no await between them, so the lane's task cannot have run for the
    /// first one: the channel holds one publication, and the second overwrites it.
    ///
    /// The lane runs the newest tree it was handed. This is what keeps a run of changes
    /// from queueing one whole pass each.
    #[tokio::test(flavor = "current_thread")]
    async fn the_lane_runs_the_newest_publication_when_two_requests_coalesce() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let earlier = stable_candidate(directory.path(), 0)?;
        fs::write(directory.path().join("lib.rs"), "pub fn lantern() {}\n")?;
        let newest = stable_candidate(directory.path(), 1)?;
        assert_ne!(
            earlier.reads.tree_revision(),
            newest.reads.tree_revision(),
            "the fixture must actually move the tree revision"
        );
        let index = Arc::new(counting_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = PopulationLane::spawn(Arc::clone(&index), cancellation.clone());

        let units = newest.reads.lexical_units();
        let described = newest.reads.described_units(&units).len() as u64;
        lane.request(Arc::clone(&earlier));
        lane.request(Arc::clone(&newest));
        described_within_bound(&index, described).await?;

        cancellation.cancel();
        Ok(())
    }

    /// A request after the lane's task ended is a shutting-down server, which is a debug
    /// line rather than a caller's failure: the request path must not learn that the lane
    /// is gone.
    #[tokio::test]
    async fn a_request_after_the_lane_ended_leaves_the_store_as_it_was() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let earlier = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(counting_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = PopulationLane::spawn(Arc::clone(&index), cancellation.clone());
        let units = earlier.reads.lexical_units();
        let described = earlier.reads.described_units(&units).len() as u64;
        lane.request(Arc::clone(&earlier));
        described_within_bound(&index, described).await?;

        cancellation.cancel();
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.has_ended() {
                break;
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        assert!(
            lane.has_ended(),
            "the lane's task must end with the cancellation it races"
        );

        fs::write(
            directory.path().join("lib.rs"),
            "pub fn lantern() {}\npub fn beacon() {}\n",
        )?;
        let newest = stable_candidate(directory.path(), 1)?;
        lane.request(Arc::clone(&newest));
        assert_eq!(
            index.readiness(),
            SemanticReadiness::Preparing {
                prepared: 0,
                total: described
            },
            "a request the ended lane refused must leave the previous pass's readiness alone"
        );
        Ok(())
    }

    /// One search index over `database` with the semantic tier off, so a test drives the
    /// full-text half without acquiring model weights.
    async fn search_index(database: &std::path::Path) -> TestResult<SearchIndex> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_semantic()
            .build();
        let index = SearchIndex::open(database, limits).await?;
        assert_eq!(index.readiness(), SemanticReadiness::Disabled);
        Ok(index)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn supervisor_dispatches_superseded_when_epoch_moves_before_acceptance() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let current = stable_candidate(directory.path(), 0)?;
        let published = Arc::new(RwLock::new(IndexState {
            current,
            failure: None,
        }));
        let watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;

        // One blocking slot, held by a placeholder so the supervisor's own rebuild for
        // epoch 1 is forced to queue behind it - a deterministic gate between the
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
                population: None,
                lexical: None,
            },
        ));

        let first_epoch = validation
            .observe_whole_workspace()
            .map_err(|error| format!("first observation must land: {error:?}"))?;
        assert_eq!(first_epoch, 1);
        // Gives the supervisor's debounce (50ms) real wall-clock time to elapse and its
        // rebuild to reach and queue behind the held placeholder. The held slot - not this
        // wait - is what guarantees the rebuild cannot be accepted until released; a
        // slower scheduler just makes this wait less generous, never wrong.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Moves the epoch again with no invalidation signal, so no second rebuild cycle is
        // ever triggered - only the already-queued rebuild for epoch 1 observes this move.
        let moved = validation.observed_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(moved, 2);

        release_sender
            .send(())
            .expect("held placeholder must accept release");
        held.await??;

        // A sentinel operation through the same one-slot executor only completes once the
        // supervisor's own queued rebuild has acquired, run, and released that slot -
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
