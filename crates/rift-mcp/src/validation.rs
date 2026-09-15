//! Current-index validation: filesystem observation, serialized rebuilds,
//! and atomic publication of the workspace snapshot.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
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
use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_index::{
    BindingPolicy, ChangeSet, FileDigest, LexicalChange, LexicalUnit, PathChanges,
    WorkspaceFingerprint, WorkspaceIndexLimits, WorkspaceSourcePolicy,
};
use rift_protocol::configuration::{
    BindingConfiguration, HistoryConfiguration, LanguageLspConfiguration, LogsConfiguration,
    LspConfiguration, SearchConfiguration, ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::dependencies::DependenciesConfiguration;
use rift_protocol::error as wire;
use rift_protocol::map::WorkspaceMap;
use rift_protocol::source::SourceConfiguration;
use rift_search::{Embedding, SearchError, SearchIndex};
use rift_server::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, DependencyStore, LspProcessKey, ReadError,
    ReadFault, ReadService, load_configuration,
};
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};
use tokio::sync::futures::Notified;
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::dependency::{DependencyLane, DependencyPlan};
use crate::failure::WireFailure;
use crate::server::{BlockingExecutor, ChangeLane};

/// Filesystem events coalesced while one rebuild is pending.
pub(crate) const INDEX_INVALIDATIONS_MAX: usize = 1;
/// Delay collecting one bounded filesystem-event batch.
pub(crate) const INDEX_DEBOUNCE: Duration = Duration::from_millis(50);
/// Complete capture retries while the tree keeps moving.
pub(crate) const INDEX_CAPTURE_ATTEMPTS_MAX: usize = 3;

/// What the next rebuild must cover, accumulated between publications.
///
/// A watcher event and a change tool both name paths, so an ordinary rebuild reads only
/// what moved. An observation that names no trustworthy path set - a watch failure, a
/// `.gitignore` write, a directory appearing or disappearing, or more retained paths than
/// the workspace's own file bound allows - asks for the whole workspace instead, and no
/// later path narrows that back down. A `rift.toml` write keeps its exact path so acceptance
/// can decide whether index-owned configuration changed.
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
    /// Changed source, language, or text-search configuration reads the whole workspace.
    /// Other accepted configuration carries forward the same source files under an empty
    /// incremental change. A path whose bytes cannot be read for any reason other than its
    /// absence asks for a whole scan, because that scan decides whether the path is a
    /// refusal or a removal.
    fn change_set(&self, root: &Path, configuration: &ConfigurationState) -> ChangeSet {
        let Some(previous) = self.previous.as_ref() else {
            return ChangeSet::Full;
        };
        if self.work.whole_workspace {
            return ChangeSet::Full;
        }
        if previous.configuration.fingerprint != configuration.fingerprint {
            if previous
                .configuration
                .index_configuration_differs(configuration)
            {
                return ChangeSet::Full;
            }
            return ChangeSet::Incremental(PathChanges::default());
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
        match policy.visible_digest(&absolute) {
            Ok(digest) => observed.push((path.clone(), digest)),
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
    /// Workspace orientation snapshot served by `rift://map`, computed once for this
    /// publication and reused until the next one - a read costs a lookup, never a rebuild.
    pub(crate) map: Arc<WorkspaceMap>,
    /// What the accepted `[dependencies]` table asks of the dependency index, compiled
    /// once beside `reads` and handed to the dependency lane with the catalog.
    pub(crate) dependency_plan: DependencyPlan,
    pub(crate) epoch: u64,
}

impl PublishedWorkspace {
    /// This publication's twin under `configuration` and `epoch`.
    ///
    /// Every part is shared, so the twin costs the clones of its handles and reads no
    /// file again. It is what a capture that read nothing new publishes: the same tree,
    /// answering a later observation.
    fn under(&self, configuration: ConfigurationState, epoch: u64) -> Self {
        Self {
            reads: Arc::clone(&self.reads),
            configuration,
            fingerprint: self.fingerprint.clone(),
            source_policy: Arc::clone(&self.source_policy),
            map: Arc::clone(&self.map),
            dependency_plan: self.dependency_plan.clone(),
            epoch,
        }
    }
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
    /// The current publication as event classification sees it: the inclusion policy
    /// the index was built under, and the index itself for what it holds. Absent before
    /// the first publication, when every event asks for the whole workspace anyway.
    pub(crate) published: SyncRwLock<Option<Arc<PublishedWorkspace>>>,
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
    /// The workspace's lexical lane, handed each published candidate's write, absent
    /// exactly when the population lane is: with no index open there is no store to
    /// commit to, and `search` reports the tier unavailable for the life of this server.
    pub(crate) lexical: Option<LexicalLane>,
    /// The dependency index every candidate's read service answers from.
    pub(crate) dependencies: Arc<DependencyStore>,
    /// The dependency lane, handed each published catalog.
    pub(crate) dependency_lane: DependencyLane,
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

    /// The `[source]` table from the last acceptance, or the default table while
    /// `rift.toml` is invalid.
    pub(crate) fn source_configuration(&self) -> SourceConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.source.clone())
            .unwrap_or_default()
    }

    /// The bounds the index builds under: `base` with its file count and aggregate byte
    /// bounds replaced by the `[source]` table's `files` and `workspace_size`. The
    /// per-file, depth, and result bounds stay as `base` carries them.
    pub(crate) fn index_limits(
        &self,
        base: WorkspaceIndexLimits,
    ) -> Result<WorkspaceIndexLimits, ReadError> {
        let source = self.source_configuration();
        let files_max = usize::try_from(source.files).unwrap_or(usize::MAX);
        let workspace_bytes_max =
            usize::try_from(source.workspace_size.bytes()).unwrap_or(usize::MAX);
        base.with_workspace_bounds(files_max, workspace_bytes_max)
            .map_err(|error| ReadError::from(ReadFault::Index(error)))
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

    /// Effective language file entries from the last acceptance.
    pub(crate) fn language_file_selections(&self) -> LanguageFileSelections {
        self.accepted.as_ref().map_or_else(
            |_| LanguageFileSelections::default(),
            LanguageFileSelections::from,
        )
    }

    /// Whether index-owned configuration differs from another acceptance.
    ///
    /// The `[providers.binding]` table counts as index-owned: its switch and bounds
    /// shape the publication set the index bakes at build time. So does the
    /// `[dependencies]` table: the read service resolves its catalog under the table's
    /// resolution policy and gates dependency-scoped lookups on its switch.
    fn index_configuration_differs(&self, other: &Self) -> bool {
        self.source_configuration() != other.source_configuration()
            || self.text_inclusion() != other.text_inclusion()
            || self.language_file_selections() != other.language_file_selections()
            || self.binding_configuration() != other.binding_configuration()
            || self.dependencies_configuration() != other.dependencies_configuration()
    }

    /// The `[providers.history]` table from the last acceptance, or the
    /// default table while `rift.toml` is invalid.
    pub(crate) fn history_configuration(&self) -> HistoryConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.providers.history.clone())
            .unwrap_or_default()
    }

    /// The `[providers.binding]` table from the last acceptance, or the
    /// default table while `rift.toml` is invalid.
    pub(crate) fn binding_configuration(&self) -> BindingConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.providers.binding.clone())
            .unwrap_or_default()
    }

    /// The `[dependencies]` table from the last acceptance, or the default table
    /// while `rift.toml` is invalid.
    pub(crate) fn dependencies_configuration(&self) -> DependenciesConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.dependencies.clone())
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

    /// LSP process definitions and exact language bindings from the last acceptance.
    ///
    /// A named `[lsp.<name>]` entry becomes a definition whether or not a language
    /// selects it, so the pool can start it the moment one does. An inline table
    /// belongs to its own language entry alone, so a disabled entry contributes
    /// neither the definition nor the binding.
    pub(crate) fn lsp_runtime_configuration(
        &self,
    ) -> (
        BTreeMap<LspProcessKey, LspConfiguration>,
        BTreeMap<String, LspProcessKey>,
    ) {
        let Ok(configuration) = &self.accepted else {
            return (BTreeMap::new(), BTreeMap::new());
        };
        let mut definitions = configuration
            .lsp
            .iter()
            .map(|(name, lsp)| (LspProcessKey::Named(name.clone()), lsp.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut bindings = BTreeMap::new();
        for (identity, language) in &configuration.languages {
            let Some(lsp) = &language.lsp else {
                continue;
            };
            if !language.enabled {
                continue;
            }
            let key = match lsp {
                LanguageLspConfiguration::Named(name) => LspProcessKey::Named(name.clone()),
                LanguageLspConfiguration::Inline(lsp) => {
                    let key = LspProcessKey::Inline(identity.clone());
                    definitions.insert(key.clone(), lsp.clone());
                    key
                }
            };
            bindings.insert(identity.clone(), key);
        }
        (definitions, bindings)
    }

    /// Whether accepted configuration runs any source-read-only hook.
    pub(crate) fn has_validation_hooks(&self) -> bool {
        self.accepted.as_ref().is_ok_and(|configuration| {
            configuration
                .hooks
                .iter()
                .any(|hook| hook.writes.is_validation())
        })
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

impl ConfigurationFingerprint {
    /// Eight-hex-character resource revision for this file state.
    pub(crate) fn wire_revision(self) -> String {
        let bytes = match self {
            Self::Content(bytes) => bytes,
            Self::MissingOrUnreadable => Sha256::digest(b"missing").into(),
            Self::Oversized(length) => Sha256::digest(length.to_be_bytes()).into(),
        };
        let mut revision = String::with_capacity(8);
        for byte in &bytes[..4] {
            use std::fmt::Write as _;
            let _ = write!(revision, "{byte:02x}");
        }
        revision
    }
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
                published: SyncRwLock::new(None),
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

    /// Installs one publication under publication linearization.
    #[cfg(test)]
    fn install_publication(&self, published: &Arc<PublishedWorkspace>) {
        let publication = self.locked_pending();
        self.replace_publication_locked(published);
        drop(publication);
    }

    /// Replaces the publication event classification answers from, while the caller
    /// owns the publication lane.
    fn replace_publication_locked(&self, published: &Arc<PublishedWorkspace>) {
        let mut current = self
            .published
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Some(Arc::clone(published));
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

    /// The publication event classification answers from, absent before the first one.
    fn current_publication(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, Option<Arc<PublishedWorkspace>>> {
        self.published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The project path one event path names, under the current inclusion policy.
    fn source_project_path(&self, path: &Path) -> Option<ProjectPath> {
        self.current_publication()
            .as_ref()?
            .source_policy
            .project_path(path)
    }

    /// Returns whether current policy includes one source event path.
    fn source_path_is_relevant(&self, path: &Path) -> bool {
        self.current_publication()
            .as_ref()
            .is_none_or(|published| published.source_policy.visible(path))
    }

    /// Returns whether current policy can include source below one directory.
    fn source_directory_is_relevant(&self, path: &Path) -> bool {
        self.current_publication()
            .as_ref()
            .is_none_or(|published| published.source_policy.may_include_descendant(path))
    }

    /// Whether the current publication holds at least one file below one event path.
    ///
    /// A path outside the policy's root, or one observed before the first publication,
    /// holds nothing the index knows about.
    fn published_holds_files_below(&self, path: &Path) -> bool {
        let current = self.current_publication();
        let Some(published) = current.as_ref() else {
            return false;
        };
        published
            .source_policy
            .project_path(path)
            .is_some_and(|directory| published.reads.holds_files_below(&directory))
    }

    /// Whether one watched path is the workspace's own configuration file.
    ///
    /// Before the first publication there is no policy to normalize through, so the
    /// raw spelling decides; afterwards the policy answers, which is what makes the
    /// comparison hold on a platform whose temporary root is a symlink.
    fn is_workspace_configuration(&self, root: &Path, path: &Path) -> bool {
        self.current_publication().as_ref().map_or_else(
            || path == root.join(WORKSPACE_CONFIGURATION_FILE),
            |published| published.source_policy.is_workspace_configuration(path),
        )
    }

    /// Whether writing this path changes what the workspace includes.
    ///
    /// Before the first publication installs a policy there is nothing to ask, so only the
    /// root `rift.toml` and a `.gitignore` are taken as inclusion deciders; a published
    /// policy answers for its own root spellings and excluded directories.
    pub(crate) fn decides_inclusion(&self, root: &Path, path: &Path) -> bool {
        self.current_publication().as_ref().map_or_else(
            || {
                path == root.join(WORKSPACE_CONFIGURATION_FILE)
                    || path.file_name() == Some(std::ffi::OsStr::new(VCS_IGNORE_FILE))
            },
            |published| published.source_policy.decides_inclusion(path),
        )
    }
}

impl Drop for IndexValidation {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl IndexSupervisor {
    /// Cancels the supervisor and joins it, bounded by `deadline`.
    ///
    /// `deadline` is the whole stop's shared deadline, so the join takes only
    /// what earlier stages left of it; a supervisor still running when it
    /// passes is aborted rather than waited on.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the task panics or outlasts `deadline`.
    ///
    /// # Cancel safety
    ///
    /// Cancellation is requested before the join begins. Dropping this future
    /// after it takes task ownership detaches that terminating task.
    pub(crate) async fn shutdown(&self, deadline: Instant) -> Result<(), ReadError> {
        self.validation.cancellation.cancel();
        let Some(mut task) = self.validation.task.lock().await.take() else {
            return Ok(());
        };
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut task).await {
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
    changed: Arc<Notify>,
}

impl SupervisorRunning {
    fn new(running: Arc<AtomicBool>, changed: Arc<Notify>) -> Self {
        running.store(true, Ordering::Release);
        Self { running, changed }
    }
}

impl Drop for SupervisorRunning {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.changed.notify_waiters();
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
///
/// A name event on an extensionless path asks for the whole workspace only when the
/// path names a directory the index can hold files under: one on disk right now, or one
/// the publication holds files below, as a directory renamed away is. The server's own
/// writes stage each file as an extensionless temporary file beside its target and
/// rename it over the target; the publication holds nothing under that staging name, so
/// its name event takes the per-path route a file event takes.
pub(crate) fn watch_path_impact(
    root: &Path,
    validation: &IndexValidation,
    kind: EventKind,
    path: &Path,
) -> WatchImpact {
    if validation.is_workspace_configuration(root, path) {
        return ProjectPath::new(WORKSPACE_CONFIGURATION_FILE.to_owned())
            .map_or(WatchImpact::WholeWorkspace, |path| {
                WatchImpact::Paths(vec![path])
            });
    }
    let policy_file = validation.decides_inclusion(root, path);
    let directory_event = matches!(
        kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder)
    );
    if matches!(kind, EventKind::Access(_)) {
        return WatchImpact::None;
    }
    if policy_file {
        return WatchImpact::WholeWorkspace;
    }
    if directory_event {
        return if validation.source_directory_is_relevant(path) {
            WatchImpact::WholeWorkspace
        } else {
            WatchImpact::None
        };
    }
    let reshapes_tree = match kind {
        EventKind::Modify(ModifyKind::Name(_)) | EventKind::Any | EventKind::Other => {
            names_a_directory(validation, path)
        }
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(_)
        | EventKind::Access(_) => false,
    };
    if reshapes_tree {
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

/// Whether one event path names a directory the index can hold files under: an
/// extensionless path the policy may include descendants of, that is a directory on
/// disk right now or that the current publication holds files below.
///
/// The disk probe is the one filesystem read event classification makes: a renamed
/// directory's new spelling is known to the disk alone, and its old spelling to the
/// publication alone.
fn names_a_directory(validation: &IndexValidation, path: &Path) -> bool {
    let extensionless = path.extension().is_none();
    if !extensionless || !validation.source_directory_is_relevant(path) {
        return false;
    }
    path.is_dir() || validation.published_holds_files_below(path)
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
    dependencies: &Arc<DependencyStore>,
) -> Result<(Arc<PublishedWorkspace>, LexicalWrite), ReadError> {
    let capture = workspace_capture(dependencies);
    initial_workspace_with(root, limits, validation, blocking, capture).await
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

/// The capture production runs: the whole-workspace scan over the dependency index.
pub(crate) fn workspace_capture(
    dependencies: &Arc<DependencyStore>,
) -> impl CaptureWorkspace + Clone + Send + 'static {
    let dependencies = Arc::clone(dependencies);
    move |root: &Path, limits: WorkspaceIndexLimits, request: &RebuildRequest| {
        build_workspace_candidate(root, limits, request, &dependencies)
    }
}

/// Runs the bounded capture loop over an injectable capture, so tests can
/// force each retry arm deterministically instead of racing the filesystem.
///
/// An attempt publishes when what it built answers the observations that landed while it
/// was building. The filesystem epoch counts observations rather than content:
/// [`IndexValidation::observe_locked`] increments it for every classified event, and
/// [`watch_path_impact`] classifies from the path and the event kind without reading a
/// byte, so an epoch that moved says nothing about whether the tree did. Two content
/// comparisons answer that instead, and either one publishes: [`answered_candidate`], the
/// same comparison a rebuild makes, when the observation names paths the candidate
/// already holds; and a rescan that folds to the fingerprint the previous attempt folded
/// to under an unmoved `rift.toml`, which is two scans taken at two epochs agreeing on
/// every visible byte. A watch the backend reported broken publishes under neither,
/// because the server no longer learns what moved.
pub(crate) async fn initial_workspace_with(
    root: &Path,
    limits: WorkspaceIndexLimits,
    validation: &IndexValidation,
    blocking: &BlockingExecutor,
    capture: impl CaptureWorkspace + Clone + Send + 'static,
) -> Result<(Arc<PublishedWorkspace>, LexicalWrite), ReadError> {
    let mut rescanned = None;
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
        let scanned_the_same_tree = rescanned.as_ref() == Some(&built.fingerprint);
        if !validation.watch_failed.load(Ordering::Acquire) {
            let publication = validation.locked_pending();
            let observed_epoch = validation.observed_epoch();
            let answering = if validation.watch_failed.load(Ordering::Acquire) {
                None
            } else if scanned_the_same_tree
                && built.configuration.fingerprint == configuration_fingerprint(root)
            {
                Some(Arc::new(
                    built.under(built.configuration.clone(), observed_epoch),
                ))
            } else {
                answered_candidate(root, &built, &publication, observed_epoch)
            };
            if let Some(answering) = answering {
                validation.replace_publication_locked(&answering);
                drop(publication);
                tracing::info!(
                    component = "index",
                    operation = "index.publish",
                    trigger = "startup",
                    epoch = observed_epoch,
                    "index snapshot published"
                );
                return Ok((answering, write));
            }
            drop(publication);
        }
        rescanned = Some(built.fingerprint.clone());
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
/// underneath it is reported as changed rather than published. Either way the candidate's
/// read service answers dependency-scoped lookups from `dependencies`.
pub(crate) fn build_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    request: &RebuildRequest,
    dependencies: &Arc<DependencyStore>,
) -> Result<WorkspaceCandidate, ReadError> {
    let configuration = ConfigurationState::accept(root);
    let change_set = request.change_set(root, &configuration);
    let candidate = match &change_set {
        ChangeSet::Full => {
            whole_workspace_candidate(root, limits, configuration, request.epoch, dependencies)?
        }
        ChangeSet::Incremental(changes) => {
            let previous = request
                .previous
                .as_ref()
                .unwrap_or_else(|| unreachable!("an incremental change set names a publication"));
            shared_workspace_candidate(previous, changes, configuration, request.epoch)?
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
    dependencies: &Arc<DependencyStore>,
) -> Result<PublishedWorkspace, ReadError> {
    let visibility = configuration.source_visibility();
    let limits = configuration.index_limits(limits)?;
    let text_inclusion = configuration.text_inclusion();
    let languages = configuration.language_file_selections();
    let binding = BindingPolicy::from(&configuration.binding_configuration());
    let dependencies_configuration = configuration.dependencies_configuration();
    let dependency_plan = DependencyPlan::compile(&dependencies_configuration)?;
    let reads = ReadService::build_with_languages(
        root,
        limits,
        &visibility,
        &text_inclusion,
        &languages,
        binding,
        configuration.history_configuration(),
        dependencies_configuration,
    )?
    .with_dependencies(Arc::clone(dependencies));
    let source_policy = reads.source_policy_handle().unwrap_or_else(|| {
        unreachable!("a current-tree read service always compiles its source policy")
    });
    let map = Arc::new(reads.workspace_map());
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy,
        map,
        dependency_plan,
        epoch,
    })
}

/// Replaces the files `changes` names and shares every other file with `previous`.
///
/// Index-owned configuration, the compiled source policy, the dependency plan, and the
/// dependency store carry over unchanged. Other accepted configuration may change without
/// rebuilding source files. An empty change set still produces a candidate because
/// current-tree requests wait for its observation epoch.
fn shared_workspace_candidate(
    previous: &PublishedWorkspace,
    changes: &PathChanges,
    configuration: ConfigurationState,
    epoch: u64,
) -> Result<PublishedWorkspace, ReadError> {
    if changes.is_empty() {
        return Ok(previous.under(configuration, epoch));
    }
    let reads = previous.reads.rebuilt(changes)?;
    let map = Arc::new(reads.workspace_map());
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy: Arc::clone(&previous.source_policy),
        map,
        dependency_plan: previous.dependency_plan.clone(),
        epoch,
    })
}

/// Embeds the declarations `published` describes, so the semantic tier ranks the tree the
/// lexical index already holds.
///
/// The lexical set is not written here. The lexical lane commits it in publication order,
/// and a request that captures `published` before that transaction lands is told so;
/// embedding runs on this lane instead, because one declaration's vector can cost more
/// than the whole freshness wait a request is bounded by.
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
            "a visible file exceeds search.text.max_chunk and was indexed in chunks; exclude it \
             in [source], or increase search.text.max_chunk, to avoid this"
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

/// How long the lane lets one lexical transaction run before it records the delay.
///
/// The bound is the store's own, never a rebuild's: a rebuild hands its write to the lane
/// when it publishes and does not wait for the transaction. The lane keeps waiting for a
/// transaction past this deadline, because a second one would only queue behind it on
/// the database's write turn, so the deadline bounds when `rift://logs` names the delay,
/// and the transaction's own end decides what the store is owed. A transaction writing
/// many units gets a longer deadline, derived by [`commit_deadline`].
///
/// Fixed, not `[server] readiness_timeout`: a slow lexical transaction is an internal
/// write-side condition independent of how long an operator lets a read wait for the
/// workspace to settle.
pub(crate) const LEXICAL_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
/// What each written unit adds to a lexical transaction's deadline, before the cap.
pub(crate) const LEXICAL_UNIT_COMMIT_BUDGET: Duration = Duration::from_millis(1);
/// The longest deadline any one lexical transaction gets, whatever its unit count.
pub(crate) const LEXICAL_COMMIT_TIMEOUT_MAX: Duration = Duration::from_secs(600);
/// Writes the lane holds behind the one it is running. A write handed to a full lane
/// supersedes every held one, and the store is then owed a whole replace.
pub(crate) const LEXICAL_COMMITS_MAX: usize = 4;
/// What the store is owed a whole replace for when a write handed to a full lane dropped
/// the held ones, as a search's warning renders it.
const SUPERSEDED_CAUSE: &str = "a write handed to a full lexical lane superseded the held ones, whose rows never reached \
     the store";

/// The deadline one transaction writing `unit_count` units gets: at least
/// [`LEXICAL_COMMIT_TIMEOUT`], one [`LEXICAL_UNIT_COMMIT_BUDGET`] per unit when that is
/// longer, and never past [`LEXICAL_COMMIT_TIMEOUT_MAX`].
pub(crate) fn commit_deadline(unit_count: usize) -> Duration {
    let units = u32::try_from(unit_count).unwrap_or(u32::MAX);
    LEXICAL_UNIT_COMMIT_BUDGET
        .checked_mul(units)
        .unwrap_or(LEXICAL_COMMIT_TIMEOUT_MAX)
        .clamp(LEXICAL_COMMIT_TIMEOUT, LEXICAL_COMMIT_TIMEOUT_MAX)
}

/// What one lexical commit writes.
#[derive(Debug)]
pub(crate) enum LexicalWrite {
    /// The whole unit set, replacing whatever is stored.
    Whole(Vec<LexicalUnit>),
    /// Only the units one change set names.
    Change(LexicalChange),
}

impl LexicalWrite {
    /// Whether this write would leave the stored set exactly as it is, so the store owes
    /// nothing for it and no transaction opens.
    ///
    /// Only an empty change set reaches this: it shares its predecessor's snapshot, so the
    /// stamp already names the tree revision the candidate answers under.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Whole(_) => false,
            Self::Change(change) => change.is_empty(),
        }
    }

    /// How many units this write moves: every unit a whole replace inserts, or every path
    /// a change replaces beside every unit it inserts.
    pub(crate) fn unit_count(&self) -> usize {
        match self {
            Self::Whole(units) => units.len(),
            Self::Change(change) => change
                .replaced()
                .len()
                .saturating_add(change.inserted().len()),
        }
    }

    /// The write's form, as a record names it.
    const fn form(&self) -> &'static str {
        match self {
            Self::Whole(_) => "whole",
            Self::Change(_) => "change",
        }
    }

    /// The write without every unit whose `content` exceeds `unit_bytes_max`, beside the
    /// units left out.
    ///
    /// The store refuses a whole batch over one oversized unit, and a publication cannot
    /// wait on an operator raising a bound the configuration does not expose. The unit is
    /// left out of the lexical index instead: its file stays in the syntax index and keeps
    /// answering `get_symbol` and symbol search, and only its text ranking is absent. A
    /// change set keeps every replaced path, so the stored units of a file whose one unit
    /// grew past the bound are still deleted.
    fn within_unit_bound(self, unit_bytes_max: usize) -> (Self, Vec<LexicalUnit>) {
        let mut left_out = Vec::new();
        let mut within = |unit: LexicalUnit| {
            if unit.content().len() > unit_bytes_max {
                left_out.push(unit);
                None
            } else {
                Some(unit)
            }
        };
        let kept = match self {
            Self::Whole(units) => Self::Whole(units.into_iter().filter_map(&mut within).collect()),
            Self::Change(change) => {
                let (replaced, inserted) = change.into_parts();
                let inserted = inserted.into_iter().filter_map(&mut within).collect();
                Self::Change(LexicalChange::new(replaced, inserted))
            }
        };
        (kept, left_out)
    }

    /// Runs this write against `store` as one transaction stamping `tree_revision`.
    async fn commit_to<Store: LexicalStore>(
        &self,
        store: &Store,
        tree_revision: &str,
    ) -> Result<(), SearchError> {
        match self {
            Self::Whole(units) => store.replace(units, tree_revision).await,
            Self::Change(change) => store.apply(change, tree_revision).await,
        }
    }
}

/// Records each unit one commit left out of the lexical index, once per build, in the
/// form `file left out of the index` takes.
fn record_units_left_out(left_out: &[LexicalUnit], unit_bytes_max: usize) {
    for unit in left_out {
        tracing::warn!(
            component = "index",
            operation = "index.build",
            path = unit.path().as_str(),
            observed = unit.content().len(),
            maximum = unit_bytes_max,
            "lexical unit left out of the search index: its content exceeds the unit byte \
             bound; the file still answers get_symbol and symbol search"
        );
    }
}

/// The store one lexical lane writes: the workspace search index, or a test's stand-in.
pub(crate) trait LexicalStore: Send + Sync + 'static {
    /// Replaces the stored unit set with `units` and stamps `tree_revision`, in one
    /// transaction.
    fn replace(
        &self,
        units: &[LexicalUnit],
        tree_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send;

    /// Applies `change` and stamps `tree_revision`, in one transaction.
    fn apply(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send;
}

impl LexicalStore for SearchIndex {
    fn replace(
        &self,
        units: &[LexicalUnit],
        tree_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send {
        self.replace_lexical(units, tree_revision)
    }

    fn apply(
        &self,
        change: &LexicalChange,
        tree_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send {
        self.apply_lexical(change, tree_revision)
    }
}

/// One write handed to the lane, with the publication it was derived for.
#[derive(Debug)]
struct LexicalCommit {
    write: LexicalWrite,
    /// The publication the write answers for. It names the tree revision the transaction
    /// stamps, and derives the whole unit set when the store is owed one.
    published: Arc<PublishedWorkspace>,
}

impl LexicalCommit {
    fn tree_revision(&self) -> &str {
        self.published.reads.tree_revision()
    }
}

/// Where one tree revision stands with the lexical lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LexicalCommitState {
    /// The write for it is held or running: the store holds it once that transaction
    /// ends.
    Committing,
    /// No write for it is held or running, and the store missed a commit for the reason
    /// `cause` renders: the next publication's transaction replaces the whole unit set.
    Owed {
        /// The refused transaction's own rendering, or the supersession that dropped the
        /// held writes.
        cause: String,
    },
    /// No write for it is held or running, and nothing is owed: the store holds it, or has
    /// moved past it to a newer publication.
    Settled,
}

/// What the lane has been handed and not yet run, and what the store is owed.
#[derive(Debug, Default)]
struct LexicalBacklog {
    /// Writes waiting behind the running one, oldest first, at most
    /// [`LEXICAL_COMMITS_MAX`].
    held: VecDeque<LexicalCommit>,
    /// The tree revision whose transaction is running, while one is.
    running: Option<String>,
    /// Why the store missed a commit, while it has: a write superseded before it ran, or
    /// a transaction that failed. The next transaction then replaces the whole unit set.
    whole_owed: Option<String>,
    /// Whether the lane's task has ended, so a later write has no one to run it.
    ended: bool,
}

impl LexicalBacklog {
    /// Holds `commit` for the task, superseding what a newer write makes moot.
    ///
    /// A full backlog is a store that cannot keep up: every held write is dropped for the
    /// new one, and the store is then owed a whole replace, because the dropped changes'
    /// rows never reach it. While a whole replace is owed only the newest held write
    /// matters, since its publication is the one the replace derives from. Returns how
    /// many held writes were dropped.
    fn hand(&mut self, commit: LexicalCommit) -> usize {
        if self.held.len() >= LEXICAL_COMMITS_MAX {
            self.whole_owed = Some(SUPERSEDED_CAUSE.to_owned());
        }
        self.held.push_back(commit);
        if self.whole_owed.is_none() {
            return 0;
        }
        let dropped = self.held.len().saturating_sub(1);
        self.held.drain(..dropped);
        dropped
    }

    /// Takes the oldest held write for the task, with whether a whole replace is owed.
    /// The owed replace becomes that transaction's responsibility: a failure hands it
    /// back through [`Self::owe_whole`].
    fn take_next(&mut self) -> Option<(LexicalCommit, bool)> {
        let commit = self.held.pop_front()?;
        let whole_owed = self.whole_owed.take().is_some();
        self.running = Some(commit.tree_revision().to_owned());
        Some((commit, whole_owed))
    }

    /// Records that the store missed a commit for the reason `cause` renders, so the next
    /// transaction replaces the whole unit set and only the newest held write still
    /// matters.
    fn owe_whole(&mut self, cause: String) {
        self.whole_owed = Some(cause);
        let dropped = self.held.len().saturating_sub(1);
        self.held.drain(..dropped);
    }

    /// Where `tree_revision` stands: held or running, missed, or settled.
    fn state_of(&self, tree_revision: &str) -> LexicalCommitState {
        let running = self.running.as_deref() == Some(tree_revision);
        let held = self
            .held
            .iter()
            .any(|commit| commit.tree_revision() == tree_revision);
        if running || held {
            return LexicalCommitState::Committing;
        }
        match &self.whole_owed {
            Some(cause) => LexicalCommitState::Owed {
                cause: cause.clone(),
            },
            None => LexicalCommitState::Settled,
        }
    }
}

/// The backlog and the wake-ups the lane's handles and its task share.
#[derive(Debug, Default)]
struct LexicalQueue {
    backlog: SyncMutex<LexicalBacklog>,
    /// Wakes the task when a write lands.
    handed: Notify,
    /// Wakes every waiter when a commit lands: a transaction ended, success or failure,
    /// or a write handed to a full lane superseded the held ones. Each moves some tree
    /// revision out of `Committing`, which is what a waiter reads again.
    landed: Notify,
}

impl LexicalQueue {
    fn locked(&self) -> std::sync::MutexGuard<'_, LexicalBacklog> {
        self.backlog
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The lexical lane: one long-lived task owning every write to the lexical index, and the
/// handle a publication hands its write to.
///
/// A publication hands its write over and returns. The lane runs the writes in the order
/// they were handed, which is publication order, one transaction at a time: `SQLite`
/// would otherwise meet two rebuilds' writes as contention rather than as an order. A
/// request that captures a publication before its transaction lands waits for the
/// landing under `[server] readiness_timeout`, and answers from identifier matching,
/// saying so, only once that budget runs out.
///
/// The lane is bounded by [`LEXICAL_COMMITS_MAX`] held writes. A write handed to a full
/// lane supersedes every held one, and a transaction that fails or is superseded leaves
/// the store owing a whole replace, which the next transaction pays from its
/// publication's whole unit set rather than applying a change on top of rows the store
/// missed.
///
/// Vector embedding never rides this lane: a lexical row costs one delete and one insert
/// batch, while embedding one declaration can run for longer than the freshness wait a
/// request is bounded by.
#[derive(Clone, Debug)]
pub(crate) struct LexicalLane {
    queue: Arc<LexicalQueue>,
}

impl LexicalLane {
    /// Spawns the lane's task over `index` and returns the handle a publication hands its
    /// write to.
    ///
    /// The task ends when the server does, racing the same cancellation token the index
    /// supervisor runs under. A write handed over after that is dropped with a debug line:
    /// no later search reads the store it would have written.
    pub(crate) fn spawn(
        index: Arc<SearchIndex>,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
    ) -> Self {
        let unit_bytes_max =
            usize::try_from(index.lexical_limits().unit_bytes_max()).unwrap_or(usize::MAX);
        Self::spawn_over(index, unit_bytes_max, blocking, cancellation)
    }

    /// Spawns the lane's task over any [`LexicalStore`] whose per-unit content bound is
    /// `unit_bytes_max`. Whole unit sets the lane derives itself run on `blocking`.
    pub(crate) fn spawn_over<Store: LexicalStore>(
        store: Arc<Store>,
        unit_bytes_max: usize,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
    ) -> Self {
        let queue = Arc::new(LexicalQueue::default());
        let task = LexicalTask {
            store,
            blocking,
            queue: Arc::clone(&queue),
            unit_bytes_max,
            cancellation,
        };
        tokio::spawn(task.run());
        Self { queue }
    }

    /// Hands `write` to the lane for `published` and returns, never awaiting the
    /// transaction.
    ///
    /// A write that changes nothing is dropped unless the store is owed a whole replace:
    /// the stored stamp already names the tree revision `published` answers under. A
    /// write handed to a full lane supersedes every held one, and the record says so;
    /// the superseded revisions are owed from then on, so the waiters on
    /// [`Self::landed`] are woken to read that.
    pub(crate) fn request(&self, write: LexicalWrite, published: Arc<PublishedWorkspace>) {
        let tree_revision = published.reads.tree_revision().to_owned();
        let mut backlog = self.queue.locked();
        if backlog.ended {
            drop(backlog);
            tracing::debug!(
                component = "search",
                operation = "search.commit",
                tree_revision,
                "the lexical lane has ended, so this publication is not committed"
            );
            return;
        }
        if write.is_empty() && backlog.whole_owed.is_none() {
            return;
        }
        let superseded = backlog.hand(LexicalCommit { write, published });
        drop(backlog);
        if superseded > 0 {
            tracing::warn!(
                component = "search",
                operation = "search.commit",
                tree_revision,
                superseded,
                "the lexical lane was handed a write it had no room for, so the held writes \
                 are superseded and the next transaction replaces the whole unit set"
            );
            self.queue.landed.notify_waiters();
        }
        self.queue.handed.notify_one();
    }

    /// Where `tree_revision` stands with the lane, for a search that found the store
    /// holding another tree.
    pub(crate) fn commit_state(&self, tree_revision: &str) -> LexicalCommitState {
        self.queue.locked().state_of(tree_revision)
    }

    /// A wake-up for the lane's next landing: a transaction's end, success or failure, or
    /// a write superseding the held ones.
    ///
    /// Created before [`Self::commit_state`] is read, it cannot miss a landing between
    /// that read and its await: tokio's `Notified` "is guaranteed to receive wakeups
    /// from `notify_waiters()` as soon as it has been created, even if it has not yet
    /// been polled".
    pub(crate) fn landed(&self) -> Notified<'_> {
        self.queue.landed.notified()
    }

    /// Whether the lane's task has ended, which a cancelled token causes.
    #[cfg(test)]
    pub(crate) fn has_ended(&self) -> bool {
        self.queue.locked().ended
    }

    /// Whether the store is owed a whole replace right now.
    #[cfg(test)]
    pub(crate) fn owes_whole(&self) -> bool {
        self.queue.locked().whole_owed.is_some()
    }
}

/// The write one candidate owes the lexical index, handed to the lane once the candidate
/// publishes, under the same linearization publication takes, so the lane runs writes in
/// publication order.
pub(crate) struct LexicalHandoff {
    lane: LexicalLane,
    write: LexicalWrite,
}

impl LexicalHandoff {
    pub(crate) const fn new(lane: LexicalLane, write: LexicalWrite) -> Self {
        Self { lane, write }
    }

    /// Hands the write to the lane for `published`, the candidate that just became
    /// current.
    fn hand_over(self, published: Arc<PublishedWorkspace>) {
        self.lane.request(self.write, published);
    }
}

/// The lane's task: takes each held write in order and runs it as one transaction.
struct LexicalTask<Store> {
    store: Arc<Store>,
    blocking: BlockingExecutor,
    queue: Arc<LexicalQueue>,
    unit_bytes_max: usize,
    cancellation: CancellationToken,
}

impl<Store: LexicalStore> LexicalTask<Store> {
    /// Runs held writes until the token is cancelled, then marks the lane ended. Every
    /// transaction's end, success or failure, wakes the waiters on
    /// [`LexicalLane::landed`] once the backlog records it.
    async fn run(self) {
        while let Some((commit, whole_owed)) = self.next_commit().await {
            let outcome = self.transaction(commit, whole_owed).await;
            let mut backlog = self.queue.locked();
            backlog.running = None;
            if let Err(error) = outcome {
                backlog.owe_whole(error.to_string());
            }
            drop(backlog);
            self.queue.landed.notify_waiters();
        }
        self.queue.locked().ended = true;
    }

    /// Waits for the next held write, or for the token; nothing once it is cancelled.
    async fn next_commit(&self) -> Option<(LexicalCommit, bool)> {
        loop {
            if self.cancellation.is_cancelled() {
                return None;
            }
            if let Some(taken) = self.queue.locked().take_next() {
                return Some(taken);
            }
            tokio::select! {
                () = self.cancellation.cancelled() => return None,
                () = self.queue.handed.notified() => {}
            }
        }
    }

    /// Runs one write as one transaction, under the deadline its unit count derives.
    ///
    /// The transaction runs on its own task: the store executes its statements inline,
    /// so the lane's own task could not observe the deadline while running them. The lane
    /// keeps waiting past the deadline - it records the delay and lets the transaction
    /// end - because a second transaction would only queue behind this one on the
    /// database's write turn.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the whole set could not be derived, when the transaction
    /// failed in the store, when its task ended without an answer, or when the lane was
    /// cancelled while it ran. Every one leaves the store owing a whole replace.
    ///
    /// # Cancel safety
    ///
    /// The lane's token cancels the transaction, never the other way round: the lane
    /// aborts the transaction's task and waits for the abort to land before it answers.
    /// The abort drops the store's future, and with it the write turn it holds and the
    /// driver's `Transaction`, whose drop sends `ROLLBACK` to its connection's task
    /// without waiting for it (`toasty::db::Transaction`: "If dropped without calling
    /// commit or rollback, the transaction is automatically rolled back"). The connection
    /// runs the rollback after the statement it is executing, so the write turn frees
    /// within one statement's time. A rolled-back whole replace leaves the store stamped
    /// with the previous tree revision; the next start's rebuild carries no previous
    /// publication (`RebuildRequest::initial`), so `RebuildRequest::change_set` answers
    /// `ChangeSet::Full` and `lexical_write` derives a whole replace from it: the first
    /// commit after a restart replaces the whole unit set.
    async fn transaction(&self, commit: LexicalCommit, whole_owed: bool) -> Result<(), ReadError> {
        let LexicalCommit { write, published } = commit;
        let write = if whole_owed {
            self.whole_set(&published).await?
        } else {
            write
        };
        let (write, left_out) = write.within_unit_bound(self.unit_bytes_max);
        record_units_left_out(&left_out, self.unit_bytes_max);
        if write.is_empty() {
            return Ok(());
        }
        let deadline = commit_deadline(write.unit_count());
        let form = write.form();
        let tree_revision = published.reads.tree_revision().to_owned();
        let store = Arc::clone(&self.store);
        let stamped = tree_revision.clone();
        let mut running = tokio::spawn(async move { write.commit_to(&*store, &stamped).await });
        let ended = tokio::select! {
            ended = &mut running => ended,
            () = self.cancellation.cancelled() => {
                abort_transaction(running).await;
                return Err(lexical_unavailable("the lexical lane ended while the transaction ran"));
            }
            () = tokio::time::sleep(deadline) => {
                record_commit_delay(&tree_revision, form, deadline);
                running.await
            }
        };
        let outcome = match ended {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => ReadFault::unavailable("lexical index commit", error.detail()),
            Err(_) => lexical_unavailable("the lexical transaction's task ended without an answer"),
        };
        record_commit_failure(&tree_revision, form, &outcome);
        Err(outcome)
    }

    /// The whole unit set `published` derives, on the worker pool.
    async fn whole_set(
        &self,
        published: &Arc<PublishedWorkspace>,
    ) -> Result<LexicalWrite, ReadError> {
        let tree_revision = published.reads.tree_revision();
        let deriving = Arc::clone(published);
        self.blocking
            .run("lexical unit derivation", move || {
                Ok(LexicalWrite::Whole(deriving.reads.lexical_units()))
            })
            .await
            .inspect_err(|error| record_commit_failure(tree_revision, "whole", error))
    }
}

/// Aborts a running transaction's task and waits for the abort to land, so the future
/// holding the store's transaction and the write turn is dropped before the lane goes on.
///
/// The join answers with the abort, or with the outcome of a transaction that ended
/// before the abort reached it. The lane is ending either way and the next start replaces
/// the whole unit set, so neither answer changes what it does.
async fn abort_transaction(running: JoinHandle<Result<(), SearchError>>) {
    running.abort();
    let _ = running.await;
}

/// Records one transaction that ran past its deadline, once.
fn record_commit_delay(tree_revision: &str, form: &'static str, deadline: Duration) {
    tracing::error!(
        component = "search",
        operation = "search.commit",
        tree_revision,
        form,
        deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
        "the lexical transaction ran past its deadline; the lane waits for it to end, and \
         search answers from identifier matching meanwhile"
    );
}

/// Records one commit the store did not take, with its cause.
fn record_commit_failure(tree_revision: &str, form: &'static str, error: &ReadError) {
    let causes = rift_core::causes(error).join("; ");
    tracing::error!(
        component = "search",
        operation = "search.commit",
        tree_revision,
        form,
        error = %error,
        causes,
        "the lexical commit failed; the next publication replaces the whole unit set"
    );
}

/// One lexical commit that could not reach the store, as the lane records it.
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
    /// Every path the observation named already held the bytes the publication holds,
    /// under the same configuration: nothing was built, the publication now carries the
    /// observation's epoch, and nothing is handed to the lanes.
    Unchanged,
    /// New observation invalidated candidate before publication.
    Superseded,
    /// The supervisor was cancelled while the capture or the publication ran; nothing
    /// publishes and the blocking thread ends on its own.
    Cancelled,
}

/// Owns native watcher and reconciles coalesced invalidations until shutdown.
///
/// A published rebuild hands its catalog to the dependency lane and its snapshot to the
/// population lane, then moves on to the next batch. The supervisor awaiting a pass itself
/// would hold the whole reconciliation loop for as long as that pass ran, and the filesystem
/// does not stop moving meanwhile.
pub(crate) async fn run_index_supervisor(
    watcher: notify::RecommendedWatcher,
    invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
) {
    let capture = workspace_capture(&context.dependencies);
    run_index_supervisor_with(watcher, invalidations, context, capture).await;
}

/// Runs the supervisor loop over an injectable capture, so a test can hold one capture
/// open at the moment a cancellation lands.
///
/// A cancelled rebuild ends the loop: the token is cancelled only at shutdown, and the
/// capture it interrupted finishes on its own thread with nothing left to publish.
pub(crate) async fn run_index_supervisor_with(
    _watcher: notify::RecommendedWatcher,
    mut invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
    capture: impl CaptureWorkspace + Clone + Send + 'static,
) {
    let validation = Arc::clone(&context.validation);
    let published = Arc::clone(&context.published);
    let population = context.population.clone();
    // The guard reports the supervisor's end however it comes: a return, a cancellation, or
    // a panic unwinding this task. A reader that meets the readiness deadline needs to know
    // which of those happened, and the process that panicked cannot tell them afterwards.
    let _running = SupervisorRunning::new(
        Arc::clone(&validation.supervisor_running),
        Arc::clone(&validation.changed),
    );
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
        let result = rebuild_workspace(&context, request, capture.clone())
            .instrument(tracing::info_span!(
                "index.build",
                component = "index",
                trigger = "filesystem",
                epoch
            ))
            .await;
        match result {
            Ok(RebuildOutcome::Published) => {
                let (current, _) = published.read().await.snapshot();
                context.dependency_lane.request_for(&current);
                if let Some(lane) = population.as_ref() {
                    lane.request(current);
                }
            }
            Ok(RebuildOutcome::Unchanged | RebuildOutcome::Superseded) => {}
            Ok(RebuildOutcome::Cancelled) => return,
            Err(error) => publish_rebuild_failure(&context, epoch, error).await,
        }
    }
}

/// Records one failed rebuild under the publication lane and wakes the requests waiting
/// on it; a failure the pool can no longer record marks the watch unhealthy instead.
async fn publish_rebuild_failure(context: &IndexSupervisorContext, epoch: u64, error: ReadError) {
    tracing::warn!(
        component = "index",
        operation = "index.build",
        epoch,
        error_code = error.descriptor().code(),
        "index rebuild failed"
    );
    let failed_state = Arc::clone(&context.published);
    let failed_validation = Arc::clone(&context.validation);
    let recorded = context
        .blocking
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
        let _ = context.validation.observe_watch_failure();
    }
    context.validation.changed.notify_waiters();
}

/// Rebuilds and atomically publishes only a still-current candidate, handing the lexical
/// lane what the candidate owes the store as it publishes.
///
/// The capture and the publication run as separate operations. The candidate is captured
/// with the change lane held, so no workspace mutation lands inside it. The publication
/// check then takes the same linearization point filesystem observation takes, so a
/// candidate superseded while it was built cannot become current, and the lexical write
/// is handed over under that same point, so the lane runs writes in publication order.
/// The rebuild never waits for the transaction: a request that captures the publication
/// before the transaction lands is told so by `search`.
///
/// An observation whose every named path already holds the bytes the publication holds,
/// under the same configuration, is answered by that publication: the capture builds
/// nothing and answers [`CapturedRebuild::Unchanged`], the candidate sharing the
/// publication's read service is stamped with the observation's epoch under the same
/// linearization, so requests waiting on that epoch proceed, and the outcome is
/// [`RebuildOutcome::Unchanged`]: nothing is recorded as a publication, and the supervisor
/// hands nothing to the lanes.
///
/// A superseded candidate hands nothing to the lane. Its workspace never publishes, and
/// pending requests keep waiting because the observed epoch still differs from the
/// published one.
///
/// Each blocking operation races the supervisor's cancellation token. A stop that lands
/// while the capture scans a large tree answers [`RebuildOutcome::Cancelled`] at once
/// instead of after the scan, and the supervisor ends on that answer.
///
/// # Errors
///
/// Returns [`ReadError`] when the capture fails. Nothing publishes then, so a current-tree
/// request meets the recorded rebuild failure and answers from the previous snapshot.
///
/// # Cancel safety
///
/// Dropping this future, or losing the race to the token, stops neither the capture nor
/// the publication once its closure runs. [`BlockingExecutor::run`] moves its permit into
/// the `spawn_blocking` closure and drops it only when that closure returns, and tokio
/// documents the closure's thread as beyond reach: "tasks spawned using `spawn_blocking`
/// cannot be aborted because they are not async", and "If a `JoinHandle` is dropped, then
/// the task continues running in the background and its return value is lost". The
/// running closure therefore holds the change lane and its permit until it ends on its
/// own, and its result is dropped. A future dropped while it still queues for the permit
/// spawns nothing. The capture meets the token at its next phase boundary and returns
/// its work to the observation; a publication that took its locks before the token was
/// cancelled still lands, and nobody hands it to the lanes.
pub(crate) async fn rebuild_workspace(
    context: &IndexSupervisorContext,
    request: RebuildRequest,
    capture: impl CaptureWorkspace + Send + 'static,
) -> Result<RebuildOutcome, ReadError> {
    let epoch = request.epoch;
    let root = context.root.clone();
    let limits = context.limits;
    let captured_state = Arc::clone(&context.published);
    let captured_validation = Arc::clone(&context.validation);
    let change_lane = Arc::clone(&context.change_lane);
    let captured = tokio::select! {
        () = context.validation.cancellation.cancelled() => return Ok(RebuildOutcome::Cancelled),
        captured = context.blocking.run("filesystem index rebuild", move || {
            capture_rebuild(
                &root,
                limits,
                &captured_state,
                &change_lane,
                &captured_validation,
                request,
                capture,
            )
        }) => captured?,
    };
    match captured {
        CapturedRebuild::Candidate {
            published,
            write,
            work,
        } => {
            let lexical = context
                .lexical
                .clone()
                .map(|lane| LexicalHandoff::new(lane, write));
            let outcome = publish_captured(context, published, work, lexical).await?;
            if outcome == RebuildOutcome::Published {
                trace_publication(epoch);
            }
            Ok(outcome)
        }
        CapturedRebuild::Unchanged { published, work } => {
            let outcome = publish_captured(context, published, work, None).await?;
            Ok(match outcome {
                RebuildOutcome::Published => RebuildOutcome::Unchanged,
                other => other,
            })
        }
        CapturedRebuild::Superseded => Ok(RebuildOutcome::Superseded),
        CapturedRebuild::Cancelled => Ok(RebuildOutcome::Cancelled),
    }
}

/// Publishes one captured candidate on the pool, racing the supervisor's cancellation.
///
/// # Errors
///
/// Returns [`ReadError`] when the pool refuses the publication.
///
/// # Cancel safety
///
/// As for [`rebuild_workspace`]: a publication whose closure already runs still lands,
/// and this future answers [`RebuildOutcome::Cancelled`] without waiting for it.
async fn publish_captured(
    context: &IndexSupervisorContext,
    candidate: Arc<PublishedWorkspace>,
    work: PendingWork,
    lexical: Option<LexicalHandoff>,
) -> Result<RebuildOutcome, ReadError> {
    let root = context.root.clone();
    let published = Arc::clone(&context.published);
    let validation = Arc::clone(&context.validation);
    let publication = context
        .blocking
        .run("filesystem index publication", move || {
            Ok(finish_rebuild(
                &root,
                &published,
                &validation,
                &candidate,
                work,
                lexical,
            ))
        });
    tokio::select! {
        () = context.validation.cancellation.cancelled() => Ok(RebuildOutcome::Cancelled),
        outcome = publication => outcome,
    }
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
    /// Every path the observation named already holds the bytes the publication holds,
    /// under the same configuration. The candidate shares the publication's read service
    /// and carries the observation's epoch; publishing it answers the requests waiting on
    /// that epoch with nothing built and no lexical write owed.
    Unchanged {
        /// The publication's twin under the observation's epoch.
        published: Arc<PublishedWorkspace>,
        /// The observation's work, returned to the supervisor when nothing publishes.
        work: PendingWork,
    },
    /// The observation was already superseded, or configuration moved during the capture.
    Superseded,
    /// The supervisor was cancelled before the capture ran, or before its candidate was
    /// handed on; the observation's work is returned and nothing publishes.
    Cancelled,
}

/// Captures one candidate while the mutation lane is held.
pub(crate) fn capture_rebuild(
    root: &Path,
    limits: WorkspaceIndexLimits,
    published: &RwLock<IndexState>,
    change_lane: &ChangeLane,
    validation: &IndexValidation,
    request: RebuildRequest,
    capture: impl CaptureWorkspace,
) -> Result<CapturedRebuild, ReadError> {
    change_lane.run(|| capture_rebuild_with(root, limits, published, validation, request, capture))
}

/// Runs one serialized capture over an injectable candidate builder, so tests can force
/// each superseded arm deterministically instead of racing the scan.
///
/// An attempt that captures nothing returns its observation's work to the supervisor:
/// publication is the acknowledgement that lets those paths be dropped, so a superseded
/// candidate leaves the next rebuild owing exactly what this one owed plus whatever landed
/// while it ran.
///
/// A stable candidate whose change set is empty, under the configuration the publication
/// was built under, built nothing: `shared_workspace_candidate` shares the publication's
/// read service when [`PathChanges::resolve`] dropped every observed path, so no file was
/// read again and no lexical write is owed. Such a capture answers
/// [`CapturedRebuild::Unchanged`], and the caller stamps the publication with the
/// observation's epoch instead of publishing and handing on a twin of it.
///
/// The supervisor's cancellation is checked at the phase boundaries: before the capture
/// runs, and before the candidate's lexical write is derived. A stop that lands during a
/// long scan therefore ends the attempt at the next boundary, with its work returned.
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
    if validation.cancellation.is_cancelled() {
        validation.restore_pending(request.work);
        return Ok(CapturedRebuild::Cancelled);
    }
    let previous = published.blocking_read().snapshot().0;
    request.previous = Some(Arc::clone(&previous));
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
    if validation.cancellation.is_cancelled() {
        validation.restore_pending(request.work);
        return Ok(CapturedRebuild::Cancelled);
    }
    let unchanged = change_set.is_empty()
        && candidate.configuration.fingerprint == previous.configuration.fingerprint;
    if unchanged {
        return Ok(CapturedRebuild::Unchanged {
            published: candidate,
            work: request.work,
        });
    }
    let write = lexical_write(&candidate, &change_set);
    Ok(CapturedRebuild::Candidate {
        published: candidate,
        write,
        work: request.work,
    })
}

/// Publishes one candidate and hands the lane its lexical write, or returns its
/// observation's work when the tree moved underneath it or the supervisor was cancelled.
pub(crate) fn finish_rebuild(
    root: &Path,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: &Arc<PublishedWorkspace>,
    work: PendingWork,
    lexical: Option<LexicalHandoff>,
) -> RebuildOutcome {
    let outcome = publish_rebuild_after(root, published, validation, candidate, lexical, || {});
    if outcome == RebuildOutcome::Published {
        validation.changed.notify_waiters();
    } else {
        validation.restore_pending(work);
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

/// Atomically publishes candidate when observation still matches, with no lexical write to
/// hand over.
#[cfg(test)]
pub(crate) fn publish_rebuild(
    root: &Path,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: &Arc<PublishedWorkspace>,
) -> RebuildOutcome {
    publish_rebuild_after(root, published, validation, candidate, None, || {})
}

/// Publishes under observation lane; hook enables deterministic overlap tests.
///
/// A published candidate's lexical write is handed to the lane before the lane's lock
/// releases, so two publications hand their writes over in the order they published.
/// A candidate that meets a cancelled token under those locks is refused unpublished:
/// the token is cancelled only at shutdown, and the lane it would be handed to has ended.
///
/// A candidate whose epoch the observation already passed still publishes when it holds
/// what every path observed since its capture now holds; see [`answered_candidate`].
pub(crate) fn publish_rebuild_after(
    root: &Path,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: &Arc<PublishedWorkspace>,
    lexical: Option<LexicalHandoff>,
    after_state_lock: impl FnOnce(),
) -> RebuildOutcome {
    let publication = validation.locked_pending();
    let observed_epoch = validation.observed_epoch();
    let candidate = answered_candidate(root, candidate, &publication, observed_epoch);
    let mut state = published.blocking_write();
    after_state_lock();
    if validation.cancellation.is_cancelled() {
        drop(state);
        drop(publication);
        return RebuildOutcome::Cancelled;
    }
    let Some(candidate) = candidate else {
        drop(state);
        drop(publication);
        return RebuildOutcome::Superseded;
    };
    let publishing = Arc::clone(&candidate);
    // IndexState::publish owns the still-current check, so a superseded
    // candidate is rejected in exactly one place.
    let published = state.publish(candidate, observed_epoch);
    if published {
        validation.replace_publication_locked(&publishing);
        if let Some(handoff) = lexical {
            handoff.hand_over(publishing);
        }
    }
    drop(state);
    drop(publication);
    if published {
        RebuildOutcome::Published
    } else {
        RebuildOutcome::Superseded
    }
}

/// The candidate to publish under `observed_epoch`, or nothing when the observation
/// genuinely moved past it.
///
/// A candidate answers the epoch its capture took. When the observation moved while the
/// capture ran, the paths that moved are still the lane's pending work, so the same
/// comparison a rebuild makes - [`PathChanges::resolve`] against the candidate's own
/// digests - says whether the candidate already holds what they name. A candidate that
/// holds all of them answers the later observation too and publishes as its twin under
/// that epoch: a change's own write reaches the watcher after the change captured it,
/// and reading the same bytes a second time is the work this drops. An observation that
/// names a path the candidate does not hold, one that asks for the whole workspace, or
/// one made while `rift.toml` moved, supersedes the candidate.
///
/// Work is one digest per observed path, bounded by how many paths one observation
/// retains, and it runs under the publication lane alone.
fn answered_candidate(
    root: &Path,
    candidate: &Arc<PublishedWorkspace>,
    pending: &PendingWork,
    observed_epoch: u64,
) -> Option<Arc<PublishedWorkspace>> {
    if candidate.epoch == observed_epoch {
        return Some(Arc::clone(candidate));
    }
    if pending.covers_whole_workspace()
        || candidate.configuration.fingerprint != configuration_fingerprint(root)
    {
        return None;
    }
    let observed = observed_digests(root, &pending.paths, &candidate.source_policy)?;
    let changes = PathChanges::resolve(observed, |path| candidate.reads.file_digest(path));
    if !changes.is_empty() {
        return None;
    }
    let twin = candidate.under(candidate.configuration.clone(), observed_epoch);
    Some(Arc::new(twin))
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
pub(crate) mod lexical_double {
    //! A lexical store a test steers, for the lane and for a server built over it.

    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rift_index::{LexicalChange, LexicalUnit};
    use rift_search::{SearchError, SearchFault, SearchIndex, SearchViolation};
    use tokio::sync::Semaphore;

    use super::LexicalStore;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Polls one lane pass a test waits on before it gives up: three seconds, at
    /// [`LANE_POLL`] each.
    pub(crate) const LANE_ATTEMPTS_MAX: usize = 60;
    /// Wait between two reads of a store the lane has not stamped yet.
    pub(crate) const LANE_POLL: Duration = Duration::from_millis(50);

    /// A lexical store a test steers: every write records what it was asked and waits for
    /// one permit before it answers, refusing changes when told to, and writing through to
    /// the attached index when one is held. A write whose future is dropped while the
    /// store still holds it is counted, so a test can prove an abort reached it.
    pub(crate) struct StoreDouble {
        permits: Semaphore,
        calls: Mutex<Vec<(&'static str, String)>>,
        refuse_changes: AtomicBool,
        inner: Mutex<Option<Arc<SearchIndex>>>,
        dropped_while_held: AtomicUsize,
    }

    /// One write the store holds at its gate: its drop is counted unless the write was
    /// released first.
    struct HeldWrite<'store> {
        dropped_while_held: &'store AtomicUsize,
        released: bool,
    }

    impl HeldWrite<'_> {
        /// The write got its permit, so its later drop is a write that ran.
        fn release(mut self) {
            self.released = true;
        }
    }

    impl Drop for HeldWrite<'_> {
        fn drop(&mut self) {
            if !self.released {
                self.dropped_while_held.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    impl StoreDouble {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                permits: Semaphore::new(0),
                calls: Mutex::new(Vec::new()),
                refuse_changes: AtomicBool::new(false),
                inner: Mutex::new(None),
                dropped_while_held: AtomicUsize::new(0),
            })
        }

        /// How many writes had their future dropped while the store still held them.
        pub(crate) fn dropped_while_held(&self) -> usize {
            self.dropped_while_held.load(Ordering::SeqCst)
        }

        /// Writes every released write through to `index` from now on.
        pub(crate) fn attach(&self, index: Arc<SearchIndex>) {
            *self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
        }

        /// Refuses every change write from now on, as a store that failed would.
        pub(crate) fn refuse_changes(&self) {
            self.refuse_changes.store(true, Ordering::SeqCst);
        }

        /// Lets exactly one write, held or still to come, proceed.
        pub(crate) fn release_one(&self) {
            self.permits.add_permits(1);
        }

        /// Every write asked of the store so far: its form and the revision it stamps.
        pub(crate) fn calls(&self) -> Vec<(&'static str, String)> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Waits under a bound until `count` writes were asked of the store.
        pub(crate) async fn calls_within_bound(
            &self,
            count: usize,
        ) -> TestResult<Vec<(&'static str, String)>> {
            for _attempt in 0..LANE_ATTEMPTS_MAX {
                let calls = self.calls();
                if calls.len() >= count {
                    return Ok(calls);
                }
                tokio::time::sleep(LANE_POLL).await;
            }
            Err(format!("the store never saw {count} writes: {:?}", self.calls()).into())
        }

        fn attached(&self) -> Option<Arc<SearchIndex>> {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn write(
            &self,
            form: &'static str,
            tree_revision: &str,
            through: impl Future<Output = Result<(), SearchError>>,
        ) -> Result<(), SearchError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((form, tree_revision.to_owned()));
            let held = HeldWrite {
                dropped_while_held: &self.dropped_while_held,
                released: false,
            };
            let permit =
                self.permits.acquire().await.map_err(|_| {
                    SearchError::new(SearchFault::new(SearchViolation::StoreFailed))
                })?;
            held.release();
            permit.forget();
            through.await
        }
    }

    impl LexicalStore for StoreDouble {
        async fn replace(
            &self,
            units: &[LexicalUnit],
            tree_revision: &str,
        ) -> Result<(), SearchError> {
            let through = async {
                match self.attached() {
                    Some(index) => index.replace_lexical(units, tree_revision).await,
                    None => Ok(()),
                }
            };
            self.write("replace", tree_revision, through).await
        }

        async fn apply(
            &self,
            change: &LexicalChange,
            tree_revision: &str,
        ) -> Result<(), SearchError> {
            let through = async {
                if self.refuse_changes.load(Ordering::SeqCst) {
                    return Err(SearchError::new(
                        SearchFault::new(SearchViolation::StoreFailed)
                            .about("the double refuses changes"),
                    ));
                }
                match self.attached() {
                    Some(index) => index.apply_lexical(change, tree_revision).await,
                    None => Ok(()),
                }
            };
            self.write("apply", tree_revision, through).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;
    use std::sync::Barrier as ThreadBarrier;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    use notify::{Event, EventKind};
    use rift_core::SourceVisibility;
    use rift_index::{LexicalChange, LexicalIndexLimits, WorkspaceIndexLimits};
    use rift_protocol::change::{ChangeResult, PatchParams};
    use rift_protocol::configuration::ServerConfiguration;
    use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits, SemanticReadiness};
    use rift_server::{ChangeService, LspProcessKey, ReadFault};
    use tokio::sync::{Barrier as AsyncBarrier, RwLock};
    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::lexical_double::{LANE_ATTEMPTS_MAX, LANE_POLL, StoreDouble};
    use super::{
        BlockingExecutor, ChangeSet, ConfigurationFingerprint, ConfigurationState, IndexState,
        IndexValidation, LEXICAL_COMMIT_TIMEOUT, LEXICAL_COMMIT_TIMEOUT_MAX, LEXICAL_COMMITS_MAX,
        LEXICAL_UNIT_COMMIT_BUDGET, LexicalCommitState, LexicalLane, LexicalWrite, PathChanges,
        PopulationLane, PublishedWorkspace, RebuildOutcome, RebuildRequest, WorkspaceCandidate,
        build_workspace_candidate, commit_deadline, publish_rebuild, publish_rebuild_after,
        record_rebuild_failure,
    };
    use crate::dependency::{DependencyLane, empty_dependency_store};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn stable_candidate(root: &std::path::Path, epoch: u64) -> TestResult<Arc<PublishedWorkspace>> {
        match build_workspace_candidate(
            root,
            WorkspaceIndexLimits::default(),
            &RebuildRequest::initial(epoch),
            &empty_dependency_store(),
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
        let (definitions, bindings) = invalid.lsp_runtime_configuration();
        assert!(
            definitions.is_empty() && bindings.is_empty(),
            "an invalid file serves no LSP configuration"
        );

        fs::write(
            directory.path().join("rift.toml"),
            "[lsp.ty]\ncommand = \"uvx\"\n[languages.python]\ninclude = [\"**/*.py\"]\nlsp = \"ty\"\n",
        )?;
        let with_lsp = ConfigurationState::accept(directory.path());
        assert!(!with_lsp.has_validation_hooks());
        let (definitions, bindings) = with_lsp.lsp_runtime_configuration();
        let key = LspProcessKey::named("ty");
        assert_eq!(
            definitions
                .get(&key)
                .and_then(|configuration| configuration.command.as_ref())
                .map(rift_protocol::configuration::CommandInput::program),
            Some("uvx"),
            "an accepted LSP definition is served"
        );
        assert_eq!(bindings.get("python"), Some(&key));

        fs::write(
            directory.path().join("rift.toml"),
            "[[hooks]]\nid = \"check\"\nkind = \"build\"\ncommand = [\"cargo\", \"check\"]\nchanged_paths = \"none\"\nwrites = \"none\"\nworking_directory = \"\"\nenvironment = {}\ntimeout = \"30s\"\noutput_limit = \"4kb\"\nfailure_severity = \"error\"\nguarantees = []\ndeterminism = \"deterministic\"\n",
        )?;
        assert!(
            ConfigurationState::accept(directory.path()).has_validation_hooks(),
            "accepted source-read-only hook must be reported"
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
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\n\
             include = [\"src/**\"]\n\
             exclude = [\"src/generated/**\"]\n\
             respect_gitignore = true\n",
        )?;
        let current = stable_candidate(&watched_root, 0)?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_publication(&current);
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
                "a visible baseline text file is read again like provider source",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "src/Cargo.lock",
                source_path("src/Cargo.lock")?,
                "a visible unclassified file names itself",
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
                super::WatchImpact::Paths(vec![
                    rift_core::ProjectPath::new("rift.toml").expect("project path"),
                ]),
                "a written configuration file names itself, and acceptance decides \
                 whether the tree rebuilds",
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
        directory: tempfile::TempDir,
        before: Arc<super::PublishedWorkspace>,
        after: Arc<super::PublishedWorkspace>,
        state: Arc<RwLock<IndexState>>,
        validation: Arc<IndexValidation>,
        _invalidations: tokio::sync::mpsc::Receiver<()>,
    }

    impl PublicationFixture {
        /// The workspace root every candidate in this fixture was captured over.
        fn root(&self) -> &std::path::Path {
            self.directory.path()
        }
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
        validation.install_publication(&before);
        fs::write(directory.path().join("lib.rs"), "pub fn after() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\nrespect_gitignore = false\n",
        )?;
        let epoch = validation.observe_whole_workspace()?;
        let after = stable_candidate(directory.path(), epoch)?;
        Ok(PublicationFixture {
            directory,
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
        let published = fixture
            .validation
            .published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(Arc::ptr_eq(
            published
                .as_ref()
                .expect("the publication must be installed"),
            &fixture.after
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
        let publisher_root = fixture.root().to_path_buf();
        let publisher_state = Arc::clone(&fixture.state);
        let publisher_validation = Arc::clone(&fixture.validation);
        let published_candidate = Arc::clone(&fixture.after);
        let locked = Arc::clone(&publication_locked);
        let released = Arc::clone(&publication_released);
        let publisher = std::thread::spawn(move || {
            publish_rebuild_after(
                &publisher_root,
                &publisher_state,
                &publisher_validation,
                &published_candidate,
                None,
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
        validation.install_publication(&before);
        assert_eq!(validation.observe_whole_workspace()?, 1);
        assert_eq!(validation.observe_whole_workspace()?, 2);
        assert_eq!(
            publish_rebuild(directory.path(), &state, &validation, &after),
            RebuildOutcome::Superseded
        );
        let state_snapshot = state.blocking_read();
        assert_workspace_identity(&state_snapshot.current, &before);
        drop(state_snapshot);
        let published = validation
            .published
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(Arc::ptr_eq(
            published
                .as_ref()
                .expect("the publication must remain installed"),
            &before
        ));
        drop(published);
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
        let current = stable_candidate(&watched_root, 0)?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_publication(&current);
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
            super::WatchImpact::Paths(vec![
                rift_core::ProjectPath::new("rift.toml").expect("project path"),
            ]),
            "a written configuration file names itself: acceptance compares the tables \
             and decides whether the tree rebuilds at all"
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
        let current = stable_candidate(&watched_root, 0)?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_publication(&current);
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

    /// One publication over `root`, installed as what event classification answers from.
    fn installed_publication(
        root: &std::path::Path,
    ) -> TestResult<(Arc<IndexValidation>, Arc<PublishedWorkspace>)> {
        let current = stable_candidate(root, 0)?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_publication(&current);
        Ok((validation, current))
    }

    /// One name event on `path` below `root`, as a rename reaches the watcher.
    fn name_event(root: &std::path::Path, path: &str) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Name(
            notify::event::RenameMode::Any,
        )))
        .add_path(root.join(path))
    }

    #[test]
    fn a_name_event_on_an_absent_extensionless_path_names_that_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        let event_root = directory.path().canonicalize()?;
        let (validation, _current) = installed_publication(directory.path())?;
        let staged = name_event(&event_root, ".tmpk3v9q2");

        assert_eq!(
            super::watch_event_impact(&event_root, &validation, &staged),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new(".tmpk3v9q2")?]),
            "the publisher renames an extensionless staged file over its target; the \
             publication holds nothing under that name, so the event names the path alone"
        );
        Ok(())
    }

    #[test]
    fn a_name_event_on_a_directory_the_publication_holds_files_below_asks_for_the_workspace()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        let event_root = directory.path().canonicalize()?;
        let (validation, _current) = installed_publication(directory.path())?;
        fs::rename(directory.path().join("src"), directory.path().join("attic"))?;
        let renamed_away = name_event(&event_root, "src");

        assert_eq!(
            super::watch_event_impact(&event_root, &validation, &renamed_away),
            super::WatchImpact::WholeWorkspace,
            "a directory renamed away is gone from the disk, and the publication still \
             holds src/lib.rs below its old spelling"
        );
        Ok(())
    }

    #[test]
    fn a_name_event_on_a_directory_on_disk_asks_for_the_workspace() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let event_root = directory.path().canonicalize()?;
        let (validation, _current) = installed_publication(directory.path())?;
        fs::create_dir_all(directory.path().join("examples"))?;
        let moved_in = name_event(&event_root, "examples");

        assert_eq!(
            super::watch_event_impact(&event_root, &validation, &moved_in),
            super::WatchImpact::WholeWorkspace,
            "a directory the publication holds nothing under is still one on disk, and \
             what moved in with it is unknown"
        );
        Ok(())
    }

    #[test]
    fn a_name_event_on_an_extensionless_file_the_publication_holds_names_that_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("LICENSE"), "beacon\n")?;
        let event_root = directory.path().canonicalize()?;
        let (validation, _current) = installed_publication(directory.path())?;
        let renamed = name_event(&event_root, "LICENSE");

        assert_eq!(
            super::watch_event_impact(&event_root, &validation, &renamed),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new("LICENSE")?]),
            "a file the publication holds is read again by itself, whatever its name"
        );
        Ok(())
    }

    /// Waits under a bound until the pending work names `path`, or escalated to the
    /// whole workspace, whichever the watcher's events produce first.
    async fn path_pending_within_bound(
        validation: &IndexValidation,
        path: &rift_core::ProjectPath,
    ) -> bool {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            let observed = {
                let pending = validation.locked_pending();
                pending.covers_whole_workspace() || pending.paths().any(|held| held == path)
            };
            if observed {
                return true;
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        false
    }

    #[tokio::test]
    async fn a_change_the_publisher_lands_keeps_the_next_batch_incremental() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let current = stable_candidate(directory.path(), 0)?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation.install_publication(&current);
        let _watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;
        let patch = "--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1 @@\n+staged through the publisher\n";
        let params = PatchParams {
            patch: patch.into(),
        };
        let landed = ChangeService::new(directory.path()).patch(&current.reads, &params)?;
        assert!(
            matches!(landed, ChangeResult::Applied { .. }),
            "the patch must land: {landed:?}"
        );
        let notes = rift_core::ProjectPath::new("notes.md")?;
        assert!(
            path_pending_within_bound(&validation, &notes).await,
            "the watcher must report the landed write"
        );

        let mut request = validation.take_pending();
        assert!(
            !request.work.covers_whole_workspace(),
            "a landed write names its own paths, the staged file's included: {:?}",
            request.work
        );
        request.previous = Some(Arc::clone(&current));
        let change_set = request.change_set(directory.path(), &current.configuration);
        let ChangeSet::Incremental(changes) = change_set else {
            return Err("a landed write resolves to an incremental change set".into());
        };
        assert!(
            changes.paths().any(|path| path == &notes),
            "the change set names the written file"
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
        let outcome = super::capture_rebuild_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            request,
            super::workspace_capture(&empty_dependency_store()),
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

        let limits = WorkspaceIndexLimits::default();
        let store = empty_dependency_store();
        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), limits, &request, &store)?
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

        let limits = WorkspaceIndexLimits::default();
        let store = empty_dependency_store();
        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), limits, &request, &store)?
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
    fn non_index_configuration_changes_reuse_the_published_tree() -> TestResult {
        let configurations = [
            "[execution]\nmax_code = \"1kb\"\n",
            "[lsp.rust]\ncommand = \"rust-analyzer\"\n",
            "[[hooks]]\nid = \"check\"\nkind = \"build\"\ncommand = [\"cargo\", \"check\"]\nchanged_paths = \"none\"\nwrites = \"none\"\nfailure_severity = \"error\"\ndeterminism = \"deterministic\"\n",
        ];
        for configuration in configurations {
            let directory = tempfile::tempdir()?;
            fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
            let previous = stable_candidate(directory.path(), 0)?;
            fs::write(directory.path().join("rift.toml"), configuration)?;
            let request = RebuildRequest {
                epoch: 1,
                work: super::PendingWork::naming([rift_core::ProjectPath::new("rift.toml")?]),
                previous: Some(Arc::clone(&previous)),
            };

            let WorkspaceCandidate::Stable {
                published: candidate,
                change_set,
            } = build_workspace_candidate(
                directory.path(),
                WorkspaceIndexLimits::default(),
                &request,
                &empty_dependency_store(),
            )?
            else {
                return Err("a stable configuration change must build a candidate".into());
            };
            assert!(
                matches!(change_set, ChangeSet::Incremental(ref paths) if paths.is_empty()),
                "non-index configuration must produce an empty incremental change: {configuration}"
            );
            assert!(
                Arc::ptr_eq(&previous.reads, &candidate.reads),
                "non-index configuration must reuse the published tree: {configuration}"
            );
            assert_eq!(
                previous.reads.tree_revision(),
                candidate.reads.tree_revision(),
                "non-index configuration must retain the tree revision: {configuration}"
            );
        }
        Ok(())
    }

    #[test]
    fn index_configuration_changes_rebuild_the_published_tree() -> TestResult {
        let configurations = [
            "[source]\nexclude = [\"lib.rs\"]\n",
            "[languages.rust]\ninclude = []\n",
            "[search.text]\ninclude = [\"**/*.log\"]\n",
        ];
        for configuration in configurations {
            let directory = tempfile::tempdir()?;
            fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
            fs::write(directory.path().join("notes.log"), "text marker\n")?;
            let previous = stable_candidate(directory.path(), 0)?;
            fs::write(directory.path().join("rift.toml"), configuration)?;
            let request = RebuildRequest {
                epoch: 1,
                work: super::PendingWork::naming([rift_core::ProjectPath::new("rift.toml")?]),
                previous: Some(Arc::clone(&previous)),
            };

            let WorkspaceCandidate::Stable {
                published: candidate,
                change_set,
            } = build_workspace_candidate(
                directory.path(),
                WorkspaceIndexLimits::default(),
                &request,
                &empty_dependency_store(),
            )?
            else {
                return Err("a stable configuration change must build a candidate".into());
            };
            assert!(
                matches!(change_set, ChangeSet::Full),
                "index configuration must produce a full change: {configuration}"
            );
            assert!(
                !Arc::ptr_eq(&previous.reads, &candidate.reads),
                "index configuration must replace the published tree: {configuration}"
            );
            assert_ne!(
                previous.reads.tree_revision(),
                candidate.reads.tree_revision(),
                "index configuration must change this fixture's tree revision: {configuration}"
            );
        }
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

    /// Every fingerprint answers one eight-character revision, and the three
    /// states answer different ones: an absent file, an oversized one, and one
    /// whose bytes were read must never share a revision.
    #[test]
    fn every_configuration_fingerprint_answers_its_own_wire_revision() {
        let content = ConfigurationFingerprint::Content([7_u8; 32]);
        let revisions = [
            content.wire_revision(),
            ConfigurationFingerprint::MissingOrUnreadable.wire_revision(),
            ConfigurationFingerprint::Oversized(1 << 20).wire_revision(),
        ];
        for revision in &revisions {
            assert_eq!(revision.len(), 8, "{revision}");
            assert!(
                revision
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            );
        }
        let unique: std::collections::BTreeSet<&String> = revisions.iter().collect();
        assert_eq!(unique.len(), revisions.len(), "{revisions:?}");
        assert_ne!(
            ConfigurationFingerprint::Oversized(1 << 20).wire_revision(),
            ConfigurationFingerprint::Oversized(1 << 21).wire_revision(),
            "two oversized files of different length answer different revisions"
        );
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let error = supervisor
            .shutdown(deadline)
            .await
            .expect_err("a stuck supervisor must miss the shutdown deadline");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        supervisor
            .shutdown(tokio::time::Instant::now() + Duration::from_secs(30))
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
        let outcome = super::capture_rebuild_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            RebuildRequest::initial(7),
            super::workspace_capture(&empty_dependency_store()),
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
        let dependencies = empty_dependency_store();
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
                dependencies: Arc::clone(&dependencies),
                dependency_lane: DependencyLane::spawn_isolated(&dependencies),
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
        let dependencies = empty_dependency_store();
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
                dependencies: Arc::clone(&dependencies),
                dependency_lane: DependencyLane::spawn_isolated(&dependencies),
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

    /// An epoch that moves while every scan folds to the same tree is an observation that
    /// changed no byte, so the startup build publishes instead of spending its bound.
    #[tokio::test]
    async fn initial_capture_publishes_when_every_scan_folds_the_same_tree() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let blocking = crate::server::BlockingExecutor::isolated(2, 60_000);
        let moving = Arc::clone(&validation);
        let (published, _write) = super::initial_workspace_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &validation,
            &blocking,
            move |root, limits, request| {
                // Every capture observes one more filesystem event, so no attempt ever
                // sees a stable epoch, and none of those events changes a byte.
                moving.observe_whole_workspace()?;
                super::build_workspace_candidate(root, limits, request, &empty_dependency_store())
            },
        )
        .await?;
        assert_eq!(
            published.epoch,
            validation.observed_epoch(),
            "the publication answers the epoch the observations reached"
        );
        Ok(())
    }

    /// A workspace whose every scan folds a different tree is genuinely moving, so the
    /// bounded attempts still refuse rather than publish a scan nothing confirmed.
    #[tokio::test]
    async fn initial_capture_fails_when_every_scan_folds_a_different_tree() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let blocking = crate::server::BlockingExecutor::isolated(2, 60_000);
        let moving = Arc::clone(&validation);
        let written = directory.path().to_path_buf();
        let error = super::initial_workspace_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &validation,
            &blocking,
            move |root, limits, request| {
                let candidate = super::build_workspace_candidate(
                    root,
                    limits,
                    request,
                    &empty_dependency_store(),
                )?;
                // The next scan reads bytes this one never held.
                fs::write(
                    written.join("lib.rs"),
                    format!("pub fn beacon{}() {{}}\n", request.epoch),
                )
                .expect("the fixture write lands");
                moving.observe_whole_workspace()?;
                Ok(candidate)
            },
        )
        .await
        .expect_err("a tree that moves on every attempt must exhaust bounded attempts");
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
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        committed_through(
            &lane,
            &index,
            super::lexical_write(&published, &ChangeSet::Full),
            &published,
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
        let dependencies = empty_dependency_store();
        let context = super::IndexSupervisorContext {
            root: root.to_path_buf(),
            limits: WorkspaceIndexLimits::default(),
            published: Arc::clone(state),
            change_lane: Arc::new(crate::server::ChangeLane::default()),
            validation: Arc::clone(validation),
            blocking: BlockingExecutor::for_configuration(&ServerConfiguration::default()),
            population: None,
            lexical,
            dependencies: Arc::clone(&dependencies),
            dependency_lane: DependencyLane::spawn_isolated(&dependencies),
        };
        let capture = super::workspace_capture(&dependencies);
        super::rebuild_workspace(&context, request, capture).await
    }

    #[tokio::test]
    async fn a_whole_commit_stamps_the_published_revision_and_leaves_units_searchable() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );

        let write = super::lexical_write(&published, &ChangeSet::Full);
        committed_through(&lane, &index, write, &published).await?;

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

    /// A search index whose lexical tier accepts at most `unit_bytes_max` content bytes
    /// per unit, for the left-out cases.
    async fn search_index_bounded(
        database: &std::path::Path,
        unit_bytes_max: u32,
    ) -> TestResult<SearchIndex> {
        let defaults = LexicalIndexLimits::default();
        let lexical = LexicalIndexLimits::new(
            defaults.units_max(),
            unit_bytes_max,
            defaults.query_terms_max(),
            defaults.matches_max(),
            defaults.pool_slots(),
            defaults.busy_timeout_ms(),
        );
        let limits = SearchIndexLimits::builder(lexical)
            .disable_semantic()
            .build();
        Ok(SearchIndex::open(database, limits).await?)
    }

    /// One unit `bytes` long at `path`, for the bound cases.
    fn unit_of(path: &str, identity: &str, bytes: usize) -> TestResult<rift_index::LexicalUnit> {
        let project_path = rift_core::ProjectPath::new(path)?;
        let kind = rift_index::LexicalUnitKind::Symbol;
        let name = Some(identity.to_owned());
        let text = "x".repeat(bytes);
        let unit = rift_index::LexicalUnit::new(identity, project_path, kind, name, text)?;
        Ok(unit)
    }

    #[test]
    fn a_whole_write_leaves_out_every_unit_over_the_bound() -> TestResult {
        let write = super::LexicalWrite::Whole(vec![
            unit_of("a.rs", "small", 8)?,
            unit_of("b.rs", "large", 9)?,
            unit_of("c.rs", "exact", 8)?,
        ]);

        let (kept, left_out) = write.within_unit_bound(8);

        let super::LexicalWrite::Whole(kept) = kept else {
            return Err("a whole write stays whole".into());
        };
        assert_eq!(
            kept.iter()
                .map(rift_index::LexicalUnit::identity)
                .collect::<Vec<_>>(),
            ["small", "exact"]
        );
        assert_eq!(left_out.len(), 1);
        assert_eq!(left_out[0].path().as_str(), "b.rs");
        Ok(())
    }

    #[test]
    fn a_change_write_keeps_its_replaced_paths_while_leaving_out_the_unit() -> TestResult {
        let path = rift_core::ProjectPath::new("grown.rs")?;
        let write = super::LexicalWrite::Change(LexicalChange::new(
            vec![path.clone()],
            vec![unit_of("grown.rs", "grown", 9)?],
        ));

        let (kept, left_out) = write.within_unit_bound(8);

        let super::LexicalWrite::Change(change) = kept else {
            return Err("a change write stays a change".into());
        };
        assert_eq!(change.replaced(), [path]);
        assert!(
            change.inserted().is_empty(),
            "the grown unit is left out: {:?}",
            change.inserted()
        );
        assert!(
            !change.is_empty(),
            "the stored units of the grown file are still deleted"
        );
        assert_eq!(left_out.len(), 1);
        Ok(())
    }

    /// The lane commits the rest of the set when one declaration exceeds the store's
    /// unit bound, stamps the revision, and records the unit it left out. The store's own
    /// refusal is what this replaces: `lexical.rs` still refuses such a unit handed to it
    /// directly.
    #[tokio::test]
    async fn a_commit_leaves_out_an_oversized_unit_records_it_and_publishes_the_rest() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let blob = format!("pub const BLOB: &str = \"{}\";\n", "b".repeat(96));
        fs::write(directory.path().join("blob.rs"), blob)?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index_bounded(&directory.path().join("search.db"), 64).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        committed_through(
            &lane,
            &index,
            super::lexical_write(&published, &ChangeSet::Full),
            &published,
        )
        .await?;

        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(published.reads.tree_revision()),
            "the commit publishes the rest of the set under the tree revision"
        );
        let ranked = ranked_at(&index, published.reads.tree_revision(), "beacon", 8).await?;
        assert!(
            !ranked.is_empty(),
            "the sibling declaration stays searchable"
        );
        let blob = ranked_at(&index, published.reads.tree_revision(), "BLOB", 8).await?;
        assert!(blob.is_empty(), "the oversized unit is absent: {blob:?}");
        let recorded = queued_records(&mut drain);
        let left_out = recorded
            .iter()
            .find(|record| record.message().contains("lexical unit left out"))
            .ok_or("the left-out unit is recorded")?;
        assert_eq!(left_out.level(), "warn");
        assert_eq!(left_out.component(), "index");
        assert!(
            left_out.fields().contains("blob.rs")
                && left_out.fields().contains("\"maximum\":\"64\""),
            "the record names the path and the bound: {}",
            left_out.fields()
        );
        cancellation.cancel();
        Ok(())
    }

    /// Drains what the queue currently holds, without a store.
    fn queued_records(drain: &mut crate::logs::LogDrain) -> Vec<rift_index::LogRecord> {
        let mut records = Vec::new();
        while let Ok(record) = drain.try_recv_record() {
            records.push(record);
        }
        records
    }

    #[tokio::test]
    async fn a_change_commit_replaces_one_path_and_keeps_every_other_unit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("kept.rs"), "pub fn keptalpha() {}\n")?;
        fs::write(directory.path().join("moved.rs"), "pub fn firstbeta() {}\n")?;
        let first = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        committed_through(
            &lane,
            &index,
            super::lexical_write(&first, &ChangeSet::Full),
            &first,
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
        let limits = WorkspaceIndexLimits::default();
        let store = empty_dependency_store();
        let WorkspaceCandidate::Stable {
            published: second,
            change_set,
        } = build_workspace_candidate(directory.path(), limits, &request, &store)?
        else {
            return Err("a stable fixture must build a stable candidate".into());
        };
        committed_through(
            &lane,
            &index,
            super::lexical_write(&second, &change_set),
            &second,
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

    /// Hands `write` to `lane` for `published`, and waits under a bound until the store
    /// is stamped with that publication's tree revision.
    async fn committed_through(
        lane: &LexicalLane,
        index: &SearchIndex,
        write: LexicalWrite,
        published: &Arc<PublishedWorkspace>,
    ) -> TestResult {
        lane.request(write, Arc::clone(published));
        stamped_within_bound(index, published.reads.tree_revision()).await
    }

    /// Waits under a bound until `index` is stamped with `tree_revision`.
    async fn stamped_within_bound(index: &SearchIndex, tree_revision: &str) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if index.tree_revision().await?.as_deref() == Some(tree_revision) {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err(format!("the lane never stamped {tree_revision}").into())
    }

    /// Waits under a bound until `lane` reports `tree_revision` as `state`.
    async fn commit_state_within_bound(
        lane: &LexicalLane,
        tree_revision: &str,
        state: LexicalCommitState,
    ) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.commit_state(tree_revision) == state {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err(format!(
            "the lane never reported {tree_revision} as {state:?}: {:?}",
            lane.commit_state(tree_revision)
        )
        .into())
    }

    /// Waits under a bound until `lane` reports `tree_revision` as owed, and answers the
    /// cause it is owed for.
    async fn owed_within_bound(lane: &LexicalLane, tree_revision: &str) -> TestResult<String> {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if let LexicalCommitState::Owed { cause } = lane.commit_state(tree_revision) {
                return Ok(cause);
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err(format!(
            "the lane never reported {tree_revision} as owed: {:?}",
            lane.commit_state(tree_revision)
        )
        .into())
    }

    /// Waits under a bound until `lane`'s task has ended.
    async fn ended_within_bound(lane: &LexicalLane) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if lane.has_ended() {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err("the lane never ended".into())
    }

    /// The candidate `lib.rs` builds to at `epoch` once it declares `declaration`.
    fn candidate_declaring(
        root: &std::path::Path,
        epoch: u64,
        declaration: &str,
    ) -> TestResult<Arc<PublishedWorkspace>> {
        fs::write(
            root.join("lib.rs"),
            format!("pub fn {declaration}() {{}}\n"),
        )?;
        stable_candidate(root, epoch)
    }

    /// One change write naming `lib.rs`, for a lane that never reads the store's rows.
    fn change_naming_lib() -> TestResult<LexicalWrite> {
        Ok(LexicalWrite::Change(LexicalChange::new(
            vec![rift_core::ProjectPath::new("lib.rs")?],
            vec![unit_of("lib.rs", "rift://symbol/rust/lib.rs/beacon", 8)?],
        )))
    }

    #[test]
    fn commit_deadline_is_floored_derived_and_capped() {
        assert_eq!(commit_deadline(0), LEXICAL_COMMIT_TIMEOUT);
        assert_eq!(commit_deadline(30_000), LEXICAL_COMMIT_TIMEOUT);
        assert_eq!(
            commit_deadline(30_001),
            LEXICAL_COMMIT_TIMEOUT + LEXICAL_UNIT_COMMIT_BUDGET
        );
        assert_eq!(commit_deadline(400_000), Duration::from_secs(400));
        assert_eq!(commit_deadline(600_000), LEXICAL_COMMIT_TIMEOUT_MAX);
        assert_eq!(commit_deadline(600_001), LEXICAL_COMMIT_TIMEOUT_MAX);
        assert_eq!(commit_deadline(usize::MAX), LEXICAL_COMMIT_TIMEOUT_MAX);
    }

    #[test]
    fn a_backlog_supersedes_at_capacity_and_keeps_the_newest_while_a_whole_is_owed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut publications = Vec::new();
        for epoch in 0..=LEXICAL_COMMITS_MAX as u64 + 1 {
            publications.push(candidate_declaring(
                directory.path(),
                epoch,
                &format!("declared{epoch}"),
            )?);
        }
        let commit = |epoch: usize| -> TestResult<super::LexicalCommit> {
            Ok(super::LexicalCommit {
                write: change_naming_lib()?,
                published: Arc::clone(&publications[epoch]),
            })
        };
        let mut backlog = super::LexicalBacklog::default();
        for epoch in 0..LEXICAL_COMMITS_MAX {
            assert_eq!(backlog.hand(commit(epoch)?), 0, "the lane has room");
        }
        assert_eq!(backlog.held.len(), LEXICAL_COMMITS_MAX);
        assert!(backlog.whole_owed.is_none());
        assert_eq!(
            backlog.state_of(publications[1].reads.tree_revision()),
            LexicalCommitState::Committing
        );

        assert_eq!(
            backlog.hand(commit(LEXICAL_COMMITS_MAX)?),
            LEXICAL_COMMITS_MAX,
            "a write handed to a full lane supersedes every held one"
        );
        assert!(backlog.whole_owed.is_some());
        assert_eq!(backlog.held.len(), 1, "only the newest write is held");
        assert!(matches!(
            backlog.state_of(publications[1].reads.tree_revision()),
            LexicalCommitState::Owed { .. }
        ));
        assert_eq!(
            backlog.state_of(publications[LEXICAL_COMMITS_MAX].reads.tree_revision()),
            LexicalCommitState::Committing
        );

        let (taken, whole_owed) = backlog.take_next().ok_or("the newest write is held")?;
        assert!(
            whole_owed,
            "the owed whole replace becomes the transaction's"
        );
        assert!(backlog.whole_owed.is_none());
        assert_eq!(
            backlog.state_of(taken.tree_revision()),
            LexicalCommitState::Committing,
            "a running transaction is still committing"
        );
        assert_eq!(backlog.hand(commit(LEXICAL_COMMITS_MAX + 1)?), 0);
        backlog.owe_whole("the store refused".to_owned());
        assert_eq!(
            backlog.held.len(),
            1,
            "a failed transaction keeps only the newest held write"
        );
        backlog.running = None;
        assert_eq!(
            backlog.state_of(taken.tree_revision()),
            LexicalCommitState::Owed {
                cause: "the store refused".to_owned()
            }
        );
        assert!(backlog.take_next().is_some());
        assert_eq!(
            backlog.state_of(taken.tree_revision()),
            LexicalCommitState::Settled,
            "nothing held, nothing running, nothing owed"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_publishes_while_the_lane_still_holds_its_transaction() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        let state = Arc::new(RwLock::new(IndexState {
            current: Arc::clone(&first),
            failure: None,
        }));
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let double = StoreDouble::new();
        double.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        double.release_one();
        committed_through(
            &lane,
            &index,
            super::lexical_write(&first, &ChangeSet::Full),
            &first,
        )
        .await?;

        fs::write(directory.path().join("lib.rs"), "pub fn secondgamma() {}\n")?;
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let outcome =
            rebuilt_through(directory.path(), &state, &validation, Some(lane.clone())).await?;

        assert_eq!(
            outcome,
            RebuildOutcome::Published,
            "the rebuild publishes without waiting for the transaction"
        );
        let current = Arc::clone(&state.read().await.current);
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(first.reads.tree_revision()),
            "the store still holds the previous tree while the transaction is held"
        );
        commit_state_within_bound(
            &lane,
            current.reads.tree_revision(),
            LexicalCommitState::Committing,
        )
        .await?;

        double.release_one();
        stamped_within_bound(&index, current.reads.tree_revision()).await?;
        assert!(
            !ranked_at(&index, current.reads.tree_revision(), "secondgamma", 8)
                .await?
                .is_empty(),
            "the published tree's units are searchable once the transaction lands"
        );
        commit_state_within_bound(
            &lane,
            current.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_failing_commit_leads_to_a_whole_replace_on_the_next_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        let second = candidate_declaring(directory.path(), 1, "secondgamma")?;
        let third = candidate_declaring(directory.path(), 2, "thirddelta")?;
        let double = StoreDouble::new();
        double.refuse_changes();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        double.release_one();
        lane.request(
            super::lexical_write(&first, &ChangeSet::Full),
            Arc::clone(&first),
        );
        double.calls_within_bound(1).await?;
        commit_state_within_bound(
            &lane,
            first.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;

        double.release_one();
        lane.request(change_naming_lib()?, Arc::clone(&second));
        let cause = owed_within_bound(&lane, second.reads.tree_revision()).await?;
        assert!(
            cause.contains("the double refuses changes"),
            "the owed cause is the store's own: {cause}"
        );
        assert!(
            lane.owes_whole(),
            "a failed change leaves a whole replace owed"
        );
        let recorded = queued_records(&mut drain);
        let failure = recorded
            .iter()
            .find(|record| record.message().contains("the lexical commit failed"))
            .ok_or("the failed commit must be recorded")?;
        assert_eq!(failure.level(), "error");
        assert!(
            failure.fields().contains("the double refuses changes"),
            "the record carries the store's own cause: {}",
            failure.fields()
        );

        double.release_one();
        lane.request(change_naming_lib()?, Arc::clone(&third));
        let calls = double.calls_within_bound(3).await?;
        assert_eq!(
            calls,
            vec![
                ("replace", first.reads.tree_revision().to_owned()),
                ("apply", second.reads.tree_revision().to_owned()),
                ("replace", third.reads.tree_revision().to_owned()),
            ],
            "the publication after a failed change commits the whole set"
        );
        commit_state_within_bound(
            &lane,
            third.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        assert!(
            !lane.owes_whole(),
            "a landed whole replace pays what was owed"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_transaction_past_its_deadline_is_recorded_and_the_lane_keeps_waiting() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        let write = super::lexical_write(&published, &ChangeSet::Full);
        let deadline = commit_deadline(write.unit_count());
        assert_eq!(
            deadline, LEXICAL_COMMIT_TIMEOUT,
            "a small set gets the floor"
        );
        lane.request(write, Arc::clone(&published));
        double.calls_within_bound(1).await?;

        tokio::time::sleep(deadline + Duration::from_millis(1)).await;
        let recorded = queued_records(&mut drain);
        let delay = recorded
            .iter()
            .find(|record| record.message().contains("ran past its deadline"))
            .ok_or("the delay must be recorded once the deadline passes")?;
        assert_eq!(delay.level(), "error");
        assert_eq!(
            lane.commit_state(published.reads.tree_revision()),
            LexicalCommitState::Committing,
            "the lane keeps waiting for the transaction past its deadline"
        );
        assert!(
            !lane.owes_whole(),
            "a delayed transaction is not yet a missed one"
        );

        double.release_one();
        commit_state_within_bound(
            &lane,
            published.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        assert!(
            !lane.owes_whole(),
            "a transaction that ended well after its deadline owes nothing"
        );
        cancellation.cancel();
        Ok(())
    }

    /// A lane cancelled while its transaction waits at the store's gate aborts that
    /// transaction: the store sees the write's future dropped, and the lane ends within a
    /// bound far under `LEXICAL_COMMIT_TIMEOUT` instead of after the transaction.
    #[tokio::test]
    async fn a_cancelled_lane_aborts_the_transaction_it_was_running() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        double.calls_within_bound(1).await?;
        assert_eq!(
            double.dropped_while_held(),
            0,
            "the held write's future lives while the lane runs"
        );

        cancellation.cancel();
        tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT / 10, ended_within_bound(&lane))
            .await
            .map_err(|_| "the lane must end far under the commit timeout")??;
        assert_eq!(
            double.dropped_while_held(),
            1,
            "the abort dropped the transaction the store still held"
        );
        let LexicalCommitState::Owed { cause } = lane.commit_state(published.reads.tree_revision())
        else {
            return Err("an aborted transaction leaves a whole replace owed".into());
        };
        assert!(
            cause.contains("the lexical lane ended while the transaction ran"),
            "the owed cause names the cancellation: {cause}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_write_handed_to_a_full_lane_supersedes_the_held_ones() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut publications = Vec::new();
        for epoch in 0..=LEXICAL_COMMITS_MAX as u64 + 1 {
            publications.push(candidate_declaring(
                directory.path(),
                epoch,
                &format!("declared{epoch}"),
            )?);
        }
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        // The first write runs and holds the lane at the store's gate.
        lane.request(
            super::lexical_write(&publications[0], &ChangeSet::Full),
            Arc::clone(&publications[0]),
        );
        double.calls_within_bound(1).await?;
        for publication in &publications[1..=LEXICAL_COMMITS_MAX] {
            lane.request(change_naming_lib()?, Arc::clone(publication));
        }
        let newest = &publications[LEXICAL_COMMITS_MAX + 1];
        lane.request(change_naming_lib()?, Arc::clone(newest));
        assert!(
            lane.owes_whole(),
            "superseding held writes owes a whole replace"
        );
        assert_eq!(
            lane.commit_state(publications[2].reads.tree_revision()),
            LexicalCommitState::Owed {
                cause: super::SUPERSEDED_CAUSE.to_owned()
            },
            "a superseded publication is owed, never committing"
        );
        assert_eq!(
            lane.commit_state(newest.reads.tree_revision()),
            LexicalCommitState::Committing
        );
        let recorded = queued_records(&mut drain);
        let superseded = recorded
            .iter()
            .find(|record| record.message().contains("superseded"))
            .ok_or("the supersession must be recorded")?;
        assert!(
            superseded.fields().contains("\"superseded\":\"4\""),
            "the record counts the dropped writes: {}",
            superseded.fields()
        );

        double.release_one();
        double.release_one();
        let calls = double.calls_within_bound(2).await?;
        assert_eq!(
            calls,
            vec![
                ("replace", publications[0].reads.tree_revision().to_owned()),
                ("replace", newest.reads.tree_revision().to_owned()),
            ],
            "the superseded writes never reach the store, and the newest commits whole"
        );
        commit_state_within_bound(
            &lane,
            newest.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        cancellation.cancel();
        Ok(())
    }

    /// A waiter subscribed before a transaction fails is woken by its end, and reads the
    /// revision as owed.
    #[tokio::test]
    async fn a_failed_transaction_wakes_the_waiters_on_the_landing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let double = StoreDouble::new();
        double.refuse_changes();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        lane.request(change_naming_lib()?, Arc::clone(&published));
        double.calls_within_bound(1).await?;
        let landed = lane.landed();
        assert_eq!(
            lane.commit_state(published.reads.tree_revision()),
            LexicalCommitState::Committing,
            "the transaction is held at the store's gate"
        );

        double.release_one();
        tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT / 10, landed)
            .await
            .map_err(|_| "a failed transaction's end wakes the waiters")?;
        assert!(
            matches!(
                lane.commit_state(published.reads.tree_revision()),
                LexicalCommitState::Owed { .. }
            ),
            "the woken waiter reads the refused transaction as owed"
        );
        cancellation.cancel();
        Ok(())
    }

    /// A write handed to a full lane wakes the waiters: the superseded revisions are owed
    /// from then on, and a waiter reads that instead of waiting on a commit that never
    /// runs.
    #[tokio::test]
    async fn a_superseding_write_wakes_the_waiters_on_the_landing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut publications = Vec::new();
        for epoch in 0..=LEXICAL_COMMITS_MAX as u64 + 1 {
            publications.push(candidate_declaring(
                directory.path(),
                epoch,
                &format!("declared{epoch}"),
            )?);
        }
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            usize::MAX,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        lane.request(
            super::lexical_write(&publications[0], &ChangeSet::Full),
            Arc::clone(&publications[0]),
        );
        double.calls_within_bound(1).await?;
        for publication in &publications[1..=LEXICAL_COMMITS_MAX] {
            lane.request(change_naming_lib()?, Arc::clone(publication));
        }
        let held = publications[LEXICAL_COMMITS_MAX].reads.tree_revision();
        let landed = lane.landed();
        assert_eq!(
            lane.commit_state(held),
            LexicalCommitState::Committing,
            "the last held write is still committing"
        );

        let newest = &publications[LEXICAL_COMMITS_MAX + 1];
        lane.request(change_naming_lib()?, Arc::clone(newest));
        tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT / 10, landed)
            .await
            .map_err(|_| "a superseding hand-off wakes the waiters")?;
        assert_eq!(
            lane.commit_state(held),
            LexicalCommitState::Owed {
                cause: super::SUPERSEDED_CAUSE.to_owned()
            },
            "the woken waiter reads the superseded revision as owed"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_published_rebuild_hands_its_write_to_the_lane() -> TestResult {
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
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        committed_through(
            &lane,
            &index,
            super::lexical_write(&first, &ChangeSet::Full),
            &first,
        )
        .await?;

        fs::write(directory.path().join("lib.rs"), "pub fn secondgamma() {}\n")?;
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let outcome =
            rebuilt_through(directory.path(), &state, &validation, Some(lane.clone())).await?;

        assert_eq!(outcome, RebuildOutcome::Published);
        let current = Arc::clone(&state.read().await.current);
        stamped_within_bound(&index, current.reads.tree_revision()).await?;
        assert!(
            !ranked_at(&index, current.reads.tree_revision(), "secondgamma", 8)
                .await?
                .is_empty(),
            "the published tree's units are searchable once the lane's transaction lands"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_publishes_when_the_lexical_lane_has_ended() -> TestResult {
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
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        committed_through(
            &lane,
            &index,
            super::lexical_write(&first, &ChangeSet::Full),
            &first,
        )
        .await?;

        // The lane ends, so the next write has no one to run it.
        cancellation.cancel();
        ended_within_bound(&lane).await?;

        fs::write(directory.path().join("lib.rs"), "pub fn secondgamma() {}\n")?;
        validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let outcome = rebuilt_through(directory.path(), &state, &validation, Some(lane)).await?;
        assert_eq!(
            outcome,
            RebuildOutcome::Published,
            "the publication never waits on the lane"
        );
        assert!(
            !Arc::ptr_eq(&state.read().await.current, &first),
            "the new publication is current"
        );
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(first.reads.tree_revision()),
            "the previously stamped revision stays intact: the write was dropped"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_change_write_is_not_held() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );

        let empty =
            super::lexical_write(&published, &ChangeSet::Incremental(PathChanges::default()));
        assert!(
            empty.is_empty(),
            "a change set naming nothing writes nothing"
        );
        lane.request(empty, Arc::clone(&published));
        assert_eq!(
            lane.commit_state(published.reads.tree_revision()),
            LexicalCommitState::Settled,
            "a write that changes nothing is not held"
        );
        assert_eq!(
            index.tree_revision().await?,
            None,
            "a write that changes nothing opens no transaction"
        );
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_write_handed_after_the_lane_ended_is_dropped() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
        );
        cancellation.cancel();
        ended_within_bound(&lane).await?;

        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        assert_eq!(
            lane.commit_state(published.reads.tree_revision()),
            LexicalCommitState::Settled,
            "an ended lane holds nothing"
        );
        assert_eq!(
            index.tree_revision().await?,
            None,
            "a dropped write leaves the store as it was"
        );
        Ok(())
    }

    /// Waits until the supervisor takes pending work for its next rebuild.
    async fn pending_work_taken_within_bound(validation: &IndexValidation) -> bool {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            let taken = {
                let pending = validation.locked_pending();
                !pending.covers_whole_workspace() && pending.paths().next().is_none()
            };
            if taken {
                return true;
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        false
    }

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
            RevisionScoped::Matched(ranked) => Ok(ranked.into_units()),
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

        let dependencies = empty_dependency_store();
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
                dependencies: Arc::clone(&dependencies),
                dependency_lane: DependencyLane::spawn_isolated(&dependencies),
            },
        ));

        let first_epoch = validation
            .observe_whole_workspace()
            .map_err(|error| format!("first observation must land: {error:?}"))?;
        assert_eq!(first_epoch, 1);
        assert!(
            pending_work_taken_within_bound(&validation).await,
            "the supervisor must take epoch 1 before the test moves the epoch"
        );

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

    /// The supervisor context the cancellation tests drive, over `blocking` sized to one
    /// slot so a sentinel run through it proves the held capture's thread has ended.
    fn cancellation_context(
        root: &std::path::Path,
        validation: &Arc<IndexValidation>,
        published: &Arc<RwLock<IndexState>>,
        blocking: &BlockingExecutor,
        dependencies: &Arc<super::DependencyStore>,
    ) -> super::IndexSupervisorContext {
        super::IndexSupervisorContext {
            root: root.to_path_buf(),
            limits: WorkspaceIndexLimits::default(),
            published: Arc::clone(published),
            change_lane: Arc::new(crate::server::ChangeLane::default()),
            validation: Arc::clone(validation),
            blocking: blocking.clone(),
            population: None,
            lexical: None,
            dependencies: Arc::clone(dependencies),
            dependency_lane: DependencyLane::spawn_isolated(dependencies),
        }
    }

    /// A capture that reports it started, blocks until the test releases it, and then
    /// scans the workspace as production would.
    ///
    /// The release is a rendezvous: it succeeds only while the capture still waits in
    /// it, so a successful release proves the capture was blocking at that moment.
    fn held_capture(
        dependencies: &Arc<super::DependencyStore>,
    ) -> (
        impl super::CaptureWorkspace + Clone + Send + 'static,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (started, started_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_receiver) = std::sync::mpsc::sync_channel::<()>(0);
        let release_receiver = Arc::new(std::sync::Mutex::new(release_receiver));
        let scan = super::workspace_capture(dependencies);
        let capture = move |root: &std::path::Path,
                            limits: WorkspaceIndexLimits,
                            request: &RebuildRequest| {
            let _ = started.send(());
            release_receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv()
                .expect("the test releases the held capture");
            scan(root, limits, request)
        };
        (capture, started_receiver, release)
    }

    #[tokio::test]
    async fn rebuild_answers_cancelled_while_its_capture_still_blocks() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let published = Arc::new(RwLock::new(IndexState {
            current: stable_candidate(directory.path(), 0)?,
            failure: None,
        }));
        let blocking = BlockingExecutor::isolated(1, 60_000);
        let dependencies = empty_dependency_store();
        let context = cancellation_context(
            directory.path(),
            &validation,
            &published,
            &blocking,
            &dependencies,
        );
        let (capture, mut started, release) = held_capture(&dependencies);
        validation.observe_whole_workspace()?;
        let request = validation.take_pending();

        let cancel_once_started = async {
            started.recv().await;
            validation.cancellation.cancel();
        };
        let rebuild = tokio::time::timeout(
            Duration::from_secs(1),
            super::rebuild_workspace(&context, request, capture),
        );
        let (outcome, ()) = tokio::join!(rebuild, cancel_once_started);
        let outcome = outcome.map_err(
            |_| "a cancelled rebuild must answer within a second while its capture blocks",
        )??;
        assert_eq!(outcome, RebuildOutcome::Cancelled);
        release
            .send(())
            .map_err(|_| "the capture must still block when the rebuild answers")?;

        // The one slot is free again only once the detached capture thread has ended.
        blocking
            .run("sentinel", || Ok::<(), rift_server::ReadError>(()))
            .await?;
        let state = published.read().await;
        let (snapshot, failure) = state.snapshot();
        assert_eq!(snapshot.epoch, 0, "a cancelled rebuild publishes nothing");
        assert!(failure.is_none(), "a cancelled rebuild records no failure");
        drop(state);
        assert!(
            validation.locked_pending().covers_whole_workspace(),
            "the cancelled capture returns its work"
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_ends_when_cancelled_while_its_capture_still_blocks() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let published = Arc::new(RwLock::new(IndexState {
            current: stable_candidate(directory.path(), 0)?,
            failure: None,
        }));
        let watcher = super::workspace_watcher(directory.path(), &validation)
            .map_err(|error| format!("watcher must start: {error:?}"))?;
        let blocking = BlockingExecutor::isolated(1, 60_000);
        let dependencies = empty_dependency_store();
        let context = cancellation_context(
            directory.path(),
            &validation,
            &published,
            &blocking,
            &dependencies,
        );
        let (capture, mut started, release) = held_capture(&dependencies);
        let supervisor = tokio::spawn(super::run_index_supervisor_with(
            watcher,
            invalidations,
            context,
            capture,
        ));
        validation.observe_whole_workspace()?;
        started
            .recv()
            .await
            .ok_or("the supervisor must start the capture")?;
        validation.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .map_err(|_| "the supervisor must end within a second while its capture blocks")??;
        release
            .send(())
            .map_err(|_| "the capture must still block when the supervisor ends")?;

        blocking
            .run("sentinel", || Ok::<(), rift_server::ReadError>(()))
            .await?;
        let state = published.read().await;
        let (snapshot, failure) = state.snapshot();
        assert_eq!(snapshot.epoch, 0, "a cancelled rebuild publishes nothing");
        assert!(failure.is_none(), "a cancelled rebuild records no failure");
        Ok(())
    }

    #[test]
    fn capture_is_cancelled_before_it_runs_once_the_token_is_cancelled() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = RwLock::new(IndexState {
            current: stable_candidate(directory.path(), 0)?,
            failure: None,
        });
        validation.observe_whole_workspace()?;
        let request = validation.take_pending();
        validation.cancellation.cancel();
        let mut ran = false;
        let outcome = super::capture_rebuild_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            request,
            |_, _, _| {
                ran = true;
                Ok(WorkspaceCandidate::ConfigurationChanged)
            },
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Cancelled));
        assert!(!ran, "a cancelled attempt never runs its capture");
        assert!(
            validation.locked_pending().covers_whole_workspace(),
            "the cancelled attempt returns its work"
        );
        Ok(())
    }

    #[test]
    fn capture_is_cancelled_before_its_candidate_is_handed_on() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = RwLock::new(IndexState {
            current: stable_candidate(directory.path(), 0)?,
            failure: None,
        });
        validation.observe_whole_workspace()?;
        let request = validation.take_pending();
        let cancelling = Arc::clone(&validation);
        let outcome = super::capture_rebuild_with(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &state,
            &validation,
            request,
            move |root, limits, request| {
                cancelling.cancellation.cancel();
                build_workspace_candidate(root, limits, request, &empty_dependency_store())
            },
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Cancelled));
        assert!(
            validation.locked_pending().covers_whole_workspace(),
            "a candidate captured under cancellation returns its work"
        );
        Ok(())
    }

    #[test]
    fn publication_is_refused_once_the_token_is_cancelled() -> TestResult {
        let fixture = publication_fixture()?;
        fixture.validation.cancellation.cancel();
        assert_eq!(
            publish_rebuild(
                fixture.root(),
                &fixture.state,
                &fixture.validation,
                &fixture.after
            ),
            RebuildOutcome::Cancelled
        );
        let state = fixture.state.blocking_read();
        assert_workspace_identity(&state.current, &fixture.before);
        Ok(())
    }
}
