//! Current-index validation: filesystem observation, serialized rebuilds,
//! and atomic publication of the workspace snapshot.

mod background;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as SyncMutex, RwLock as SyncRwLock};
use std::time::Duration;

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use rift_core::ProjectPath;
use rift_core::constants::{
    VCS_IGNORE_FILE, WORKSPACE_CONFIGURATION_FILE, WORKSPACE_IGNORED_DIRECTORIES,
};
use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_dependency::DependencyContext;
use rift_index::{
    ChangeSet, FileRecord, LastCapture, LexicalChange, LexicalStamp, PathChanges, TrigramBatch,
    WorkspaceDigests, WorkspaceFingerprint, WorkspaceIndexError, WorkspaceIndexLimits,
    WorkspaceIndexPreparation, WorkspaceSourcePolicy, capture_digests_with_languages_cancellable,
    capture_selected_paths_cancellable,
};
use rift_protocol::configuration::{
    GlobalConfiguration, HistoryConfiguration, LanguageLspConfiguration, LogsConfiguration,
    LspConfiguration, SearchConfiguration, ServerConfiguration, SyntaxConfiguration,
    WorkspaceConfiguration,
};
use rift_protocol::dependencies::DependenciesConfiguration;
use rift_protocol::error as wire;
use rift_protocol::map::WorkspaceMap;
use rift_protocol::read::ReadWarning;
use rift_protocol::source::SourceConfiguration;
use rift_ranking::IndexDocument;
use rift_search::{Embedding, SearchError, SearchIndex, VectorReadiness};
use rift_server::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationError, LspProcessKey, ReadError, ReadFault,
    ReadService, ReadServiceBuild, load_configuration,
};
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};
use tokio::sync::futures::Notified;
use tokio::sync::{Mutex as AsyncMutex, Notify, RwLock, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::failure::WireFailure;
use crate::server::{BlockingExecutor, EngineHold};

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
    #[cfg(test)]
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
    /// Supervisor stop state, checked between files by index capture.
    pub(crate) cancellation: CancellationToken,
}

impl RebuildRequest {
    /// The rebuild startup runs: every visible file, with nothing to share.
    #[cfg(test)]
    pub(crate) fn initial(epoch: u64) -> Self {
        Self {
            epoch,
            work: PendingWork::whole_workspace(),
            previous: None,
            cancellation: CancellationToken::new(),
        }
    }

    /// Resolves this observation into the change set the candidate builds from.
    ///
    /// Changed source, language, or text-search configuration reads the whole workspace.
    /// Other accepted configuration carries forward the same source files under an empty
    /// incremental change. A path whose bytes cannot be read for any reason other than its
    /// absence, a directory, or the per-file byte bound asks for a whole scan, because that
    /// scan decides whether the path is a refusal or a removal.
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
        if previous.holds_files_below_a_gone_path(root, &self.work.paths) {
            return ChangeSet::Full;
        }
        let Some(source_policy) = previous.source_policy.as_deref() else {
            return ChangeSet::Full;
        };
        let Some(observed) = observed_records(root, &self.work.paths, source_policy) else {
            return ChangeSet::Full;
        };
        ChangeSet::Incremental(PathChanges::resolve(observed, |path| {
            previous.file_record(path)
        }))
    }
}

/// Reads each observed path's current bytes into the record one change set compares
/// against, or nothing when a read failed for a reason other than the path being gone, a
/// directory, or past the per-file byte bound.
///
/// A path the workspace's policy no longer includes reads as absent, so an excluded file
/// leaves the index exactly as a deleted one does.
fn observed_records(
    root: &Path,
    paths: &BTreeSet<ProjectPath>,
    policy: &WorkspaceSourcePolicy,
) -> Option<Vec<(ProjectPath, Option<FileRecord>)>> {
    let mut observed = Vec::with_capacity(paths.len());
    for path in paths {
        match observed_record(policy, root, path) {
            Ok(record) => observed.push((path.clone(), record)),
            Err(_) => return None,
        }
    }
    Some(observed)
}

/// One observed path's record under `policy`: the digest of its bytes, the warning of a
/// file past the per-file byte bound, or none for a path the policy excludes, one gone from
/// the disk, or a directory, because none of them holds a file.
///
/// A file past the bound is still on disk, and its warning is what a publication that left
/// it out records too, so the two compare equal while a deleted one compares as removed.
/// A directory reaches here as a path a modify event named, which Windows reports for a
/// directory whenever an entry inside it changes. Reading it as a file fails, and that
/// failure would otherwise ask for the whole workspace; the disk probe runs only once the
/// read has failed.
///
/// # Errors
///
/// Returns [`WorkspaceIndexError`] when the read fails for any other reason.
fn observed_record(
    policy: &WorkspaceSourcePolicy,
    root: &Path,
    path: &ProjectPath,
) -> Result<Option<FileRecord>, WorkspaceIndexError> {
    let absolute = root.join(path.as_str());
    match policy.visible_digest(&absolute) {
        Ok(digest) => Ok(digest.map(FileRecord::Digest)),
        Err(_) if absolute.is_dir() => Ok(None),
        Err(error) => match error.fault().left_out_file(path.clone()) {
            Some(warning) => Ok(Some(FileRecord::LeftOut(warning))),
            None => Err(error),
        },
    }
}

#[derive(Clone, Debug)]
struct VisibleWorkspaceFiles {
    digests: Arc<WorkspaceDigests>,
    refused: Arc<BTreeMap<ProjectPath, rift_index::WorkspaceIndexWarning>>,
}

/// Read index and configuration policy published as one immutable value.
#[derive(Debug)]
pub(crate) struct PublishedWorkspace {
    pub(crate) reads: Arc<ReadService>,
    pub(crate) configuration: ConfigurationState,
    pub(crate) fingerprint: WorkspaceFingerprint,
    pub(crate) source_policy: Option<Arc<WorkspaceSourcePolicy>>,
    pub(crate) preparation: Option<LocalIndexPreparation>,
    /// Workspace resource digests captured with this publication. Prepared publications
    /// carry the indexed batch; complete publications carry every visible file.
    pub(crate) visible_digests: Arc<WorkspaceDigests>,
    /// Visible files left out by the per-file bound, keyed for observation comparisons.
    visible_refused: Arc<BTreeMap<ProjectPath, rift_index::WorkspaceIndexWarning>>,
    /// Workspace orientation snapshot served by `rift://map`, computed once for this
    /// publication and reused until the next one - a read costs a lookup, never a rebuild.
    pub(crate) map: Arc<WorkspaceMap>,
    pub(crate) epoch: u64,
}

impl PublishedWorkspace {
    /// The record this publication uses to compare a path capture, including visible paths
    /// the read index does not classify.
    fn file_record(&self, path: &ProjectPath) -> Option<FileRecord> {
        self.visible_digests
            .get(path)
            .map(FileRecord::Digest)
            .or_else(|| {
                self.visible_refused
                    .get(path)
                    .cloned()
                    .map(FileRecord::LeftOut)
            })
            .or_else(|| self.reads.file_record(path))
    }

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
            source_policy: self.source_policy.as_ref().map(Arc::clone),
            preparation: self.preparation.clone(),
            visible_digests: Arc::clone(&self.visible_digests),
            visible_refused: Arc::clone(&self.visible_refused),
            map: Arc::clone(&self.map),
            epoch,
        }
    }

    /// Replaces request-named paths in the complete visible set, including paths the read
    /// index does not classify.
    fn update_visible_digests(
        &self,
        root: &Path,
        reads: &ReadService,
        paths: &BTreeSet<ProjectPath>,
    ) -> Result<VisibleWorkspaceFiles, ReadError> {
        if self.preparation.is_some() {
            return Ok(VisibleWorkspaceFiles {
                digests: Arc::new(reads.content_digests()),
                refused: Arc::new(BTreeMap::new()),
            });
        }
        if paths.is_empty() {
            return Ok(VisibleWorkspaceFiles {
                digests: Arc::clone(&self.visible_digests),
                refused: Arc::clone(&self.visible_refused),
            });
        }
        let policy = self.source_policy.as_deref().unwrap_or_else(|| {
            unreachable!("a complete current-tree publication holds its source policy")
        });
        let mut digests: BTreeMap<ProjectPath, rift_index::FileDigest> = self
            .visible_digests
            .iter()
            .map(|(path, digest)| (path.clone(), digest))
            .collect();
        let mut refused = self.visible_refused.as_ref().clone();
        for path in paths {
            let digest = match reads.file_digest(path) {
                Some(digest) => Ok(Some(digest)),
                None => policy.visible_digest(&root.join(path.as_str())),
            };
            match digest {
                Ok(Some(digest)) => {
                    digests.insert(path.clone(), digest);
                    refused.remove(path);
                }
                Ok(None) => {
                    digests.remove(path);
                    refused.remove(path);
                }
                Err(error) if error.fault().left_out_file(path.clone()).is_some() => {
                    digests.remove(path);
                    let warning = error
                        .fault()
                        .left_out_file(path.clone())
                        .unwrap_or_else(|| unreachable!("the guard found a left-out file"));
                    refused.insert(path.clone(), warning);
                }
                Err(error) => return Err(ReadFault::index(error)),
            }
        }
        Ok(VisibleWorkspaceFiles {
            digests: Arc::new(WorkspaceDigests::new(digests)),
            refused: Arc::new(refused),
        })
    }

    /// Whether this publication already holds what every one of `paths` holds on disk,
    /// read under its own policy, which reads a path it excludes as absent.
    ///
    /// A file left out under the per-file byte bound matches only a publication that left
    /// it out the same way; an I/O error matches nothing.
    fn holds_observed(&self, root: &Path, paths: &BTreeSet<ProjectPath>) -> bool {
        self.source_policy
            .as_deref()
            .and_then(|policy| observed_records(root, paths, policy))
            .is_some_and(|observed| {
                PathChanges::resolve(observed, |path| self.file_record(path)).is_empty()
            })
    }

    /// Whether a rebuild naming `paths` moves every one of them: each reads, under this
    /// publication's own policy, as something other than what this publication holds.
    ///
    /// This is the comparison [`RebuildRequest::change_set`] makes, so a path that fails it
    /// is one a rebuild naming it leaves as it is. A read that fails asks for the whole
    /// workspace there, and answers `false` here.
    pub(crate) fn rebuild_moves_every(&self, root: &Path, paths: &BTreeSet<ProjectPath>) -> bool {
        self.source_policy
            .as_deref()
            .and_then(|policy| observed_records(root, paths, policy))
            .is_some_and(|observed| {
                PathChanges::resolve(observed, |path| self.file_record(path)).len() == paths.len()
            })
    }

    /// Whether this publication holds files below one of `paths` that is no longer a
    /// directory on disk.
    ///
    /// Such a path is a directory renamed or removed by an event classified before this
    /// publication held its files: before the first publication, or against an earlier
    /// one. Reading the path finds nothing, so only a scan of the whole workspace drops
    /// the files it held. Work is one ordered-map probe per path, and a disk probe only for
    /// a path this publication holds files below.
    fn holds_files_below_a_gone_path(&self, root: &Path, paths: &BTreeSet<ProjectPath>) -> bool {
        paths
            .iter()
            .any(|path| self.reads.holds_files_below(path) && !root.join(path.as_str()).is_dir())
    }
}

/// Progress carried by one local publication while discovery or file preparation remains.
#[derive(Clone, Debug)]
pub(crate) struct LocalIndexPreparation {
    pub(crate) prepared: usize,
    pub(crate) total: Option<usize>,
    pub(crate) source_paths: Arc<Vec<ProjectPath>>,
    pub(crate) other_paths: Arc<Vec<ProjectPath>>,
    pub(crate) map_source_paths: Arc<Vec<(ProjectPath, rift_protocol::read::Language)>>,
    pub(crate) map_text_paths: Arc<Vec<ProjectPath>>,
}

impl LocalIndexPreparation {
    pub(crate) fn warning(&self) -> Option<ReadWarning> {
        if self.total.is_some_and(|total| self.prepared >= total) {
            return None;
        }
        Some(ReadWarning::LocalIndexPreparing {
            prepared: u64::try_from(self.prepared).unwrap_or(u64::MAX),
            total: self
                .total
                .map(|total| u64::try_from(total).unwrap_or(u64::MAX)),
            ready_in: None,
            detail: "selected local files are still being prepared".to_owned(),
        })
    }
}

fn publication_map(
    reads: &ReadService,
    preparation: Option<&LocalIndexPreparation>,
) -> Arc<WorkspaceMap> {
    let mut map = preparation.map_or_else(
        || reads.workspace_map(),
        |preparation| {
            reads.workspace_preparation_map(
                &preparation.map_source_paths,
                &preparation.map_text_paths,
            )
        },
    );
    if let Some(warning) = preparation.and_then(LocalIndexPreparation::warning) {
        map.warnings.push(warning);
    }
    Arc::new(map)
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
    /// Latest successful candidate whose publication was superseded. A publication at
    /// or beyond this epoch makes the recorded movement obsolete.
    superseded_epoch: AtomicU64,
    /// The publication linearization point, holding the work the next rebuild owes.
    /// Observation and publication both take it, so a path observed between a rebuild's
    /// capture and its publication cannot be lost.
    pub(crate) publication_lane: SyncMutex<PendingWork>,
    paths_max: usize,
    initial_preparation: SyncMutex<Option<WorkspaceIndexPreparation>>,

    /// The current publication as event classification sees it: the inclusion policy
    /// the index was built under, and the index itself for what it holds. Absent before
    /// the first publication, when an event names its path by its spelling below the root
    /// and the startup candidate's own policy decides what that path holds.
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
    /// The engine hold each publication hands its changed files to, set once the server
    /// holds one. Absent in a test that builds no server.
    engines: std::sync::OnceLock<Arc<EngineHold>>,
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
    pub(crate) validation: Arc<IndexValidation>,
    pub(crate) blocking: BlockingExecutor,
    /// The workspace's population lane, absent when the search index could not be opened
    /// at startup.
    pub(crate) population: Option<PopulationLane>,
    /// The workspace's lexical lane, handed each published candidate's write, absent
    /// exactly when the population lane is: with no index open there is no store to
    /// commit to, and `search` reports the tier unavailable for the life of this server.
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

    /// The `[global]` table from the last acceptance, or the default table while
    /// `rift.toml` is invalid or absent.
    pub(crate) fn global_configuration(&self) -> GlobalConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.global.clone())
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

    /// The bounds the index builds under: `base` with its file count, aggregate byte, and
    /// declaration bounds replaced by the `[source]` table's `files`, `workspace_size`,
    /// and `declarations`, and its syntax and per-file bounds by `[providers.syntax]`. The
    /// depth and result bounds stay as `base` carries them.
    pub(crate) fn index_limits(
        &self,
        base: WorkspaceIndexLimits,
    ) -> Result<WorkspaceIndexLimits, ReadError> {
        let source = self.source_configuration();
        let files_max = usize::try_from(source.files).unwrap_or(usize::MAX);
        let workspace_bytes_max =
            usize::try_from(source.workspace_size.bytes()).unwrap_or(usize::MAX);
        let declarations_max = usize::try_from(source.declarations).unwrap_or(usize::MAX);
        let syntax = self.syntax_configuration();
        let large_files = self.text_inclusion().large_files();
        base.with_workspace_bounds(files_max, workspace_bytes_max, declarations_max)
            .and_then(|limits| limits.with_syntax_configuration(&syntax))
            .map(|limits| limits.with_large_files(large_files))
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

    /// The `[search.text]` inclusion and `[documentation]` table from the last acceptance, or
    /// the defaults while `rift.toml` is invalid.
    pub(crate) fn text_inclusion(&self) -> TextFileInclusion {
        self.accepted
            .as_ref()
            .map_or_else(|_| TextFileInclusion::default(), TextFileInclusion::from)
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
    /// The `[dependencies]` table counts as index-owned: the read service reads its
    /// dependency context under the table's packages and resolution policy.
    ///
    /// `[providers.syntax]` counts too: its bounds decide which declarations a file's parse
    /// keeps, so a publication built under other bounds holds other units. The
    /// `[documentation]` table rides the text inclusion: it decides which text files the
    /// index collects as documentation.
    fn index_configuration_differs(&self, other: &Self) -> bool {
        self.source_configuration() != other.source_configuration()
            || self.text_inclusion() != other.text_inclusion()
            || self.language_file_selections() != other.language_file_selections()
            || self.dependencies_configuration() != other.dependencies_configuration()
            || self.syntax_configuration() != other.syntax_configuration()
    }

    /// Digest of every configuration table the index derives its content under: the
    /// `[source]`, `[search.text]`, `[documentation]`, `[languages]`, `[dependencies]`, and
    /// `[providers.syntax]` tables [`Self::index_configuration_differs`] compares, and the
    /// `[search.lexical]` bounds the lexical lane writes under. An invalid `rift.toml`
    /// digests the defaults, the configuration every accessor here answers while
    /// acceptance refuses the file.
    pub(crate) fn index_configuration_digest(&self) -> String {
        let defaults = WorkspaceConfiguration::default();
        let configuration = self.accepted.as_ref().unwrap_or(&defaults);
        let tables = serde_json::json!({
            "source": configuration.source,
            "text": configuration.search.text,
            "documentation": configuration.documentation,
            "lexical": configuration.search.lexical,
            "languages": configuration.languages,
            "dependencies": configuration.dependencies,
            "syntax": configuration.providers.syntax,
        });
        format!("{:x}", Sha256::digest(tables.to_string().as_bytes()))
    }

    /// The `[providers.syntax]` bounds from the last acceptance, or the defaults while
    /// `rift.toml` is invalid.
    fn syntax_configuration(&self) -> SyntaxConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.providers.syntax.clone())
            .unwrap_or_default()
    }

    /// The `[providers.history]` table from the last acceptance, or the
    /// default table while `rift.toml` is invalid.
    pub(crate) fn history_configuration(&self) -> HistoryConfiguration {
        self.accepted
            .as_ref()
            .map(|configuration| configuration.providers.history.clone())
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
                superseded_epoch: AtomicU64::new(0),
                publication_lane: SyncMutex::new(PendingWork::default()),
                paths_max,
                initial_preparation: SyncMutex::new(None),
                published: SyncRwLock::new(None),
                cancellation: CancellationToken::new(),
                task: AsyncMutex::new(None),
                engines: std::sync::OnceLock::new(),
            }),
            receiver,
        )
    }

    /// Gives the supervisor the one preparation created for this workspace.
    pub(crate) fn prepare_initial(&self, preparation: WorkspaceIndexPreparation) {
        *self
            .initial_preparation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(preparation);
    }

    /// Takes the initial preparation once, transferring its retained file state to supervisor.
    fn take_initial_preparation(&self) -> Option<WorkspaceIndexPreparation> {
        self.initial_preparation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Hands every later publication's changed files to `engines`.
    ///
    /// The server builds one hold for its one validation, so the first hold handed here
    /// is the one fed, and a second is ignored.
    pub(crate) fn feed_engines(&self, engines: Arc<EngineHold>) {
        let _first_hold_stays = self.engines.set(engines);
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
            cancellation: self.cancellation.clone(),
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

    /// Latest successful capture superseded after this publication was built, if any.
    pub(crate) fn superseded_after(&self, published_epoch: u64) -> Option<u64> {
        let epoch = self.superseded_epoch();
        (epoch > published_epoch).then_some(epoch)
    }

    /// Epoch of the latest successful capture superseded at publication.
    ///
    /// Zero before the first one.
    pub(crate) fn superseded_epoch(&self) -> u64 {
        self.superseded_epoch.load(Ordering::SeqCst)
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
    fn observe_event(&self, roots: &WatchRoots, event: &Event) -> Result<Option<u64>, ReadError> {
        let mut publication = self.locked_pending();
        let result = match watch_event_impact(roots, self, event) {
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
    ///
    /// Before the first publication there is no policy to ask, so the path's spelling below
    /// the canonical root names it. The startup candidate digests that path under its own
    /// policy before it publishes, which reads a path the policy excludes as absent.
    fn source_project_path(&self, roots: &WatchRoots, path: &Path) -> Option<ProjectPath> {
        self.current_publication().as_ref().map_or_else(
            || roots.project_path(path),
            |published| {
                published
                    .source_policy
                    .as_deref()
                    .and_then(|policy| policy.project_path(path))
                    .or_else(|| roots.project_path(path))
            },
        )
    }

    /// Returns whether current policy includes one source event path.
    fn source_path_is_relevant(&self, path: &Path) -> bool {
        self.current_publication().as_ref().is_none_or(|published| {
            published
                .source_policy
                .as_deref()
                .is_none_or(|policy| policy.visible(path))
        })
    }

    /// Returns whether current policy can include source below one directory.
    fn source_directory_is_relevant(&self, path: &Path) -> bool {
        self.current_publication().as_ref().is_none_or(|published| {
            published
                .source_policy
                .as_deref()
                .is_none_or(|policy| policy.may_include_descendant(path))
        })
    }

    /// Whether the current publication holds at least one file below one event path.
    ///
    /// A path outside the policy's root, or one observed before the first publication,
    /// holds nothing the index knows about. Before that publication the startup candidate
    /// answers the same question for the paths it is observed with, through
    /// [`PublishedWorkspace::holds_files_below_a_gone_path`].
    fn published_holds_files_below(&self, path: &Path) -> bool {
        let current = self.current_publication();
        let Some(published) = current.as_ref() else {
            return false;
        };
        published
            .source_policy
            .as_deref()
            .and_then(|policy| policy.project_path(path))
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
            |published| {
                published
                    .source_policy
                    .as_deref()
                    .is_none_or(|policy| policy.is_workspace_configuration(path))
            },
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
            |published| {
                published
                    .source_policy
                    .as_deref()
                    .is_none_or(|policy| policy.decides_inclusion(path))
            },
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

/// The two spellings one watcher compares an event path against.
///
/// `canonical` is the root the index scan and the published source policy use; `watched`
/// is the spelling the server was started with, which a platform whose temporary root
/// reaches the watcher through a symlink hands back in its events.
/// [`WorkspaceSourcePolicy`](rift_index::WorkspaceSourcePolicy) carries the same pair for
/// the same reason and answers for it once a publication is installed; this pair answers
/// from the first event, which is the window the initial index build runs in.
#[derive(Clone, Debug)]
pub(crate) struct WatchRoots {
    canonical: PathBuf,
    watched: PathBuf,
    git_directory: Option<PathBuf>,
}

impl WatchRoots {
    /// The spellings a watch over `root` compares against.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when `root` cannot be canonicalized.
    pub(crate) fn resolve(root: &Path) -> Result<Self, ReadError> {
        let canonical = std::fs::canonicalize(root)
            .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
        Ok(Self {
            git_directory: rift_history::Repository::open(&canonical)
                .ok()
                .and_then(|repository| {
                    repository.index_lock_path().parent().map(Path::to_path_buf)
                }),
            canonical,
            watched: root.to_path_buf(),
        })
    }

    /// The canonical root, which every comparison runs against.
    pub(crate) fn canonical(&self) -> &Path {
        &self.canonical
    }

    /// One spelling standing for both, for a test whose root is not on disk to
    /// canonicalize.
    #[cfg(test)]
    pub(crate) fn at(root: &Path) -> Self {
        Self {
            canonical: root.to_path_buf(),
            watched: root.to_path_buf(),
            git_directory: None,
        }
    }

    /// `path`'s project-relative form under either spelling, or nothing when it lies
    /// under neither.
    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        path.strip_prefix(&self.canonical)
            .or_else(|_| path.strip_prefix(&self.watched))
            .ok()
    }

    /// The project path `path` spells below either root spelling, or nothing when it lies
    /// under neither or spells no valid project path.
    fn project_path(&self, path: &Path) -> Option<ProjectPath> {
        rift_index::relative_path(self.relative(path)?).ok()
    }

    /// `path` under the canonical root, or nothing when it lies under neither spelling.
    fn placed<'a>(&self, path: &'a Path) -> Option<std::borrow::Cow<'a, Path>> {
        if path.strip_prefix(&self.canonical).is_ok() {
            return Some(std::borrow::Cow::Borrowed(path));
        }
        let relative = path.strip_prefix(&self.watched).ok()?;
        Some(std::borrow::Cow::Owned(self.canonical.join(relative)))
    }
}

/// One watcher start over a workspace root: the native watcher production runs, or a test's
/// watcher on no path.
pub(crate) trait WatchWorkspace:
    FnOnce(&Path, &Arc<IndexValidation>) -> Result<notify::RecommendedWatcher, ReadError>
    + Send
    + 'static
{
}

impl<Watch> WatchWorkspace for Watch where
    Watch: FnOnce(&Path, &Arc<IndexValidation>) -> Result<notify::RecommendedWatcher, ReadError>
        + Send
        + 'static
{
}

/// Creates one native watcher rooted before the initial index scan.
pub(crate) fn workspace_watcher(
    root: &Path,
    validation: &Arc<IndexValidation>,
) -> Result<notify::RecommendedWatcher, ReadError> {
    let roots = WatchRoots::resolve(root)?;
    let event_roots = roots.clone();
    let validation = Arc::clone(validation);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        report_watch_outcome(&event_roots, &validation, result);
    })
    .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    watcher
        .watch(roots.canonical(), RecursiveMode::Recursive)
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    if let Some(git_directory) = &roots.git_directory {
        watcher
            .watch(git_directory, RecursiveMode::NonRecursive)
            .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))?;
    }
    Ok(watcher)
}

/// A watcher on no path, for a test that moves the epoch itself.
///
/// A native watcher over the test's directory can report a write the test made before the
/// watcher started: `FSEvents` did on macOS, and Windows detects a last-write change "only
/// when the file is written to the disk" (`ReadDirectoryChangesW`). Each report moves the
/// epoch past the one the test asserts.
///
/// # Errors
///
/// Returns [`ReadError`] when the platform refuses to create a watcher.
#[cfg(test)]
pub(crate) fn unwatched(
    _root: &Path,
    _validation: &Arc<IndexValidation>,
) -> Result<notify::RecommendedWatcher, ReadError> {
    notify::recommended_watcher(|_: notify::Result<Event>| {})
        .map_err(|error| ReadFault::unavailable("workspace watch", error.to_string()))
}

/// Observes one watcher callback: a delivered event enters the inclusion filter, and a
/// backend failure marks the watch unhealthy.
pub(crate) fn report_watch_outcome(
    roots: &WatchRoots,
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
    // Git control changes live outside linked worktrees. Route HEAD and index.lock
    // before the source floor; access events keep the existing rescan classification.
    if !matches!(event.kind, EventKind::Access(_))
        && roots.git_directory.as_ref().is_some_and(|directory| {
            event.paths.iter().any(|path| {
                path.parent() == Some(directory.as_path())
                    && matches!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some("HEAD" | "index.lock")
                    )
            })
        })
    {
        if let Err(error) = validation.observe_whole_workspace() {
            tracing::error!(component = "index", operation = "watch.observe", error = %error, "index watch failed");
        }
        return;
    }
    if validation.observe_event(roots, &event).is_err() {
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
    roots: &WatchRoots,
    validation: &IndexValidation,
    event: &Event,
) -> WatchImpact {
    if event.need_rescan() {
        return WatchImpact::WholeWorkspace;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return WatchImpact::None;
    }
    event
        .paths
        .iter()
        .filter(|path| hard_floor_includes_watch_path(roots, path))
        .map(|path| watch_path_impact(roots, validation, event.kind, path))
        .fold(WatchImpact::None, WatchImpact::absorb)
}

/// Whether one event path stays above Rift's hard floor.
///
/// The path is placed under the canonical root first, because the watcher reports
/// whatever spelling the platform hands it, and the floor's names say nothing about a
/// path it cannot place. A path under neither spelling is dropped. This arm used to admit
/// such a path instead, and what it admitted was the server's own `.rift` state: the
/// workspace database moves continuously while SQLite runs, so every one of those writes
/// moved the filesystem epoch that reads and the initial build both wait on.
///
/// Dropping an unplaceable path is safe only beside that placement. On a platform whose
/// root reaches the watcher through a symlink every event carries the other spelling, so
/// a floor that dropped them without mapping first would leave the index never
/// invalidated at all.
pub(crate) fn hard_floor_includes_watch_path(roots: &WatchRoots, path: &Path) -> bool {
    let Some(relative) = roots.relative(path) else {
        return false;
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
/// The path is placed under the canonical root first, so every comparison below runs on
/// the spelling the index and the published source policy use rather than on the bytes
/// the event carried.
///
/// A policy file rewrites what the workspace includes and a directory event can add or
/// drop many files at once, so both ask for the whole workspace. Only a path that is
/// itself a visible file narrows the next rebuild to that file. Before the first
/// publication no policy decides visibility yet, so every such path names itself, and the
/// startup candidate's own policy decides what it holds.
///
/// A name event, and a create or remove event whose kind does not say it names a file,
/// asks for the whole workspace only when its path names a directory the index can hold
/// files under: one on disk right now, or one the publication holds files below, as a
/// directory renamed or removed is. notify reports every create and remove on Windows as
/// `Create(Any)` and `Remove(Any)`, so a directory moved in or out arrives in that shape,
/// with no event for the files it carries. External writes can stage each file as an
/// extensionless temporary file beside its target and rename it over the target; the
/// publication holds nothing under that staging name, so its name event takes the
/// per-path route a file event takes.
///
/// A modify event on a directory adds nothing: Windows reports a directory's last-write
/// time moving whenever an entry inside it is added or removed, and the entries report
/// their own events. One the publication holds files below is known a directory without a
/// disk probe and has no impact; any other directory reaches the rebuild as a path, where
/// [`observed_record`] reads it as holding no file.
pub(crate) fn watch_path_impact(
    roots: &WatchRoots,
    validation: &IndexValidation,
    kind: EventKind,
    path: &Path,
) -> WatchImpact {
    let Some(placed) = roots.placed(path) else {
        return WatchImpact::None;
    };
    let path = placed.as_ref();
    let root = roots.canonical();
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
    let folder_modified = matches!(
        kind,
        EventKind::Modify(
            ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Other,
        )
    ) && validation.published_holds_files_below(path);
    if folder_modified {
        return WatchImpact::None;
    }
    let reshapes_tree = match kind {
        EventKind::Create(CreateKind::File)
        | EventKind::Remove(RemoveKind::File)
        | EventKind::Modify(
            ModifyKind::Any | ModifyKind::Data(_) | ModifyKind::Metadata(_) | ModifyKind::Other,
        )
        | EventKind::Access(_) => false,
        EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Name(_))
        | EventKind::Any
        | EventKind::Other => names_a_directory(validation, kind, path),
    };
    if reshapes_tree {
        return WatchImpact::WholeWorkspace;
    }
    if !validation.source_path_is_relevant(path) {
        return WatchImpact::None;
    }
    validation
        .source_project_path(roots, path)
        .map_or(WatchImpact::WholeWorkspace, |path| {
            WatchImpact::Paths(vec![path])
        })
}

/// Whether one event path names a directory the index can hold files under: a path the
/// policy may include descendants of, that the publication holds files below or, unless the
/// event reports the side a removal or a rename left, that is a directory on disk now.
///
/// The side decides which fact is read, whatever the path's spelling. A path that left is
/// gone from the disk, so only the publication knows what it held, and asking it is an
/// ordered-map probe. Only a path an event may have created or renamed into place costs a
/// disk probe, the one filesystem read event classification makes, so a file's content
/// write costs none. A directory's name may carry a dot like a file's, which is why the
/// spelling decides nothing.
fn names_a_directory(validation: &IndexValidation, kind: EventKind, path: &Path) -> bool {
    if !validation.source_directory_is_relevant(path) {
        return false;
    }
    let left = matches!(
        kind,
        EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From))
    );
    validation.published_holds_files_below(path) || (!left && path.is_dir())
}

/// Creates an empty readable publication and retains its owner for background preparation.
pub(crate) fn empty_workspace_preparation(
    root: &Path,
    limits: WorkspaceIndexLimits,
    configuration: ConfigurationState,
    epoch: u64,
    content_cache: rift_index::WorkspaceContentCache,
) -> Result<(Arc<PublishedWorkspace>, WorkspaceIndexPreparation), ReadError> {
    let limits = configuration.index_limits(limits)?;
    let preparation = WorkspaceIndexPreparation::new(
        root,
        limits,
        &configuration.text_inclusion(),
        &configuration.language_file_selections(),
    )
    .map_err(ReadFault::index)?
    .with_content_cache(content_cache);
    let index = preparation.empty_snapshot().map_err(ReadFault::index)?;
    let reads = ReadService::from_prepared_index(
        index,
        None,
        Arc::new(DependencyContext::default()),
        configuration.history_configuration(),
        configuration.dependencies_configuration(),
    );
    let preparation_state = LocalIndexPreparation {
        prepared: 0,
        total: None,
        source_paths: Arc::new(Vec::new()),
        other_paths: Arc::new(Vec::new()),
        map_source_paths: Arc::new(Vec::new()),
        map_text_paths: Arc::new(Vec::new()),
    };
    let map = publication_map(&reads, Some(&preparation_state));
    let published = Arc::new(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy: None,
        preparation: Some(preparation_state),
        visible_digests: Arc::new(WorkspaceDigests::default()),
        visible_refused: Arc::new(BTreeMap::new()),
        map,
        epoch,
    });
    Ok((published, preparation))
}

/// Captures only paths held by one immutable publication.
pub(crate) fn capture_prepared_workspace(
    root: &Path,
    published: &PublishedWorkspace,
    last: &LastCapture,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(WorkspaceDigests, LastCapture), ReadError> {
    let limits = published.reads.workspace_limits();
    if let Some(preparation) = &published.preparation {
        return capture_selected_paths_cancellable(
            root,
            limits,
            &preparation.source_paths,
            &preparation.other_paths,
            last,
            cancelled,
        )
        .map_err(ReadFault::index);
    }
    let visibility = published.configuration.source_visibility();
    let text_inclusion = published.configuration.text_inclusion();
    let languages = published.configuration.language_file_selections();
    capture_digests_with_languages_cancellable(
        root,
        limits,
        &visibility,
        &text_inclusion,
        &languages,
        last,
        cancelled,
    )
    .map_err(ReadFault::index)
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

/// The capture production runs: the whole-workspace scan.
#[cfg(test)]
pub(crate) fn workspace_capture() -> impl CaptureWorkspace + Clone + Send + 'static {
    build_workspace_candidate
}

fn workspace_capture_with_cache(
    content_cache: rift_index::WorkspaceContentCache,
) -> impl CaptureWorkspace + Clone + Send + 'static {
    move |root, limits, request| {
        build_workspace_candidate_with_cache(root, limits, request, &content_cache)
    }
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
#[cfg(test)]
pub(crate) fn build_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    request: &RebuildRequest,
) -> Result<WorkspaceCandidate, ReadError> {
    build_workspace_candidate_with_cache(
        root,
        limits,
        request,
        &rift_index::WorkspaceContentCache::default(),
    )
}

fn build_workspace_candidate_with_cache(
    root: &Path,
    limits: WorkspaceIndexLimits,
    request: &RebuildRequest,
    content_cache: &rift_index::WorkspaceContentCache,
) -> Result<WorkspaceCandidate, ReadError> {
    tracing::debug!(
        component = "index",
        operation = "index.build",
        phase = "start",
        epoch = request.epoch,
        "index capture started"
    );
    let configuration = ConfigurationState::accept(root);
    let change_set = request.change_set(root, &configuration);
    let cancelled = || request.cancellation.is_cancelled();
    let candidate = match &change_set {
        ChangeSet::Full => {
            let sharing = request.previous.as_deref().filter(|previous| {
                !previous
                    .configuration
                    .index_configuration_differs(&configuration)
            });
            whole_workspace_candidate(
                root,
                limits,
                configuration,
                request.epoch,
                sharing,
                &cancelled,
                content_cache,
            )?
        }
        ChangeSet::Incremental(changes) => {
            let previous = request
                .previous
                .as_ref()
                .unwrap_or_else(|| unreachable!("an incremental change set names a publication"));
            shared_workspace_candidate(
                root,
                previous,
                changes,
                &request.work.paths,
                configuration,
                request.epoch,
                &cancelled,
            )?
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
/// A change set names exactly the paths whose rows move, and deriving their units is the
/// candidate's own work, so it runs beside the parse rather than inside the commit. A whole
/// rebuild names no difference: the lane compares every file the publication holds with the
/// digests the store recorded, and derives units only for the files that differ.
pub(crate) fn lexical_write(
    published: &PublishedWorkspace,
    change_set: &ChangeSet,
) -> LexicalWrite {
    if published.preparation.is_some() {
        return LexicalWrite::Hold;
    }
    match change_set {
        ChangeSet::Full => LexicalWrite::Whole,
        ChangeSet::Incremental(changes) => {
            LexicalWrite::Change(published.reads.lexical_change(changes))
        }
    }
}

/// Retains every visible file digest, reusing index-held content digests and reading only
/// paths the index did not classify or could not digest.
fn complete_visible_digests(
    root: &Path,
    reads: &ReadService,
    policy: &WorkspaceSourcePolicy,
) -> Result<VisibleWorkspaceFiles, ReadError> {
    let mut digests = Vec::new();
    let mut refused = BTreeMap::new();
    for path in policy.visible_paths().map_err(ReadFault::index)? {
        let digest = match reads.file_digest(&path) {
            Some(digest) => Some(digest),
            None => match policy.visible_digest(&root.join(path.as_str())) {
                Ok(digest) => digest,
                Err(error) if error.fault().left_out_file(path.clone()).is_some() => {
                    let warning = error
                        .fault()
                        .left_out_file(path.clone())
                        .unwrap_or_else(|| unreachable!("the guard found a left-out file"));
                    refused.insert(path.clone(), warning);
                    None
                }
                Err(error) => return Err(ReadFault::index(error)),
            },
        };
        if let Some(digest) = digest {
            digests.push((path, digest));
        }
    }
    Ok(VisibleWorkspaceFiles {
        digests: Arc::new(WorkspaceDigests::new(digests)),
        refused: Arc::new(refused),
    })
}

/// Scans every visible file, taking the `[source]` policy `reads` already compiled
/// rather than compiling a second one - one predicate per snapshot, one walk of the
/// tree's `.gitignore` files instead of two.
///
/// A publication in `sharing` was built under the same index-owned configuration, so every
/// file whose bytes it already parsed is shared rather than parsed again: a folder event or
/// an ignore-file write costs a walk, a read, and a digest of the tree, and a parse of the
/// files that moved.
fn whole_workspace_candidate(
    root: &Path,
    limits: WorkspaceIndexLimits,
    configuration: ConfigurationState,
    epoch: u64,
    sharing: Option<&PublishedWorkspace>,
    cancelled: &(dyn Fn() -> bool + Sync),
    content_cache: &rift_index::WorkspaceContentCache,
) -> Result<PublishedWorkspace, ReadError> {
    let visibility = configuration.source_visibility();
    let limits = configuration.index_limits(limits)?;
    let text_inclusion = configuration.text_inclusion();
    let languages = configuration.language_file_selections();
    let dependencies_configuration = configuration.dependencies_configuration();
    let reads = match sharing {
        Some(previous) => previous.reads.rescanned_cancellable(
            root,
            &visibility,
            &text_inclusion,
            &languages,
            cancelled,
        )?,
        None => ReadService::build_with_languages_cancellable(ReadServiceBuild {
            root,
            limits,
            visibility: &visibility,
            text_inclusion: &text_inclusion,
            languages: &languages,
            history: configuration.history_configuration(),
            dependencies: dependencies_configuration,
            content_cache: Some(content_cache),
            cancelled,
        })?,
    };
    let source_policy = Some(reads.source_policy_handle().unwrap_or_else(|| {
        unreachable!("a current-tree read service always compiles its source policy")
    }));
    let visible_files = complete_visible_digests(
        root,
        &reads,
        source_policy.as_deref().unwrap_or_else(|| {
            unreachable!("a current-tree read service always compiles its source policy")
        }),
    )?;
    let map = publication_map(&reads, None);
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy,
        preparation: None,
        visible_digests: visible_files.digests,
        visible_refused: visible_files.refused,
        map,
        epoch,
    })
}

/// Replaces the files `changes` names and shares every other file with `previous`.
///
/// Index-owned configuration, the compiled source policy, the dependency plan, and the
/// dependency store carry over unchanged. Other accepted configuration may change without
/// rebuilding source files. An empty change set still produces a candidate because
/// current-tree requests wait for its observation epoch; it shares `previous` whole unless
/// a project environment the dependency context listed lists differently now, the one
/// movement no changed path reports, in which case the rebuild reads the context again.
fn shared_workspace_candidate(
    root: &Path,
    previous: &PublishedWorkspace,
    changes: &PathChanges,
    observed_paths: &BTreeSet<ProjectPath>,
    configuration: ConfigurationState,
    epoch: u64,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<PublishedWorkspace, ReadError> {
    if changes.is_empty() && !previous.reads.project_environment_moved() {
        let configuration_changed = previous.configuration.fingerprint != configuration.fingerprint;
        let non_index_configuration_changed = configuration_changed
            && !previous
                .configuration
                .index_configuration_differs(&configuration);
        let configuration_path_observed = non_index_configuration_changed
            && observed_paths
                .iter()
                .any(|path| path.as_str() == WORKSPACE_CONFIGURATION_FILE);
        let holds_observed = if configuration_path_observed {
            let observed_for_tree = observed_paths
                .iter()
                .filter(|path| path.as_str() != WORKSPACE_CONFIGURATION_FILE)
                .cloned()
                .collect();
            previous.holds_observed(root, &observed_for_tree)
        } else {
            previous.holds_observed(root, observed_paths)
        };
        if holds_observed {
            let mut candidate = previous.under(configuration, epoch);
            if configuration_path_observed {
                let visible_files =
                    previous.update_visible_digests(root, &previous.reads, observed_paths)?;
                candidate.visible_digests = visible_files.digests;
                candidate.visible_refused = visible_files.refused;
            }
            return Ok(candidate);
        }
    }
    let reads = previous.reads.rebuilt_cancellable(changes, cancelled)?;
    let visible_files = previous.update_visible_digests(root, &reads, observed_paths)?;
    let map = publication_map(&reads, previous.preparation.as_ref());
    Ok(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration,
        source_policy: previous.source_policy.as_ref().map(Arc::clone),
        preparation: previous.preparation.clone(),
        visible_digests: visible_files.digests,
        visible_refused: visible_files.refused,
        map,
        epoch,
    })
}

/// Embeds the declarations `published` describes, so the vector ranking ranks the tree the
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
/// Population failure is a warning, never a request failure: the vector ranking reports its
/// own readiness, and the next successful publication asks for another pass.
/// A disabled vector ranking still reports chunked files, then skips declaration derivation.
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
    if index.pass_readiness() == VectorReadiness::Disabled {
        return;
    }
    let units = published.reads.symbol_index_documents_by_file();
    let described = published.reads.described_symbol_units_by_file(&units);
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
            "the vector ranking could not embed this publication; the full-text tier keeps \
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
/// Paths one held write names before it compares the whole publication instead: past
/// it, reading every recorded digest costs less than deriving each named path.
pub(crate) const LEXICAL_HELD_PATHS_MAX: usize = 65_536;
/// Tree revisions one held write answers for, the newest kept. A revision past it reads as
/// settled, and a request that captured it recaptures the publication the store holds.
pub(crate) const LEXICAL_HELD_REVISIONS_MAX: usize = 64;

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

/// Derivation revision of lexical rows from the analyzer, corpus, and configuration.
///
/// The full analyzer manifest digest covers the sources deriving stored rows. Unrelated
/// binary changes preserve the revision, while changed analysis inputs invalidate reuse.
pub(crate) fn derivation_revision(
    analyzer_revision: &str,
    configuration: &ConfigurationState,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(analyzer_revision.as_bytes());
    hasher.update([0]);
    hasher.update(rift_ranking::CorpusRevision::current().as_str().as_bytes());
    hasher.update([0]);
    hasher.update(configuration.index_configuration_digest().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// What one lexical commit writes.
#[derive(Debug)]
pub(crate) enum LexicalWrite {
    /// The publication is partial and must not change stored rows.
    Hold,
    /// Every file the publication holds, compared with the digests the store recorded:
    /// only a file whose digest differs, or that one side lacks, is written.
    Whole,
    /// The units one change set names, derived beside the parse.
    Change(LexicalChange),
    /// The paths several writes named, derived from the newest of their publications when
    /// the lane runs them.
    Paths(BTreeSet<ProjectPath>),
}

impl LexicalWrite {
    /// Whether this write would leave the stored set exactly as it is, so the store owes
    /// nothing for it and no transaction opens.
    ///
    /// Only an empty change set reaches this: it shares its predecessor's snapshot, so the
    /// stamp already names the tree revision the candidate answers under.
    pub(crate) fn is_empty(&self) -> bool {
        match self {
            Self::Hold => true,
            Self::Whole => false,
            Self::Change(change) => change.is_empty(),
            Self::Paths(paths) => paths.is_empty(),
        }
    }

    /// The write's form, as a record names it.
    const fn form(&self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Whole => "whole",
            Self::Change(_) => "change",
            Self::Paths(_) => "paths",
        }
    }

    /// This write and a newer one as one write.
    ///
    /// A change replaces whole paths and writing it twice leaves what writing it once left,
    /// so the union of two writes' paths, derived from the newer publication, leaves the
    /// store exactly as running both in order would. A whole write on either side stays
    /// whole, and a union past [`LEXICAL_HELD_PATHS_MAX`] becomes one.
    fn merged_with(self, newer: Self) -> Self {
        let (Some(mut paths), Some(newer)) = (self.named_paths(), newer.named_paths()) else {
            return Self::Whole;
        };
        paths.extend(newer);
        if paths.len() > LEXICAL_HELD_PATHS_MAX {
            return Self::Whole;
        }
        Self::Paths(paths)
    }

    /// The paths this write names, or nothing for a whole write.
    fn named_paths(self) -> Option<BTreeSet<ProjectPath>> {
        match self {
            Self::Hold => Some(BTreeSet::new()),
            Self::Whole => None,
            Self::Change(change) => Some(change.replaced().iter().cloned().collect()),
            Self::Paths(paths) => Some(paths),
        }
    }
}

/// `change` without every unit whose `content` exceeds `unit_bytes_max`, beside the units
/// left out.
///
/// The store refuses a whole batch over one oversized unit, and a publication cannot wait
/// on an operator raising a bound the configuration does not expose. The unit is left out
/// of the lexical index instead: its file stays in the syntax index and keeps answering
/// `get_symbol` and symbol search, and only its text ranking is absent. The change keeps
/// every replaced path and recorded digest, so the stored units of a file whose one unit
/// grew past the bound are still deleted, and the file is not derived again until its
/// bytes change.
fn within_unit_bound(
    change: LexicalChange,
    unit_bytes_max: usize,
) -> (LexicalChange, Vec<IndexDocument>) {
    let (replaced, inserted, recorded) = change.into_parts();
    let (kept, left_out) = inserted
        .into_iter()
        .partition(|unit| unit.content().len() <= unit_bytes_max);
    (
        LexicalChange::new(replaced, kept).with_recorded(recorded),
        left_out,
    )
}

/// Records each unit one commit left out of the lexical index, once per build, in the
/// form `file left out of the index` takes.
fn record_units_left_out(left_out: &[IndexDocument], unit_bytes_max: usize) {
    for unit in left_out {
        tracing::warn!(
            component = "index",
            operation = "index.build",
            path = unit.project_path().map_or("", ProjectPath::as_str),
            observed = unit.content().len(),
            maximum = unit_bytes_max,
            "lexical unit left out of the search index: its content exceeds the unit byte \
             bound; the file still answers get_symbol and symbol search"
        );
    }
}

/// The store one lexical lane writes: the workspace search index, or a test's stand-in.
pub(crate) trait LexicalStore: Send + Sync + 'static {
    /// The content digest each stored file's rows were derived from under
    /// `derivation_revision`, or `None` when no stored row can be kept.
    fn recorded(
        &self,
        derivation_revision: &str,
    ) -> impl Future<Output = Result<Option<WorkspaceDigests>, SearchError>> + Send;

    /// Deletes every row and recorded digest, and stamps no publication under
    /// `derivation_revision`.
    fn clear(
        &self,
        derivation_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send;

    /// Applies `change` and stamps `stamp` in one transaction, replacing the documentation
    /// metadata with `documentation` when one is given and keeping it otherwise.
    fn apply(
        &self,
        change: &LexicalChange,
        stamp: &LexicalStamp,
        documentation: Option<&rift_index::DocumentationCollection>,
    ) -> impl Future<Output = Result<(), SearchError>> + Send;

    /// Indexes the oldest file rows the trigram index lacks in one bounded transaction, and
    /// answers what it indexed and how many rows it still lacks.
    fn index_trigrams(&self) -> impl Future<Output = Result<TrigramBatch, SearchError>> + Send;
}

impl LexicalStore for SearchIndex {
    fn recorded(
        &self,
        derivation_revision: &str,
    ) -> impl Future<Output = Result<Option<WorkspaceDigests>, SearchError>> + Send {
        self.recorded_lexical_files(derivation_revision)
    }

    fn clear(
        &self,
        derivation_revision: &str,
    ) -> impl Future<Output = Result<(), SearchError>> + Send {
        self.clear_lexical(derivation_revision)
    }

    async fn apply(
        &self,
        change: &LexicalChange,
        stamp: &LexicalStamp,
        documentation: Option<&rift_index::DocumentationCollection>,
    ) -> Result<(), SearchError> {
        match documentation {
            Some(documentation) => {
                self.apply_lexical_with_documentation(change, stamp, documentation)
                    .await
            }
            None => self.apply_lexical(change, stamp).await,
        }
    }

    fn index_trigrams(&self) -> impl Future<Output = Result<TrigramBatch, SearchError>> + Send {
        SearchIndex::index_trigrams(self)
    }
}

/// The bounds one lexical lane writes under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub(crate) struct LexicalLaneBounds {
    /// Content bytes one unit may carry before it is left out.
    pub(crate) unit_bytes_max: usize,
    /// Units one transaction writes, at most, whole files kept together.
    pub(crate) transaction_units_max: usize,
    /// Content bytes one transaction writes, at most, whole files kept together.
    pub(crate) transaction_bytes_max: usize,
}

impl LexicalLaneBounds {
    /// The bounds the lexical store's own limits name.
    pub(crate) fn of(limits: rift_index::LexicalIndexLimits) -> Self {
        Self {
            unit_bytes_max: usize::try_from(limits.unit_bytes_max()).unwrap_or(usize::MAX),
            transaction_units_max: limits.transaction_units_max(),
            transaction_bytes_max: limits.transaction_bytes_max(),
        }
    }
}

/// One write handed to the lane, with the publication it was derived for.
#[derive(Debug)]
struct LexicalCommit {
    write: LexicalWrite,
    /// The publication the write answers for. It names the tree revision the last
    /// transaction stamps, and derives the units the store is owed.
    published: Arc<PublishedWorkspace>,
    /// Every tree revision this write answers for once it lands, oldest first: its own,
    /// and those of the writes merged into it, at most [`LEXICAL_HELD_REVISIONS_MAX`].
    answers: VecDeque<String>,
}

impl LexicalCommit {
    fn new(write: LexicalWrite, published: Arc<PublishedWorkspace>) -> Self {
        let answers = VecDeque::from([published.reads.tree_revision().to_owned()]);
        Self {
            write,
            published,
            answers,
        }
    }

    /// This write and a newer one as one write answering for both, beside the older
    /// publication the merge no longer needs.
    fn merged_with(self, newer: Self) -> (Self, Arc<PublishedWorkspace>) {
        let mut answers = self.answers;
        answers.extend(newer.answers);
        let past = answers.len().saturating_sub(LEXICAL_HELD_REVISIONS_MAX);
        answers.drain(..past);
        let merged = Self {
            write: self.write.merged_with(newer.write),
            published: newer.published,
            answers,
        };
        (merged, self.published)
    }

    fn answers_for(&self, tree_revision: &str) -> bool {
        self.answers
            .iter()
            .any(|answered| answered == tree_revision)
    }
}

/// Where one tree revision stands with the lexical lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LexicalCommitState {
    /// The write for it is held or running: the store holds it once that write lands.
    Committing,
    /// No write for it is held or running, and the store missed a commit for the reason
    /// `cause` renders: the next write compares every file the publication holds with
    /// the digests the store recorded.
    Owed {
        /// The refused transaction's own rendering.
        cause: String,
    },
    /// No write for it is held or running, and nothing is owed: the store holds it, or has
    /// moved past it to a newer publication.
    Settled,
}

/// What the lane has been handed and not yet run, and what the store is owed.
#[derive(Debug, Default)]
struct LexicalBacklog {
    /// The write waiting behind the running one. A write handed while one waits merges
    /// into it, so the lane holds one write however long the running one takes.
    held: Option<LexicalCommit>,
    /// The tree revisions the running write answers for, while one runs.
    running: Vec<String>,
    /// Why the store missed a commit, while it has. The next write then compares every file
    /// its publication holds with the digests the store recorded.
    whole_owed: Option<String>,
    /// Whether the lane's task has ended, so a later write has no one to run it.
    ended: bool,
}

impl LexicalBacklog {
    /// Holds `commit` for the task, merging it into the held write when there is one, and
    /// returns the publication the merge released.
    fn hand(&mut self, commit: LexicalCommit) -> Option<Arc<PublishedWorkspace>> {
        let Some(held) = self.held.take() else {
            self.held = Some(commit);
            return None;
        };
        let (merged, released) = held.merged_with(commit);
        self.held = Some(merged);
        Some(released)
    }

    /// Takes the held write for the task, with whether a whole comparison is owed. The owed
    /// comparison becomes that write's responsibility: a failure hands it back through
    /// [`Self::owe_whole`].
    fn take_next(&mut self) -> Option<(LexicalCommit, bool)> {
        let commit = self.held.take()?;
        let whole_owed = self.whole_owed.take().is_some();
        self.running = commit.answers.iter().cloned().collect();
        Some((commit, whole_owed))
    }

    /// Records that the store missed a commit for the reason `cause` renders, so the next
    /// write compares its whole publication with what the store recorded.
    fn owe_whole(&mut self, cause: String) {
        self.whole_owed = Some(cause);
    }

    /// Where `tree_revision` stands: held or running, missed, or settled.
    fn state_of(&self, tree_revision: &str) -> LexicalCommitState {
        let running = self
            .running
            .iter()
            .any(|answered| answered == tree_revision);
        let held = self
            .held
            .as_ref()
            .is_some_and(|commit| commit.answers_for(tree_revision));
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
    /// Wakes every waiter when a commit lands: a write ended, success or failure, or a
    /// merge moved the revisions a held write answers for. Each can move some tree
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
/// A publication hands its write over and returns. The lane runs one write at a time:
/// `SQLite` would otherwise meet two rebuilds' writes as contention rather than as an
/// order. A request that captures a publication before its write lands waits for the
/// landing under `[server] readiness_timeout`, and answers from identifier matching,
/// saying so, only once that budget runs out.
///
/// The lane holds one write behind the running one. A write handed while one waits merges
/// into it: the population lane coalesces the same way, running the newest publication
/// rather than a backlog of superseded ones, and a merged write answers for every revision
/// it absorbed. A burst of publications therefore costs one write per lane turn however
/// long it runs, and a missed commit costs a comparison of every file with the digests
/// the store recorded, never a rewrite of every row.
///
/// Every write runs in parts of at most [`LexicalLaneBounds::transaction_units_max`] units
/// and [`LexicalLaneBounds::transaction_bytes_max`] bytes. Every part but the last stamps
/// no publication, so a reader meets no tree rather than a mix of two, and a stop aborts
/// one part rather than a write of every row.
///
/// Between writes the lane indexes the file rows the trigram index lacks, one transaction
/// under the same two bounds at a time, and a held write always runs before the next one.
/// No request waits for those transactions: a regex search verifies the rows the trigram
/// index lacks, or answers from the rows it holds and warns `pattern_index_preparing`.
///
/// Vector embedding never rides this lane: a lexical row costs one delete and one insert,
/// while embedding one declaration can run for longer than the freshness wait a request
/// is bounded by.
#[derive(Clone, Debug)]
pub(crate) struct LexicalLane {
    queue: Arc<LexicalQueue>,
}

impl LexicalLane {
    /// Spawns the lane's task over `index` and returns the handle a publication hands its
    /// write to. `analyzer_revision` carries the full manifest digest deriving the rows.
    ///
    /// The task ends when the server does, racing the same cancellation token the index
    /// supervisor runs under. A write handed over after that is dropped with a debug line:
    /// no later search reads the store it would have written.
    pub(crate) fn spawn(
        index: Arc<SearchIndex>,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
        analyzer_revision: Arc<str>,
    ) -> Self {
        let bounds = LexicalLaneBounds::of(index.lexical_limits());
        Self::spawn_over(index, bounds, blocking, cancellation, analyzer_revision)
    }

    /// Spawns the lane's task over any [`LexicalStore`], writing under `bounds`. Units the
    /// lane derives itself are derived on `blocking`.
    pub(crate) fn spawn_over<Store: LexicalStore>(
        store: Arc<Store>,
        bounds: LexicalLaneBounds,
        blocking: BlockingExecutor,
        cancellation: CancellationToken,
        analyzer_revision: Arc<str>,
    ) -> Self {
        let queue = Arc::new(LexicalQueue::default());
        let task = LexicalTask {
            store,
            blocking,
            queue: Arc::clone(&queue),
            bounds,
            cancellation,
            analyzer_revision,
        };
        tokio::spawn(task.run());
        Self { queue }
    }

    /// Hands `write` to the lane for `published` and returns, never awaiting the
    /// transaction.
    ///
    /// A write that changes nothing is dropped unless the store is owed a comparison: the
    /// stored stamp already names the tree revision `published` answers under. A write
    /// handed while another waits merges into it, and the waiters on [`Self::landed`] are
    /// woken to read the revisions the merged write answers for.
    pub(crate) fn request(&self, write: LexicalWrite, published: Arc<PublishedWorkspace>) {
        if published.preparation.is_some() {
            return;
        }
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
        let released = backlog.hand(LexicalCommit::new(write, published));
        drop(backlog);
        if released.is_some() {
            tracing::debug!(
                component = "search",
                operation = "search.commit",
                tree_revision,
                "the lexical lane merged this write into the one it holds"
            );
            self.queue.landed.notify_waiters();
        }
        drop(released);
        self.queue.handed.notify_one();
    }

    /// Where `tree_revision` stands with the lane, for a search that found the store
    /// holding another tree.
    pub(crate) fn commit_state(&self, tree_revision: &str) -> LexicalCommitState {
        self.queue.locked().state_of(tree_revision)
    }

    /// A wake-up for the lane's next landing: a write's end, success or failure, or a
    /// merge into the held write.
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

    /// Whether the store is owed a whole comparison right now.
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

/// The lane's task: takes each held write in order and runs it.
struct LexicalTask<Store> {
    store: Arc<Store>,
    blocking: BlockingExecutor,
    queue: Arc<LexicalQueue>,
    bounds: LexicalLaneBounds,
    cancellation: CancellationToken,
    analyzer_revision: Arc<str>,
}

/// The lane task's next piece of work.
enum LaneTurn {
    /// A held write, and whether the store is owed a whole comparison.
    Write(LexicalCommit, bool),
    /// One batch of the file rows the trigram index lacks, while no write is held.
    Trigrams,
}

impl<Store: LexicalStore> LexicalTask<Store> {
    /// Runs held writes, and trigram batches while no write is held, until the token is
    /// cancelled, then marks the lane ended.
    ///
    /// The lane starts owing a trigram batch: a store this process opened may hold rows an
    /// earlier process wrote and stopped before indexing. Every write owes one after it,
    /// since each file row it stored waits for the trigram index. A held write always runs
    /// before the next batch, so a batch delays a write by one bounded transaction at most.
    async fn run(self) {
        let mut trigrams_owed = true;
        while let Some(turn) = self.next_turn(trigrams_owed).await {
            match turn {
                LaneTurn::Write(commit, whole_owed) => {
                    self.write(commit, whole_owed).await;
                    trigrams_owed = true;
                }
                LaneTurn::Trigrams => trigrams_owed = self.index_trigrams().await,
            }
        }
        self.queue.locked().ended = true;
    }

    /// Runs one held write, then wakes the waiters on [`LexicalLane::landed`] once the
    /// backlog records its end, success or failure.
    async fn write(&self, commit: LexicalCommit, whole_owed: bool) {
        let outcome = self.transaction(commit, whole_owed).await;
        let mut backlog = self.queue.locked();
        backlog.running.clear();
        if let Err(error) = outcome {
            backlog.owe_whole(error.to_string());
        }
        drop(backlog);
        self.queue.landed.notify_waiters();
    }

    /// Waits for the next held write, or for the token; a trigram batch when no write is
    /// held and one is owed; nothing once the token is cancelled.
    async fn next_turn(&self, trigrams_owed: bool) -> Option<LaneTurn> {
        loop {
            if self.cancellation.is_cancelled() {
                return None;
            }
            if let Some((commit, whole_owed)) = self.queue.locked().take_next() {
                return Some(LaneTurn::Write(commit, whole_owed));
            }
            if trigrams_owed {
                return Some(LaneTurn::Trigrams);
            }
            tokio::select! {
                () = self.cancellation.cancelled() => return None,
                () = self.queue.handed.notified() => {}
            }
        }
    }

    /// Runs one trigram batch on its own task, as a write's parts run, and answers whether
    /// the trigram index still lacks rows.
    ///
    /// A refused batch is recorded and answers that nothing more is owed: the next write
    /// owes a batch again, so a store that keeps refusing costs one attempt per write
    /// rather than a loop of refusals. Its rows stay lacking meanwhile, and a regex search
    /// verifies them or warns that the trigram index is still being prepared.
    async fn index_trigrams(&self) -> bool {
        let store = Arc::clone(&self.store);
        let running = tokio::spawn(async move { store.index_trigrams().await });
        match self.store_answer(running).await {
            Ok(batch) => {
                tracing::debug!(
                    component = "search",
                    operation = "search.commit",
                    indexed = batch.indexed(),
                    pending = batch.pending(),
                    "the trigram index took a batch of file rows"
                );
                batch.pending() > 0
            }
            Err(error) => {
                if !self.cancellation.is_cancelled() {
                    tracing::warn!(
                        component = "search",
                        operation = "search.commit",
                        error = %error,
                        "the trigram index could not take a batch of file rows; regex \
                         searches verify the rows it lacks or warn, and the next lexical \
                         write retries"
                    );
                }
                false
            }
        }
    }

    /// Runs one write: derives the change it owes, then commits it in parts, the last one
    /// stamping the publication and writing its documentation metadata.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the change could not be derived, when a part failed in
    /// the store, when its task ended without an answer, or when the lane was cancelled
    /// while it ran. Every one leaves the store owing a whole comparison.
    ///
    /// # Cancel safety
    ///
    /// The lane's token cancels the part running, never the other way round: the lane
    /// aborts the part's task and waits for the abort to land before it answers. The abort
    /// drops the store's future, and with it the write turn it holds and the driver's
    /// `Transaction`, whose drop sends `ROLLBACK` to its connection's task without waiting
    /// for it (`toasty::db::Transaction`: "If dropped without calling commit or rollback,
    /// the transaction is automatically rolled back"). The connection runs the rollback
    /// after the statement it is executing, so the write turn frees within one statement's
    /// time. The parts already committed stay: each recorded its paths' digests with their
    /// rows and stamped no publication, so the next start's comparison
    /// (`RebuildRequest::initial` publishes a whole write) writes only the paths the
    /// interrupted write did not reach.
    async fn transaction(&self, commit: LexicalCommit, whole_owed: bool) -> Result<(), ReadError> {
        let LexicalCommit {
            write, published, ..
        } = commit;
        let tree_revision = published.reads.tree_revision().to_owned();
        let derivation = derivation_revision(&self.analyzer_revision, &published.configuration);
        let documentation = published.reads.documentation_snapshot();
        let write = if whole_owed {
            LexicalWrite::Whole
        } else {
            write
        };
        let form = write.form();
        let change = self
            .derived(write, published, &derivation)
            .await
            .inspect_err(|error| record_commit_failure(&tree_revision, form, error))?;
        let bounds = self.bounds;
        let (parts, left_out) = self
            .blocking
            .run_with_cancellation(
                "lexical unit derivation",
                self.cancellation.clone(),
                move |_| {
                    let (change, left_out) = within_unit_bound(change, bounds.unit_bytes_max);
                    let parts = change.into_parts_within(
                        bounds.transaction_units_max,
                        bounds.transaction_bytes_max,
                    );
                    Ok((parts, left_out))
                },
            )
            .await
            .inspect_err(|error| record_commit_failure(&tree_revision, form, error))?;
        record_units_left_out(&left_out, self.bounds.unit_bytes_max);
        // A source selection or resolved reference can change metadata without changing
        // lexical documents, so the last part stamps and writes metadata even when empty.
        let last = parts.len().saturating_sub(1);
        for (index, part) in parts.into_iter().enumerate() {
            let closing = index == last;
            let stamp = if closing {
                LexicalStamp::published(&tree_revision, &derivation)
            } else {
                LexicalStamp::unpublished(&derivation)
            };
            let documentation = closing.then(|| Arc::clone(&documentation));
            self.part(part, stamp, documentation, &tree_revision, form)
                .await?;
        }
        Ok(())
    }

    /// The change `write` owes the store for `published`.
    ///
    /// A change set's units were derived beside the parse. Merged paths are derived again
    /// from the newest publication, on the worker pool. A whole write reads the digests the
    /// store recorded under `derivation_revision` and compares every file `published` holds
    /// with them, so only a file whose bytes moved, or that one side lacks, is derived; a
    /// store holding rows another derivation wrote is cleared first, and every file then
    /// reads as added.
    async fn derived(
        &self,
        write: LexicalWrite,
        published: Arc<PublishedWorkspace>,
        derivation_revision: &str,
    ) -> Result<LexicalChange, ReadError> {
        match write {
            LexicalWrite::Hold => Ok(LexicalChange::default()),
            LexicalWrite::Change(change) => {
                // The prepared write owns its units. Release the publication on the
                // blocking pool before SQL waits, without blocking the lane on its Drop.
                drop(tokio::task::spawn_blocking(move || drop(published)));
                Ok(change)
            }
            LexicalWrite::Paths(paths) => {
                self.blocking
                    .run("lexical unit derivation", move || {
                        Ok(published.reads.lexical_change_for(&paths))
                    })
                    .await
            }
            LexicalWrite::Whole => {
                let recorded = self.recorded_or_cleared(derivation_revision).await?;
                self.blocking
                    .run("lexical unit derivation", move || {
                        let current = published.reads.content_digests();
                        let changes = PathChanges::between(&recorded, &current);
                        Ok(published.reads.lexical_change(&changes))
                    })
                    .await
            }
        }
    }

    /// The digests the store recorded under `derivation_revision`, or none once a store
    /// holding rows another derivation wrote has been cleared.
    async fn recorded_or_cleared(
        &self,
        derivation_revision: &str,
    ) -> Result<WorkspaceDigests, ReadError> {
        let store = Arc::clone(&self.store);
        let derivation = derivation_revision.to_owned();
        let running = tokio::spawn(async move {
            if let Some(recorded) = store.recorded(&derivation).await? {
                return Ok(recorded);
            }
            store.clear(&derivation).await?;
            Ok(WorkspaceDigests::default())
        });
        self.store_answer(running).await
    }

    /// Runs one part as one transaction, under the deadline its unit count derives.
    ///
    /// The transaction runs on its own task: the store executes its statements inline,
    /// so the lane's own task could not observe the deadline while running them. The lane
    /// keeps waiting past the deadline - it records the delay and lets the transaction
    /// end - because a second transaction would only queue behind this one on the
    /// database's write turn.
    async fn part(
        &self,
        part: LexicalChange,
        stamp: LexicalStamp,
        documentation: Option<Arc<rift_index::DocumentationCollection>>,
        tree_revision: &str,
        form: &'static str,
    ) -> Result<(), ReadError> {
        let deadline = commit_deadline(part.replaced().len().saturating_add(part.inserted().len()));
        let store = Arc::clone(&self.store);
        let mut running =
            tokio::spawn(async move { store.apply(&part, &stamp, documentation.as_deref()).await });
        let ended = tokio::select! {
            ended = wait_for_transaction(&mut running, tree_revision, form, deadline) => ended,
            () = self.cancellation.cancelled() => {
                abort_transaction(running).await;
                return Err(lexical_unavailable("the lexical lane ended while the transaction ran"));
            }
        };
        let outcome = match ended {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => ReadFault::unavailable("lexical index commit", error.detail()),
            Err(_) => lexical_unavailable("the lexical transaction's task ended without an answer"),
        };
        record_commit_failure(tree_revision, form, &outcome);
        Err(outcome)
    }

    /// Waits for one store call running on its own task, aborting it when the lane's token
    /// is cancelled first.
    async fn store_answer<T: Send + 'static>(
        &self,
        mut running: JoinHandle<Result<T, SearchError>>,
    ) -> Result<T, ReadError> {
        tokio::select! {
            ended = &mut running => match ended {
                Ok(Ok(answer)) => Ok(answer),
                Ok(Err(error)) => Err(ReadFault::unavailable("lexical index commit", error.detail())),
                Err(_) => Err(lexical_unavailable("the lexical store's task ended without an answer")),
            },
            () = self.cancellation.cancelled() => {
                abort_transaction(running).await;
                Err(lexical_unavailable("the lexical lane ended while the store ran"))
            }
        }
    }
}

/// Waits for a transaction, recording its diagnostic deadline once without ending the wait.
/// The caller races this entire future against cancellation, including the delayed wait.
async fn wait_for_transaction(
    running: &mut JoinHandle<Result<(), SearchError>>,
    tree_revision: &str,
    form: &'static str,
    deadline: Duration,
) -> Result<Result<(), SearchError>, tokio::task::JoinError> {
    if let Ok(ended) = tokio::time::timeout(deadline, &mut *running).await {
        ended
    } else {
        record_commit_delay(tree_revision, form, deadline);
        running.await
    }
}

/// Aborts a running store call's task and waits for the abort to land, so the future
/// holding the store's transaction and the write turn is dropped before the lane goes on.
///
/// The join answers with the abort, or with the outcome of a call that ended before the
/// abort reached it. The lane is ending either way and the next start compares what the
/// store recorded, so neither answer changes what it does.
async fn abort_transaction<T>(running: JoinHandle<T>) {
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
        "the lexical commit failed; the next publication compares every file with the \
         digests the store recorded"
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
/// holds the published tree, and the vector ranking ranks nothing until the pass for that
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
    /// A publication older than the one the lane already holds is dropped. Two callers
    /// ask for a pass - the supervisor as each publication lands, and the vector
    /// preparation once its model is held - and the second reads the current publication
    /// before it asks, so a publication landing between that read and the ask would
    /// otherwise displace the newer tree with the older one. The lane would then embed a
    /// tree nobody is served, and no later ask would correct it, because the newer
    /// publication has already been sent. Epochs are minted in publication order, so
    /// keeping the higher one keeps the newest tree. One publication asked for twice is
    /// two passes over the same tree, which is what the vector preparation asks for when
    /// nothing has published since the model started loading.
    ///
    /// A closed channel is a server already shutting down: the lane's task ended with the
    /// cancellation token, and no later search will read the store this pass would have
    /// written. That is a debug line rather than a caller's failure, because the work this
    /// publication came from already landed.
    pub(crate) fn request(&self, published: Arc<PublishedWorkspace>) {
        if published.preparation.is_some() {
            return;
        }
        if self.publications.is_closed() {
            tracing::debug!(
                component = "search",
                operation = "search.populate",
                "the population lane has ended, so this publication is not populated for"
            );
            return;
        }
        let epoch = published.epoch;
        self.publications.send_if_modified(|waiting| {
            if waiting.as_ref().is_some_and(|held| held.epoch > epoch) {
                return false;
            }
            *waiting = Some(published);
            true
        });
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
/// A published rebuild hands its snapshot to the population lane, then moves on to the
/// next batch. The supervisor awaiting a pass itself
/// would hold the whole reconciliation loop for as long as that pass ran, and the filesystem
/// does not stop moving meanwhile.
pub(crate) async fn run_index_supervisor(
    watcher: notify::RecommendedWatcher,
    invalidations: mpsc::Receiver<()>,
    context: IndexSupervisorContext,
) {
    let capture = workspace_capture_with_cache(context.blocking.content_cache());
    run_index_supervisor_with(watcher, invalidations, context, capture).await;
}

/// Discovers and publishes the selected local index before the supervisor accepts events.
#[cfg(test)]
pub(crate) async fn prepare_initial_workspace(
    context: &IndexSupervisorContext,
) -> Result<(), ReadError> {
    prepare_initial_workspace_with_epoch(context)
        .await
        .map_err(|failure| failure.error)
}

/// Startup failure and exact invalidation epoch queued for its retry.
struct InitialPreparationFailure {
    error: ReadError,
    queued_epoch: Option<u64>,
}

async fn prepare_initial_workspace_with_epoch(
    context: &IndexSupervisorContext,
) -> Result<(), Box<InitialPreparationFailure>> {
    let preparation = match discover_initial_workspace(context).await {
        Ok(preparation) => preparation,
        Err(error) => return Err(Box::new(initial_preparation_failure(context, error))),
    };
    let Some(preparation) = preparation else {
        return Ok(());
    };
    prepare_initial_workspace_from_with_epoch(context, preparation).await
}

/// Retained owner and immutable discovery result for one startup preparation.
pub(crate) struct InitialWorkspacePreparation {
    preparation: WorkspaceIndexPreparation,
    source_policy: Arc<WorkspaceSourcePolicy>,
    configuration: ConfigurationState,
    total: usize,
    map_source_paths: Arc<Vec<(ProjectPath, rift_protocol::read::Language)>>,
    map_text_paths: Arc<Vec<ProjectPath>>,
    last: LastCapture,
}

/// Discovers visible paths and publishes their names before reading and parsing files.
pub(crate) async fn discover_initial_workspace(
    context: &IndexSupervisorContext,
) -> Result<Option<InitialWorkspacePreparation>, ReadError> {
    let Some(preparation) = context.validation.take_initial_preparation() else {
        return Ok(None);
    };
    let validation = Arc::clone(&context.validation);
    let root = context.root.clone();
    let limits = context.limits;
    let cancellation = validation.cancellation.clone();
    let configuration = context
        .published
        .read()
        .await
        .snapshot()
        .0
        .configuration
        .clone();
    let visibility = configuration.source_visibility();
    let text_inclusion = configuration.text_inclusion();
    let languages = configuration.language_file_selections();
    let effective_limits = configuration.index_limits(limits)?;
    let (preparation, source_policy, total, map_source_paths, map_text_paths, empty_index) =
        context
            .blocking
            .run_with_cancellation(
                "initial index discovery",
                cancellation.clone(),
                move |token| {
                    let cancelled = || token.is_cancelled();
                    let empty_index = preparation.empty_snapshot().map_err(ReadFault::index)?;
                    let policy = WorkspaceSourcePolicy::build_with_languages_cancellable(
                        &root,
                        effective_limits,
                        &visibility,
                        &text_inclusion,
                        &languages,
                        &cancelled,
                    )
                    .map_err(ReadFault::index)?;
                    let mut preparation = preparation;
                    let total = preparation
                        .discover(&visibility, &cancelled)
                        .map_err(ReadFault::index)?;
                    let map_paths = preparation
                        .discovered_map_paths()
                        .map_err(ReadFault::index)?;
                    Ok((
                        preparation,
                        Arc::new(policy),
                        total,
                        Arc::new(map_paths.source),
                        Arc::new(map_paths.text),
                        empty_index,
                    ))
                },
            )
            .await?;
    let initial = InitialWorkspacePreparation {
        preparation,
        source_policy,
        configuration: configuration.clone(),
        total,
        map_source_paths,
        map_text_paths,
        last: LastCapture::default(),
    };
    publish_initial_empty(context, initial, empty_index, configuration).await
}

async fn publish_initial_empty(
    context: &IndexSupervisorContext,
    initial: InitialWorkspacePreparation,
    empty_index: rift_index::WorkspaceIndex,
    configuration: ConfigurationState,
) -> Result<Option<InitialWorkspacePreparation>, ReadError> {
    let InitialWorkspacePreparation {
        preparation,
        source_policy,
        configuration: initial_configuration,
        total,
        map_source_paths,
        map_text_paths,
        last,
    } = initial;
    let root = context.root.clone();
    let state = Arc::clone(&context.published);
    let validation = Arc::clone(&context.validation);
    let source_policy_for_publish = Arc::clone(&source_policy);
    let map_source_paths_for_publish = Arc::clone(&map_source_paths);
    let map_text_paths_for_publish = Arc::clone(&map_text_paths);
    let lexical = context.lexical.clone();
    let cancellation = validation.cancellation.clone();
    let empty = context
        .blocking
        .run_with_cancellation(
            "initial index empty publication",
            cancellation,
            move |token| {
                let preparation_state = (total > 0).then(|| LocalIndexPreparation {
                    prepared: 0,
                    total: Some(total),
                    source_paths: Arc::new(Vec::new()),
                    other_paths: Arc::new(Vec::new()),
                    map_source_paths: Arc::clone(&map_source_paths_for_publish),
                    map_text_paths: Arc::clone(&map_text_paths_for_publish),
                });
                let candidate = preparation_publication(
                    &root,
                    empty_index,
                    source_policy_for_publish,
                    &configuration,
                    preparation_state,
                    validation.observed_epoch(),
                )?;
                if token.is_cancelled() {
                    return Err(ReadFault::cancelled());
                }
                let lexical = if total == 0 {
                    lexical.map(|lane| LexicalHandoff::new(lane, LexicalWrite::Whole))
                } else {
                    None
                };
                Ok(publish_preparation_after(
                    &root,
                    &state,
                    &validation,
                    &candidate,
                    lexical,
                ))
            },
        )
        .await?;
    if empty == RebuildOutcome::Cancelled {
        return Ok(None);
    }
    Ok(Some(InitialWorkspacePreparation {
        preparation,
        source_policy,
        configuration: initial_configuration,
        total,
        map_source_paths,
        map_text_paths,
        last,
    }))
}

/// Reads bounded batches from one discovery result, preserving its visibility policy.
#[cfg(test)]
pub(crate) async fn prepare_initial_workspace_from(
    context: &IndexSupervisorContext,
    initial: InitialWorkspacePreparation,
) -> Result<(), ReadError> {
    prepare_initial_workspace_from_with_epoch(context, initial)
        .await
        .map_err(|failure| failure.error)
}

async fn prepare_initial_workspace_from_with_epoch(
    context: &IndexSupervisorContext,
    initial: InitialWorkspacePreparation,
) -> Result<(), Box<InitialPreparationFailure>> {
    let result = prepare_initial_workspace_steps(context, initial).await;
    result.map_err(|error| Box::new(initial_preparation_failure(context, error)))
}

fn schedule_initial_rebuild(context: &IndexSupervisorContext) -> Result<Option<u64>, ReadError> {
    if context.validation.cancellation.is_cancelled() {
        return Ok(None);
    }
    context.validation.observe_whole_workspace().map(Some)
}

fn initial_preparation_failure(
    context: &IndexSupervisorContext,
    error: ReadError,
) -> InitialPreparationFailure {
    match schedule_initial_rebuild(context) {
        Ok(queued_epoch) => InitialPreparationFailure {
            error,
            queued_epoch,
        },
        Err(error) => InitialPreparationFailure {
            error,
            queued_epoch: None,
        },
    }
}

async fn prepare_initial_workspace_steps(
    context: &IndexSupervisorContext,
    mut initial: InitialWorkspacePreparation,
) -> Result<(), ReadError> {
    let validation = Arc::clone(&context.validation);
    let cancellation = validation.cancellation.clone();
    while let Some(target) = initial.preparation.next_checkpoint() {
        let (next_initial, outcome) =
            prepare_initial_workspace_batch(context, initial, target, cancellation.clone()).await?;
        initial = next_initial;
        if outcome == RebuildOutcome::Cancelled {
            return Ok(());
        }
    }
    let current = Arc::clone(&context.published.read().await.current);
    if current.preparation.is_none()
        && let Some(population) = &context.population
    {
        population.request(current);
    }
    Ok(())
}

async fn prepare_initial_workspace_batch(
    context: &IndexSupervisorContext,
    mut initial: InitialWorkspacePreparation,
    target: usize,
    cancellation: CancellationToken,
) -> Result<(InitialWorkspacePreparation, RebuildOutcome), ReadError> {
    let root = context.root.clone();
    let state = Arc::clone(&context.published);
    let validation = Arc::clone(&context.validation);
    let configuration = initial.configuration.clone();
    let source_policy = Arc::clone(&initial.source_policy);
    let previous_capture = std::mem::take(&mut initial.last);
    let complete = target == initial.total;
    let lexical = context.lexical.clone();
    let map_source_paths = Arc::clone(&initial.map_source_paths);
    let map_text_paths = Arc::clone(&initial.map_text_paths);
    let batch = InitialWorkspaceBatch {
        preparation: initial.preparation,
        target,
        complete,
        total: initial.total,
        root,
        state,
        validation,
        configuration,
        source_policy,
        previous_capture,
        map_source_paths,
        map_text_paths,
        lexical,
    };
    let (preparation, last, outcome) = context
        .blocking
        .run_with_cancellation("initial index preparation", cancellation, move |token| {
            prepare_initial_batch(token, batch)
        })
        .await?;
    initial.preparation = preparation;
    initial.last = last;
    Ok((initial, outcome))
}

struct InitialWorkspaceBatch {
    preparation: WorkspaceIndexPreparation,
    target: usize,
    complete: bool,
    total: usize,
    root: PathBuf,
    state: Arc<RwLock<IndexState>>,
    validation: Arc<IndexValidation>,
    configuration: ConfigurationState,
    source_policy: Arc<WorkspaceSourcePolicy>,
    previous_capture: LastCapture,
    map_source_paths: Arc<Vec<(ProjectPath, rift_protocol::read::Language)>>,
    map_text_paths: Arc<Vec<ProjectPath>>,
    lexical: Option<LexicalLane>,
}

fn prepare_initial_batch(
    token: &CancellationToken,
    batch: InitialWorkspaceBatch,
) -> Result<(WorkspaceIndexPreparation, LastCapture, RebuildOutcome), ReadError> {
    let cancelled = || token.is_cancelled();
    let mut preparation = batch.preparation;
    let current = batch.state.blocking_read().snapshot().0;
    let mut index = {
        let current = &current;
        if current.configuration.fingerprint == batch.configuration.fingerprint
            && current
                .source_policy
                .as_ref()
                .is_some_and(|policy| Arc::ptr_eq(policy, &batch.source_policy))
        {
            current
                .reads
                .advance_workspace_preparation(&mut preparation, batch.target, &cancelled)
        } else {
            preparation.advance_to(batch.target, &cancelled)
        }
    }
    .map_err(ReadFault::index)?;
    drop(current);
    let (captured, mut last) = preparation
        .capture_prepared_paths(&batch.previous_capture, &cancelled)
        .map_err(ReadFault::index)?;
    let changes = PathChanges::between(&index.digests(), &captured);
    if !changes.is_empty() {
        index = index
            .rebuilt_cancellable(&changes, &cancelled)
            .map_err(ReadFault::index)?;
        preparation.retain_rebuilt_snapshot(&index);
        let (verified, refreshed_last) = preparation
            .capture_prepared_paths(&last, &cancelled)
            .map_err(ReadFault::index)?;
        if !PathChanges::between(&index.digests(), &verified).is_empty() {
            return Err(ReadFault::unavailable(
                "initial index preparation",
                "a selected file kept changing during its capture",
            ));
        }
        last = refreshed_last;
    }
    let (source_paths, other_paths) = preparation
        .prepared_path_classes()
        .map_err(ReadFault::index)?;
    let candidate_state = (!batch.complete).then(|| LocalIndexPreparation {
        prepared: preparation.prepared(),
        total: Some(batch.total),
        source_paths: Arc::new(source_paths),
        other_paths: Arc::new(other_paths),
        map_source_paths: batch.map_source_paths,
        map_text_paths: batch.map_text_paths,
    });
    let candidate = preparation_publication(
        &batch.root,
        index,
        batch.source_policy,
        &batch.configuration,
        candidate_state,
        batch.validation.observed_epoch(),
    )?;
    let handoff = if batch.complete {
        batch
            .lexical
            .map(|lane| LexicalHandoff::new(lane, LexicalWrite::Whole))
    } else {
        None
    };
    let outcome = publish_preparation_after(
        &batch.root,
        &batch.state,
        &batch.validation,
        &candidate,
        handoff,
    );
    Ok((preparation, last, outcome))
}

fn preparation_publication(
    root: &Path,
    index: rift_index::WorkspaceIndex,
    source_policy: Arc<WorkspaceSourcePolicy>,
    configuration: &ConfigurationState,
    preparation: Option<LocalIndexPreparation>,
    epoch: u64,
) -> Result<Arc<PublishedWorkspace>, ReadError> {
    let context = match &preparation {
        Some(_) => DependencyContext::default(),
        None => ReadService::dependency_context_for_policy(
            root,
            &source_policy,
            &configuration.dependencies_configuration(),
        )?,
    };
    let reads = ReadService::from_prepared_index(
        index,
        Some(Arc::clone(&source_policy)),
        Arc::new(context),
        configuration.history_configuration(),
        configuration.dependencies_configuration(),
    );
    let visible_files = if preparation.is_some() {
        VisibleWorkspaceFiles {
            digests: Arc::new(reads.content_digests()),
            refused: Arc::new(BTreeMap::new()),
        }
    } else {
        complete_visible_digests(root, &reads, &source_policy)?
    };
    let map = publication_map(&reads, preparation.as_ref());
    Ok(Arc::new(PublishedWorkspace {
        fingerprint: reads.workspace_fingerprint().clone(),
        reads: Arc::new(reads),
        configuration: configuration.clone(),
        source_policy: Some(source_policy),
        preparation,
        visible_digests: visible_files.digests,
        visible_refused: visible_files.refused,
        map,
        epoch,
    }))
}

fn publish_preparation_after(
    root: &Path,
    published: &RwLock<IndexState>,
    validation: &IndexValidation,
    candidate: &Arc<PublishedWorkspace>,
    lexical: Option<LexicalHandoff>,
) -> RebuildOutcome {
    let pending = validation.locked_pending();
    let observed_epoch = validation.observed_epoch();
    if pending.covers_whole_workspace()
        || candidate.configuration.fingerprint != configuration_fingerprint(root)
    {
        drop(pending);
        return RebuildOutcome::Superseded;
    }
    let answer = answered_candidate(root, candidate, &pending, observed_epoch)
        .published_at_startup(candidate);
    let Some(answer) = answer else {
        drop(pending);
        return RebuildOutcome::Superseded;
    };
    let mut state = published.blocking_write();
    if state.current.preparation.is_none() || answer.epoch < state.current.epoch {
        drop(state);
        drop(pending);
        return RebuildOutcome::Superseded;
    }
    let previous = Arc::clone(&state.current);
    if !state.publish(Arc::clone(&answer), answer.epoch) {
        drop(state);
        drop(pending);
        return RebuildOutcome::Superseded;
    }
    validation.replace_publication_locked(&answer);
    if let Some(engines) = validation.engines.get() {
        engines.owe_publication(&previous, &answer, &ChangeSet::Full);
    }
    if let Some(handoff) = lexical {
        handoff.hand_over(Arc::clone(&answer));
    }
    drop(state);
    drop(pending);
    validation.changed.notify_waiters();
    if answer.preparation.is_none() {
        let published_epoch = answer.epoch;
        tracing::info!(
            component = "index",
            operation = "index.publish",
            trigger = "startup",
            epoch = published_epoch,
            "index snapshot published"
        );
    }
    RebuildOutcome::Published
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
    let (mut background, mut version_control) = match background::start(&context).await {
        Ok(Some(scheduling)) => scheduling,
        Ok(None) => return,
        Err(error) => {
            if !validation.cancellation.is_cancelled() {
                publish_rebuild_failure(&context, validation.observed_epoch(), error).await;
            }
            return;
        }
    };
    if let Err(failure) = prepare_initial_workspace_with_epoch(&context).await
        && let Some(epoch) = failure.queued_epoch
    {
        publish_rebuild_failure(&context, epoch, failure.error).await;
    }
    loop {
        let Some(trigger) = background
            .next(&mut invalidations, &validation.cancellation)
            .await
        else {
            return;
        };
        tokio::select! {
            () = validation.cancellation.cancelled() => return,
            () = tokio::time::sleep(INDEX_DEBOUNCE) => {}
        }
        match version_control.wait(&context).await {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                publish_rebuild_failure(&context, validation.observed_epoch(), error).await;
                continue;
            }
        }
        if trigger == background::Trigger::Validation
            && let Err(error) = background.validate(&context).await
        {
            if validation.cancellation.is_cancelled() {
                return;
            }
            publish_rebuild_failure(&context, validation.observed_epoch(), error).await;
            continue;
        }
        // The bounded signal may remain after a timer took the work it announced.
        // Drain before taking pending work, so a later observation retains its signal.
        let _ = invalidations.try_recv();
        let pending_is_empty = {
            let pending = validation.locked_pending();
            !pending.covers_whole_workspace() && pending.paths.is_empty()
        };
        if pending_is_empty
            && validation.observed_epoch() <= published.read().await.snapshot().0.epoch
        {
            continue;
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
/// A superseded candidate hands nothing to the lane. A successful capture superseded at
/// publication records its epoch and wakes reads, so a read captures the tree again and
/// answers stale when it moved since that read's previous capture, while changes keep
/// waiting for a current publication.
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
    let captured = tokio::select! {
        () = context.validation.cancellation.cancelled() => return Ok(RebuildOutcome::Cancelled),
        captured = context.blocking.run("filesystem index rebuild", move || {
            capture_rebuild_with(
                &root,
                limits,
                &captured_state,
                &captured_validation,
                request,
                capture,
            )
        }) => captured?,
    };
    match captured {
        CapturedRebuild::Candidate {
            published,
            change_set,
            write,
            work,
        } => {
            let lexical = context
                .lexical
                .clone()
                .map(|lane| LexicalHandoff::new(lane, write));
            let outcome = publish_captured(context, published, change_set, work, lexical).await?;
            if outcome == RebuildOutcome::Published {
                trace_publication(epoch);
            }
            Ok(outcome)
        }
        CapturedRebuild::Unchanged { published, work } => {
            let unchanged = ChangeSet::Incremental(PathChanges::default());
            let outcome = publish_captured(context, published, unchanged, work, None).await?;
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
    change_set: ChangeSet,
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
                &change_set,
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
        /// What the rebuild covers against the publication it captured from, which the
        /// engine feed hands on.
        change_set: ChangeSet,
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
            if request.cancellation.is_cancelled() || validation.cancellation.is_cancelled() {
                return Ok(CapturedRebuild::Cancelled);
            }
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
        change_set,
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
    change_set: &ChangeSet,
    work: PendingWork,
    lexical: Option<LexicalHandoff>,
) -> RebuildOutcome {
    let outcome = publish_rebuild_after(
        root,
        published,
        validation,
        candidate,
        change_set,
        lexical,
        || {},
    );
    if outcome == RebuildOutcome::Published {
        validation.changed.notify_waiters();
    } else {
        validation.restore_pending(work);
        if outcome == RebuildOutcome::Superseded {
            validation
                .superseded_epoch
                .fetch_max(candidate.epoch, Ordering::SeqCst);
            validation.changed.notify_waiters();
        }
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
    publish_rebuild_after(
        root,
        published,
        validation,
        candidate,
        &ChangeSet::Full,
        None,
        || {},
    )
}

/// Publishes under observation lane; hook enables deterministic overlap tests.
///
/// A published candidate's lexical write is handed to the lane before the lane's lock
/// releases, so two publications hand their writes over in the order they published.
/// Its `change_set`, the rebuild's own against the publication it captured from, reaches
/// the engine hold under the same locks.
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
    change_set: &ChangeSet,
    lexical: Option<LexicalHandoff>,
    after_state_lock: impl FnOnce(),
) -> RebuildOutcome {
    let publication = validation.locked_pending();
    let observed_epoch = validation.observed_epoch();
    let candidate =
        answered_candidate(root, candidate, &publication, observed_epoch).into_current();
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
    let previous = Arc::clone(&state.current);
    // IndexState::publish owns the still-current check, so a superseded
    // candidate is rejected in exactly one place.
    let published = state.publish(candidate, observed_epoch);
    if published {
        validation.replace_publication_locked(&publishing);
        // Under the state lock, so no request reads this publication before its
        // engines owe its changed files (engine_read.rs opens and maps index bytes).
        if let Some(engines) = validation.engines.get() {
            engines.owe_publication(&previous, &publishing, change_set);
        }
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

/// Where one captured candidate stands against the observation made since its capture.
#[derive(Debug)]
enum CandidateAnswer {
    /// The candidate answers the observed epoch: as itself when nothing was observed since
    /// its capture, or as its twin under that epoch when it already holds what every
    /// observed path holds.
    Current(Arc<PublishedWorkspace>),
    /// The observation names a path whose bytes the candidate does not hold, or could not
    /// read under its own policy; a rebuild reading the named paths answers it.
    Behind,
    /// The observation asks for the whole workspace, names a path the candidate holds
    /// files below that is gone from the disk, or was made while `rift.toml` moved since
    /// the candidate accepted it; only a capture of the whole workspace answers it.
    Rescan,
}

impl CandidateAnswer {
    /// The publication a running supervisor makes: the candidate answers the observation,
    /// or it is superseded.
    fn into_current(self) -> Option<Arc<PublishedWorkspace>> {
        match self {
            Self::Current(answering) => Some(answering),
            Self::Behind | Self::Rescan => None,
        }
    }

    /// The publication startup makes, or nothing when the workspace has to be captured
    /// again.
    ///
    /// A candidate behind the observation still publishes, under the epoch its own capture
    /// took. The paths it does not hold stay the publication lane's pending work, so the
    /// supervisor's first rebuild reads them as an incremental change, through the same
    /// lexical lane and epoch sequence as every later change, and a read meets the startup
    /// snapshot first and that change right after.
    fn published_at_startup(
        self,
        candidate: &Arc<PublishedWorkspace>,
    ) -> Option<Arc<PublishedWorkspace>> {
        match self {
            Self::Current(answering) => Some(answering),
            Self::Behind => Some(Arc::clone(candidate)),
            Self::Rescan => None,
        }
    }
}

/// Where `candidate` stands against the observation `pending` holds under
/// `observed_epoch`.
///
/// A candidate answers the epoch its capture took. When the observation moved while the
/// capture ran, the paths that moved are still the lane's pending work, so the same
/// comparison a rebuild makes - [`PathChanges::resolve`] against the candidate's own
/// digests - says whether the candidate already holds what they name. A candidate that
/// holds all of them answers the later observation too, as its twin under that epoch: a
/// change's own write reaches the watcher after the change captured it, and reading the
/// same bytes a second time is the work this drops. An observation that names a path the
/// candidate does not hold leaves it behind; one that asks for the whole workspace, one
/// naming a path the candidate holds files below that is gone from the disk, or one made
/// while `rift.toml` moved, asks for a rescan.
///
/// Work is one digest per observed path, bounded by how many paths one observation
/// retains, and it runs under the publication lane alone.
fn answered_candidate(
    root: &Path,
    candidate: &Arc<PublishedWorkspace>,
    pending: &PendingWork,
    observed_epoch: u64,
) -> CandidateAnswer {
    if candidate.epoch == observed_epoch {
        return CandidateAnswer::Current(Arc::clone(candidate));
    }
    if pending.covers_whole_workspace()
        || candidate.configuration.fingerprint != configuration_fingerprint(root)
        || candidate.holds_files_below_a_gone_path(root, &pending.paths)
    {
        return CandidateAnswer::Rescan;
    }
    if !candidate.holds_observed(root, &pending.paths) {
        return CandidateAnswer::Behind;
    }
    let twin = candidate.under(candidate.configuration.clone(), observed_epoch);
    CandidateAnswer::Current(Arc::new(twin))
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

    use rift_index::{LexicalChange, LexicalStamp, TrigramBatch, WorkspaceDigests};
    use rift_search::{SearchError, SearchFault, SearchIndex, SearchViolation};
    use tokio::sync::{Semaphore, watch};

    use super::{LexicalLaneBounds, LexicalStore};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Polls one lane pass a test waits on before it gives up: three seconds, at
    /// [`LANE_POLL`] each.
    pub(crate) const LANE_ATTEMPTS_MAX: usize = 60;
    /// Longest a test waits for the lexical lane, this double, or the supervisor to reach
    /// a state it expects.
    ///
    /// Each wait ends as soon as the state holds, so a passing test never spends this
    /// bound: it only ends a wait for a state that never comes, with that wait's own
    /// message, well inside nextest's one-minute deadline. The slowest of these tests took
    /// 2.4 s from start to end on the slowest CI runner, macos-15-intel, so a runner three
    /// times slower still ends every wait inside half of it.
    pub(crate) const LANE_WAIT_MAX: Duration = Duration::from_secs(15);
    /// Wait between two reads of a store the lane has not stamped yet.
    pub(crate) const LANE_POLL: Duration = Duration::from_millis(50);
    /// Bounds that never leave a unit out and never split a write into parts.
    pub(crate) const UNBOUNDED: LexicalLaneBounds = LexicalLaneBounds {
        unit_bytes_max: usize::MAX,
        transaction_units_max: usize::MAX,
        transaction_bytes_max: usize::MAX,
    };
    /// The product version a test lane derives its rows under.
    pub(crate) const PRODUCT_VERSION: &str = "0.0.45";

    /// A lexical store a test steers: every write records what it was asked and waits for
    /// one permit before it answers, refusing changes, recorded-digest reads, or trigram
    /// batches when told to, and writing through to the attached index when one is held. A write whose future is dropped while the
    /// store still holds it is counted, so a test can prove an abort reached it.
    ///
    /// Trigram batches pass through to the attached index without a permit, and wait at a
    /// gate of their own while a test holds them.
    pub(crate) struct StoreDouble {
        permits: Semaphore,
        calls: Mutex<Vec<(&'static str, String)>>,
        applied: Mutex<Vec<Vec<rift_core::ProjectPath>>>,
        refuse_changes: AtomicBool,
        refuse_reads: AtomicBool,
        refuse_trigrams: AtomicBool,
        inner: Mutex<Option<Arc<SearchIndex>>>,
        dropped_while_held: AtomicUsize,
        trigram_gate: watch::Sender<TrigramGate>,
        trigram_arrivals: watch::Sender<usize>,
        trigram_batches: AtomicUsize,
    }

    /// Which trigram batches wait at the double's gate, and how many held ones may pass.
    #[derive(Clone, Copy, Debug, Default)]
    struct TrigramGate {
        /// Batches arriving once the store took at least this many writes wait; `None`
        /// lets every batch through.
        held_after_writes: Option<usize>,
        /// Held batches that may still pass.
        permits: usize,
    }

    impl TrigramGate {
        /// Whether a batch arriving after `writes` writes waits at the gate.
        fn holds(self, writes: usize) -> bool {
            self.held_after_writes.is_some_and(|after| writes >= after)
        }

        /// Whether such a batch may go on now.
        fn passes(self, writes: usize) -> bool {
            !self.holds(writes) || self.permits > 0
        }

        /// Spends a permit on such a batch, when it was held.
        fn spend(&mut self, writes: usize) {
            if self.holds(writes) {
                self.permits = self.permits.saturating_sub(1);
            }
        }
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
                applied: Mutex::new(Vec::new()),
                refuse_changes: AtomicBool::new(false),
                refuse_reads: AtomicBool::new(false),
                refuse_trigrams: AtomicBool::new(false),
                inner: Mutex::new(None),
                dropped_while_held: AtomicUsize::new(0),
                trigram_gate: watch::Sender::new(TrigramGate::default()),
                trigram_arrivals: watch::Sender::new(0),
                trigram_batches: AtomicUsize::new(0),
            })
        }

        /// Holds every trigram batch at its gate from now on, until
        /// [`Self::release_trigrams`] or [`Self::release_trigram_batch`].
        pub(crate) fn hold_trigrams(&self) {
            self.hold_trigrams_after_writes(0);
        }

        /// Holds every trigram batch arriving once the store took `writes` writes, so the
        /// batch a lane runs before its first write passes whenever it lands.
        pub(crate) fn hold_trigrams_after_writes(&self, writes: usize) {
            self.trigram_gate.send_replace(TrigramGate {
                held_after_writes: Some(writes),
                permits: 0,
            });
        }

        /// Lets exactly one held trigram batch, waiting or still to come, proceed.
        pub(crate) fn release_trigram_batch(&self) {
            self.trigram_gate.send_modify(|gate| gate.permits += 1);
        }

        /// Lets every held and later trigram batch proceed.
        pub(crate) fn release_trigrams(&self) {
            self.trigram_gate.send_replace(TrigramGate::default());
        }

        /// How many trigram batches reached the gate, held or not.
        pub(crate) fn trigram_arrivals(&self) -> usize {
            *self.trigram_arrivals.borrow()
        }

        /// Waits until `count` trigram batches reached the gate, held or not.
        pub(crate) async fn trigram_arrivals_reach(&self, count: usize) -> TestResult {
            let mut arrivals = self.trigram_arrivals.subscribe();
            arrivals
                .wait_for(|arrived| *arrived >= count)
                .await
                .map(drop)?;
            Ok(())
        }

        /// Refuses every trigram batch past the gate from now on, as a store that failed
        /// would.
        pub(crate) fn refuse_trigrams(&self) {
            self.refuse_trigrams.store(true, Ordering::SeqCst);
        }

        /// How many trigram batches passed the gate.
        pub(crate) fn trigram_batches(&self) -> usize {
            self.trigram_batches.load(Ordering::SeqCst)
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

        /// Accepts change writes again, as a store that recovered would.
        pub(crate) fn accept_changes(&self) {
            self.refuse_changes.store(false, Ordering::SeqCst);
        }

        /// Refuses every recorded-digest read until [`Self::accept_reads`].
        pub(crate) fn refuse_reads(&self) {
            self.refuse_reads.store(true, Ordering::SeqCst);
        }

        /// Answers recorded-digest reads again, as a store that recovered would.
        pub(crate) fn accept_reads(&self) {
            self.refuse_reads.store(false, Ordering::SeqCst);
        }

        /// The paths each write asked of the store replaced, in the order asked.
        pub(crate) fn applied(&self) -> Vec<Vec<rift_core::ProjectPath>> {
            self.applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
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
        async fn recorded(
            &self,
            derivation_revision: &str,
        ) -> Result<Option<WorkspaceDigests>, SearchError> {
            if self.refuse_reads.load(Ordering::SeqCst) {
                return Err(SearchError::new(
                    SearchFault::new(SearchViolation::StoreFailed)
                        .about("the double refuses reads"),
                ));
            }
            match self.attached() {
                Some(index) => index.recorded_lexical_files(derivation_revision).await,
                None => Ok(None),
            }
        }

        async fn clear(&self, derivation_revision: &str) -> Result<(), SearchError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(("clear", String::new()));
            match self.attached() {
                Some(index) => index.clear_lexical(derivation_revision).await,
                None => Ok(()),
            }
        }

        async fn apply(
            &self,
            change: &LexicalChange,
            stamp: &LexicalStamp,
            documentation: Option<&rift_index::DocumentationCollection>,
        ) -> Result<(), SearchError> {
            let through = async {
                if self.refuse_changes.load(Ordering::SeqCst) {
                    return Err(SearchError::new(
                        SearchFault::new(SearchViolation::StoreFailed)
                            .about("the double refuses changes"),
                    ));
                }
                match self.attached() {
                    Some(index) => index.apply(change, stamp, documentation).await,
                    None => Ok(()),
                }
            };
            self.applied
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(change.replaced().to_vec());
            let tree_revision = stamp.tree_revision().unwrap_or_default();
            self.write("apply", tree_revision, through).await
        }

        async fn index_trigrams(&self) -> Result<TrigramBatch, SearchError> {
            self.trigram_arrivals.send_modify(|arrived| *arrived += 1);
            let writes = self.applied().len();
            self.trigram_gate
                .subscribe()
                .wait_for(|gate| gate.passes(writes))
                .await
                .map(drop)
                .map_err(|_| SearchError::new(SearchFault::new(SearchViolation::StoreFailed)))?;
            self.trigram_gate.send_modify(|gate| gate.spend(writes));
            if self.refuse_trigrams.load(Ordering::SeqCst) {
                return Err(SearchError::new(
                    SearchFault::new(SearchViolation::StoreFailed)
                        .about("the double refuses trigram batches"),
                ));
            }
            self.trigram_batches.fetch_add(1, Ordering::SeqCst);
            match self.attached() {
                Some(index) => index.index_trigrams().await,
                None => Ok(TrigramBatch::default()),
            }
        }
    }
}

impl ConfigurationState {
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
}

#[cfg(test)]
pub(crate) mod tests {
    use super::ConfigurationState;
    use rift_core::{ProjectPath, SourceVisibility};
    use rift_server::LspProcessKey;
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

    use std::collections::{BTreeMap, BTreeSet};
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;
    use std::sync::Barrier as ThreadBarrier;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    use notify::{Event, EventKind};
    use rift_core::SymbolId;
    use rift_index::{LexicalChange, LexicalIndexLimits, WorkspaceIndexLimits};
    use rift_protocol::configuration::{GlobalConfiguration, ServerConfiguration};
    use rift_protocol::map::WorkspaceMap;
    use rift_ranking::{
        DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
        ParsedQuery, Pattern, QueryPhase, RankingInput, SearchableField,
    };
    use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits, VectorReadiness};
    use rift_server::ReadFault;
    use tokio::sync::{Barrier as AsyncBarrier, RwLock};
    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::lexical_double::{LANE_ATTEMPTS_MAX, LANE_POLL, LANE_WAIT_MAX, StoreDouble};
    use super::{
        BlockingExecutor, ChangeSet, ConfigurationFingerprint, IndexState, IndexValidation,
        LEXICAL_COMMIT_TIMEOUT, LEXICAL_COMMIT_TIMEOUT_MAX, LEXICAL_HELD_PATHS_MAX,
        LEXICAL_HELD_REVISIONS_MAX, LEXICAL_UNIT_COMMIT_BUDGET, LexicalCommitState, LexicalLane,
        LexicalWrite, PathChanges, PopulationLane, PublishedWorkspace, RebuildOutcome,
        RebuildRequest, WorkspaceCandidate, build_workspace_candidate, commit_deadline,
        publish_rebuild, publish_rebuild_after, record_rebuild_failure, unwatched,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    #[test]
    fn global_configuration_accessor_keeps_defaults_when_acceptance_fails() -> TestResult {
        let directory = tempfile::tempdir()?;
        let defaults = GlobalConfiguration::default();

        let missing = ConfigurationState::accept(directory.path());
        assert_eq!(missing.global_configuration(), defaults);

        fs::write(
            directory.path().join("rift.toml"),
            "[global]\nenabled = false\nattempts = 5\n",
        )?;
        let accepted = ConfigurationState::accept(directory.path());
        let mut expected = defaults.clone();
        expected.enabled = false;
        expected.attempts = 5;
        assert_eq!(accepted.global_configuration(), expected);

        fs::write(
            directory.path().join("rift.toml"),
            "[global]\nattempts = 6\n",
        )?;
        let invalid = ConfigurationState::accept(directory.path());
        assert!(invalid.accepted.is_err());
        assert_eq!(invalid.global_configuration(), defaults);
        Ok(())
    }

    fn stable_candidate(root: &std::path::Path, epoch: u64) -> TestResult<Arc<PublishedWorkspace>> {
        candidate_with_limits(root, epoch, WorkspaceIndexLimits::default())
    }

    fn initial_preparation_context(
        root: &std::path::Path,
    ) -> TestResult<(
        super::IndexSupervisorContext,
        tokio::sync::mpsc::Receiver<()>,
    )> {
        let limits = WorkspaceIndexLimits::default();
        let (validation, invalidations) = IndexValidation::new(limits.files_max());
        let (empty, preparation) = super::empty_workspace_preparation(
            root,
            limits,
            ConfigurationState::accept(root),
            0,
            rift_index::WorkspaceContentCache::default(),
        )?;
        validation.prepare_initial(preparation);
        let published = Arc::new(RwLock::new(IndexState {
            current: empty,
            failure: None,
        }));
        Ok((
            super::IndexSupervisorContext {
                root: root.to_path_buf(),
                limits,
                published,
                validation,
                blocking: BlockingExecutor::isolated(2, 60_000),
                population: None,
                lexical: None,
            },
            invalidations,
        ))
    }

    fn candidate_with_limits(
        root: &std::path::Path,
        epoch: u64,
        limits: WorkspaceIndexLimits,
    ) -> TestResult<Arc<PublishedWorkspace>> {
        match build_workspace_candidate(root, limits, &RebuildRequest::initial(epoch))? {
            WorkspaceCandidate::Stable { published, .. } => Ok(published),
            WorkspaceCandidate::ConfigurationChanged => {
                Err("fixture configuration must remain stable".into())
            }
        }
    }

    #[tokio::test]
    async fn later_watch_epoch_rejects_queued_initial_preparation_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("a.rs"), "pub fn first() {}\n")?;
        fs::write(directory.path().join("b.rs"), "pub fn second() {}\n")?;
        let (context, _invalidations) = initial_preparation_context(directory.path())?;
        let initial = super::discover_initial_workspace(&context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        fs::remove_file(directory.path().join("b.rs"))?;

        let failure = super::prepare_initial_workspace_from_with_epoch(&context, initial)
            .await
            .expect_err("a missing discovered path must refuse its preparation batch");
        let queued_epoch = failure
            .queued_epoch
            .ok_or("preparation refusal must queue a full rebuild")?;
        assert_eq!(context.validation.observed_epoch(), queued_epoch);

        let later_epoch = context
            .validation
            .observe_paths([rift_core::ProjectPath::new("a.rs")?])?;
        assert_eq!(later_epoch, queued_epoch + 1);
        let published = Arc::clone(&context.published);
        let validation = Arc::clone(&context.validation);
        let error = failure.error;
        let recorded = tokio::task::spawn_blocking(move || {
            record_rebuild_failure(&published, &validation, queued_epoch, error)
        })
        .await?;
        assert!(
            !recorded,
            "failure for the queued startup epoch cannot replace later watcher state"
        );
        assert!(context.published.read().await.failure.is_none());
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
                super::watch_event_impact(
                    &super::WatchRoots::resolve(&watched_root)?,
                    &validation,
                    &event(kind, path)
                ),
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
        match (&actual.source_policy, &expected.source_policy) {
            (Some(actual), Some(expected)) => assert!(Arc::ptr_eq(actual, expected)),
            (None, None) => {}
            _ => panic!("publications must carry the same source policy state"),
        }
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
                &ChangeSet::Full,
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
    fn oversized_candidate_acknowledges_its_late_file_event() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = rift_core::ProjectPath::new("lib.rs")?;
        let absolute = directory.path().join(path.as_str());
        let limits = WorkspaceIndexLimits::new(4, 60, 60, 4, 100)?;
        fs::write(
            directory.path().join("rift.toml"),
            "[providers.syntax]\nmax_file = \"60b\"\n\n[search.text]\nlarge_files = \"skip\"\n",
        )?;
        fs::write(&absolute, "pub fn beacon() {}\n")?;
        let before = candidate_with_limits(directory.path(), 0, limits)?;
        let (validation, _invalidations) = IndexValidation::new(limits.files_max());
        validation.install_publication(&before);
        fs::write(&absolute, "// oversized source\n".repeat(8))?;
        let epoch = validation.observe_paths([path.clone()])?;
        let _work = validation.take_pending();
        let candidate = candidate_with_limits(directory.path(), epoch, limits)?;
        assert!(candidate.reads.file_digest(&path).is_none());
        assert!(
            candidate
                .source_policy
                .as_ref()
                .expect("candidate carries accepted source policy")
                .visible_digest(&absolute)
                .is_err()
        );
        let state = RwLock::new(IndexState {
            current: before,
            failure: None,
        });
        let late_epoch = validation.observe_paths([path.clone()])?;
        assert_eq!(
            publish_rebuild(directory.path(), &state, &validation, &candidate),
            RebuildOutcome::Published,
            "a late observation of the same oversized file must not reject its captured omission"
        );
        let (published, _) = state.blocking_read().snapshot();
        assert_eq!(published.epoch, late_epoch);
        let params = serde_json::from_value(serde_json::json!({"name": "beacon"}))?;
        let answer = published.reads.get_symbol(&params)?;
        assert!(answer.hits.is_empty());
        assert!(answer.warnings.iter().any(|warning| matches!(
            warning,
            rift_protocol::read::ReadWarning::SourceUnavailable { detail, .. }
                if detail.contains("file byte limit")
        )));

        fs::write(&absolute, "pub fn changed() {}\n")?;
        validation.observe_paths([path])?;
        assert_eq!(
            publish_rebuild(directory.path(), &state, &validation, &published),
            RebuildOutcome::Superseded,
            "new bounded source must supersede the old omission"
        );
        fs::remove_file(&absolute)?;
        // A link to itself fails every read with a loop error: an I/O failure that is
        // neither a missing file nor a directory, which both hold no file.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&absolute, &absolute)?;
            assert!(
                published
                    .source_policy
                    .as_ref()
                    .expect("publication carries accepted source policy")
                    .visible_digest(&absolute)
                    .is_err()
            );
            assert_eq!(
                publish_rebuild(directory.path(), &state, &validation, &published),
                RebuildOutcome::Superseded,
                "an I/O failure must not be accepted as the earlier file bound"
            );
        }
        Ok(())
    }

    #[test]
    fn oversized_observation_requires_the_same_recorded_omission() -> TestResult {
        for original in [
            None,
            Some(b"source\0bytes".as_slice()),
            Some(b"pub fn beacon() {}\n".as_slice()),
        ] {
            let directory = tempfile::tempdir()?;
            let path = rift_core::ProjectPath::new("lib.rs")?;
            let absolute = directory.path().join(path.as_str());
            if let Some(bytes) = original {
                fs::write(&absolute, bytes)?;
            }
            let limits = WorkspaceIndexLimits::new(4, 60, 60, 4, 100)?;
            let candidate = candidate_with_limits(directory.path(), 0, limits)?;
            let (validation, _invalidations) = IndexValidation::new(limits.files_max());
            validation.install_publication(&candidate);
            let state = RwLock::new(IndexState {
                current: Arc::clone(&candidate),
                failure: None,
            });
            fs::write(&absolute, "// oversized source\n".repeat(8))?;
            validation.observe_paths([path])?;
            assert_eq!(
                publish_rebuild(directory.path(), &state, &validation, &candidate),
                RebuildOutcome::Superseded,
                "the candidate must carry the same file omission it acknowledges"
            );
        }
        Ok(())
    }

    #[test]
    fn watch_backend_failure_marks_the_watch_unhealthy() {
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let root = std::path::Path::new("/rift-workspace");
        super::report_watch_outcome(
            &super::WatchRoots::at(root),
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
        super::report_watch_outcome(&super::WatchRoots::at(root), &validation, Ok(event));
        assert!(validation.watch_failed.load(Ordering::Acquire));
    }

    #[test]
    fn pathless_rescan_event_schedules_a_whole_workspace_rebuild() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let event = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let roots = super::WatchRoots::resolve(&root)?;

        assert_eq!(
            super::watch_event_impact(&roots, &validation, &event),
            super::WatchImpact::WholeWorkspace,
            "rescan says event history may have gaps, even when event has no paths"
        );
        assert_eq!(validation.observe_event(&roots, &event)?, Some(1));
        let pending = validation.take_pending();
        assert!(
            pending.work.covers_whole_workspace(),
            "pathless rescan must retain whole-workspace recovery"
        );
        Ok(())
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
        // The watch is registered under one spelling and the events arrive in another,
        // which is what `WatchRoots` maps between before the floor reads a path.
        let roots = super::WatchRoots::resolve(&watched_root)?;
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));

        assert_eq!(
            super::watch_event_impact(
                &roots,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), "src/lib.rs")
            ),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new("src/lib.rs")?]),
            "an edited source file names itself, so only it is read again"
        );
        assert_eq!(
            super::watch_event_impact(
                &roots,
                &validation,
                &event(EventKind::Modify(ModifyKind::Any), ".gitignore")
            ),
            super::WatchImpact::WholeWorkspace,
            "a written ignore file decides what the workspace includes"
        );
        assert_eq!(
            super::watch_event_impact(
                &roots,
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
                &roots,
                &validation,
                &event(EventKind::Remove(RemoveKind::Folder), "src")
            ),
            super::WatchImpact::WholeWorkspace,
            "a directory that disappears takes an unknown set of files with it"
        );
        Ok(())
    }

    /// Before the first publication no policy decides what an event path holds, so a file
    /// event names its own path in the spelling the index keys files by, and the startup
    /// candidate's policy decides what it holds. An event that rewrites what the workspace
    /// includes or reshapes the tree asks for the whole workspace, as it does after that
    /// publication.
    #[test]
    fn an_event_before_the_first_publication_names_its_own_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        let watched_root = directory.path().join(".");
        let event_root = directory.path().canonicalize()?;
        fs::create_dir_all(directory.path().join("src/nested"))?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let roots = super::WatchRoots::resolve(&watched_root)?;
        let event = |kind, path: &str| Event::new(kind).add_path(event_root.join(path));
        let named = |path: &str| -> TestResult<super::WatchImpact> {
            Ok(super::WatchImpact::Paths(vec![
                rift_core::ProjectPath::new(path)?,
            ]))
        };
        let renamed = EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::Any));
        let expectations: Vec<(EventKind, &str, super::WatchImpact, &str)> = vec![
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                "src/lib.rs",
                named("src/lib.rs")?,
                "an edited file names itself",
            ),
            (
                EventKind::Create(CreateKind::File),
                "src/generated/code.rs",
                named("src/generated/code.rs")?,
                "a created file names itself, whatever policy later excludes it",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                "rift.toml",
                named("rift.toml")?,
                "a written configuration file names itself, and the candidate's acceptance \
                 decides whether the scan answers it",
            ),
            (
                EventKind::Modify(ModifyKind::Any),
                ".gitignore",
                super::WatchImpact::WholeWorkspace,
                "the workspace's ignore file decides what is included",
            ),
            (
                EventKind::Create(CreateKind::Folder),
                "src/fresh",
                super::WatchImpact::WholeWorkspace,
                "a directory that appears brings an unknown set of files with it",
            ),
            (
                renamed,
                "src/nested",
                super::WatchImpact::WholeWorkspace,
                "a directory renamed into place holds files no event named",
            ),
            (
                renamed,
                "src/departed",
                named("src/departed")?,
                "a path renamed away names itself, and the startup candidate decides whether \
                 it held files below it",
            ),
            (
                renamed,
                "src/departed.rs",
                named("src/departed.rs")?,
                "a file renamed away names itself",
            ),
        ];
        for (kind, path, expected, reason) in expectations {
            assert_eq!(
                super::watch_event_impact(&roots, &validation, &event(kind, path)),
                expected,
                "{path}: {reason}"
            );
        }
        Ok(())
    }

    /// A server does not index its own state. The walk's hard floor prunes `.rift` by
    /// name before either lane sees it, so neither the syntax-indexed files nor the
    /// recorded set holds the workspace database, whose bytes move for as long as SQLite
    /// runs.
    #[test]
    fn the_index_holds_no_path_under_the_state_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let state = directory
            .path()
            .join(rift_core::constants::RIFT_STATE_DIRECTORY);
        fs::create_dir_all(&state)?;
        for name in ["db", "db-shm", "db-wal", "server.json"] {
            fs::write(state.join(name), "state\n")?;
        }

        let candidate = super::build_workspace_candidate(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &super::RebuildRequest::initial(0),
        )?;
        let WorkspaceCandidate::Stable { published, .. } = candidate else {
            return Err("the fixture capture must be stable".into());
        };

        let digests = published.reads.workspace_digests();
        let held: Vec<&str> = digests.iter().map(|(path, _)| path.as_str()).collect();
        assert!(
            held.iter()
                .all(|path| !path.starts_with(rift_core::constants::RIFT_STATE_DIRECTORY)),
            "the recorded set holds no state file: {held:?}"
        );
        assert!(held.contains(&"lib.rs"), "{held:?}");
        Ok(())
    }

    /// A server does not observe its own state either, under any spelling of its root.
    ///
    /// No publication is installed, which is the startup window the initial build runs
    /// in: `source_path_is_relevant` is wide open there, so every path the floor admits
    /// names itself. The floor is the only guard left, and it places the event path
    /// against both spellings before reading it, so the database writes that run for the
    /// life of the server move no filesystem epoch. The last assertion is what makes
    /// dropping an unplaceable path safe: a source write under the symlinked spelling is
    /// still observed, under the path the index keys it by.
    #[test]
    fn a_state_file_event_reaches_no_impact_under_every_root_spelling() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let canonical = directory.path().canonicalize()?;
        let state = canonical
            .join(rift_core::constants::RIFT_STATE_DIRECTORY)
            .join("db");
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let written = |path: &std::path::Path| {
            Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path.to_path_buf())
        };

        for root in [directory.path().to_path_buf(), directory.path().join(".")] {
            assert_eq!(
                super::watch_event_impact(
                    &super::WatchRoots::resolve(&root)?,
                    &validation,
                    &written(&state)
                ),
                super::WatchImpact::None,
                "a state write is not observed under root spelling {}",
                root.display()
            );
        }

        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir()?;
            let linked = elsewhere.path().join("workspace");
            std::os::unix::fs::symlink(directory.path(), &linked)?;
            let roots = super::WatchRoots::resolve(&linked)?;
            let linked_state = linked
                .join(rift_core::constants::RIFT_STATE_DIRECTORY)
                .join("db");
            assert_eq!(
                super::watch_event_impact(&roots, &validation, &written(&linked_state)),
                super::WatchImpact::None,
                "a state write is not observed through a symlinked root"
            );
            assert_eq!(
                super::watch_event_impact(&roots, &validation, &written(&linked.join("lib.rs"))),
                super::WatchImpact::Paths(vec![rift_core::ProjectPath::new("lib.rs")?]),
                "a source write through a symlinked root is still observed, which is what \
                 makes dropping an unplaceable path safe"
            );
        }
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
            super::watch_event_impact(
                &super::WatchRoots::resolve(&watched_root)?,
                &validation,
                &renamed
            ),
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
            super::watch_event_impact(
                &super::WatchRoots::resolve(&event_root)?,
                &validation,
                &staged
            ),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new(".tmpk3v9q2")?]),
            "an external write renames an extensionless staged file over its target; the \
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
            super::watch_event_impact(
                &super::WatchRoots::resolve(&event_root)?,
                &validation,
                &renamed_away
            ),
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
            super::watch_event_impact(
                &super::WatchRoots::resolve(&event_root)?,
                &validation,
                &moved_in
            ),
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
            super::watch_event_impact(
                &super::WatchRoots::resolve(&event_root)?,
                &validation,
                &renamed
            ),
            super::WatchImpact::Paths(vec![rift_core::ProjectPath::new("LICENSE")?]),
            "a file the publication holds is read again by itself, whatever its name"
        );
        Ok(())
    }

    /// One event whose kind does not say what it names, as notify reports every
    /// `FILE_ACTION_ADDED` and `FILE_ACTION_REMOVED` on Windows.
    fn event_at(kind: EventKind, path: &std::path::Path) -> Event {
        Event::new(kind).add_path(path.to_path_buf())
    }

    /// One publication over `root`, installed for `validation`'s event classification, and
    /// the state a rebuild publishes into.
    fn installed_state(
        root: &std::path::Path,
        validation: &IndexValidation,
    ) -> TestResult<Arc<RwLock<IndexState>>> {
        let current = stable_candidate(root, 0)?;
        validation.install_publication(&current);
        Ok(Arc::new(RwLock::new(IndexState {
            current,
            failure: None,
        })))
    }

    /// Folder names a directory event carries: one plain, and one with a dot the way a file
    /// name's extension has one.
    const FOLDER_SPELLINGS: [&str; 2] = ["pkg", "pkg.v2"];

    /// A directory moved into the workspace reaches the watcher as one `Create(Any)` on
    /// Windows, with no event for the files it carries. It names a directory on disk, so
    /// the rebuild reads the whole workspace and indexes those files, whatever the folder's
    /// name looks like, and also under a policy whose globs name only the files below it.
    #[tokio::test]
    async fn a_directory_moved_in_as_create_any_is_read_whole() -> TestResult {
        for folder in FOLDER_SPELLINGS {
            moved_in_as_create_any(folder).await?;
        }
        Ok(())
    }

    /// Moves `folder` into the workspace, reports it as `Create(Any)`, and proves the
    /// rebuild indexes the files it carried.
    async fn moved_in_as_create_any(folder: &str) -> TestResult {
        let directory = tempfile::tempdir()?;
        let elsewhere = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[source]\ninclude = [\"src/**/*.rs\"]\n",
        )?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = installed_state(directory.path(), &validation)?;
        fs::create_dir_all(elsewhere.path().join(folder))?;
        fs::write(
            elsewhere.path().join(folder).join("mod.rs"),
            "pub fn arrived() {}\n",
        )?;
        let moved_in = directory.path().join("src").join(folder);
        fs::rename(elsewhere.path().join(folder), &moved_in)?;
        let roots = super::WatchRoots::resolve(directory.path())?;
        let event_path = roots.canonical().join("src").join(folder);
        let created = event_at(EventKind::Create(CreateKind::Any), &event_path);
        super::report_watch_outcome(&roots, &validation, Ok(created));

        let whole = validation.locked_pending().covers_whole_workspace();
        assert!(
            whole,
            "{folder}: a directory on disk carries files no event named"
        );
        let outcome = rebuilt_through(directory.path(), &state, &validation, None).await?;
        assert_eq!(outcome, RebuildOutcome::Published, "{folder}");
        let current = Arc::clone(&state.read().await.current);
        assert_eq!(declarations_named(&current, "arrived")?, 1, "{folder}");
        assert_eq!(declarations_named(&current, "beacon")?, 1, "{folder}");
        Ok(())
    }

    /// A directory removed, or moved out, reaches the watcher as one `Remove(Any)` on
    /// Windows when none of its files were reported first. The publication holds files
    /// below that path, whatever its name looks like, so the rebuild reads the whole
    /// workspace and they leave the index.
    #[tokio::test]
    async fn a_directory_removed_as_remove_any_is_read_whole() -> TestResult {
        for folder in FOLDER_SPELLINGS {
            removed_as_remove_any(folder).await?;
        }
        Ok(())
    }

    /// Removes `folder` from the workspace, reports it as `Remove(Any)`, and proves its
    /// files leave the index.
    async fn removed_as_remove_any(folder: &str) -> TestResult {
        let directory = tempfile::tempdir()?;
        let removed_folder = directory.path().join("src").join(folder);
        fs::create_dir_all(&removed_folder)?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(removed_folder.join("mod.rs"), "pub fn departed() {}\n")?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = installed_state(directory.path(), &validation)?;
        fs::remove_dir_all(&removed_folder)?;
        let roots = super::WatchRoots::resolve(directory.path())?;
        let event_path = roots.canonical().join("src").join(folder);
        let removed = event_at(EventKind::Remove(RemoveKind::Any), &event_path);
        super::report_watch_outcome(&roots, &validation, Ok(removed));

        let whole = validation.locked_pending().covers_whole_workspace();
        assert!(
            whole,
            "{folder}: the publication holds files below the removed path"
        );
        let outcome = rebuilt_through(directory.path(), &state, &validation, None).await?;
        assert_eq!(outcome, RebuildOutcome::Published, "{folder}");
        let current = Arc::clone(&state.read().await.current);
        assert_eq!(declarations_named(&current, "departed")?, 0, "{folder}");
        assert_eq!(declarations_named(&current, "beacon")?, 1, "{folder}");
        Ok(())
    }

    /// A file created or removed with a kind that does not say it is a file is still one
    /// file: it names no directory on disk and the publication holds nothing below it, so
    /// the event names that path alone, extensionless or not.
    #[test]
    fn a_file_created_or_removed_as_any_names_that_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("src/gone.rs"), "pub fn gone() {}\n")?;
        fs::write(directory.path().join("LICENSE"), "beacon\n")?;
        let (validation, _current) = installed_publication(directory.path())?;
        fs::write(directory.path().join("NOTICE"), "beacon\n")?;
        fs::remove_file(directory.path().join("src/gone.rs"))?;
        fs::remove_file(directory.path().join("LICENSE"))?;
        let roots = super::WatchRoots::resolve(directory.path())?;
        let created = EventKind::Create(CreateKind::Any);
        let removed = EventKind::Remove(RemoveKind::Any);
        for (kind, path) in [
            (created, "src/lib.rs"),
            (created, "NOTICE"),
            (removed, "src/gone.rs"),
            (removed, "LICENSE"),
        ] {
            let event = event_at(kind, &roots.canonical().join(path));
            assert_eq!(
                super::watch_event_impact(&roots, &validation, &event),
                super::WatchImpact::Paths(vec![rift_core::ProjectPath::new(path)?]),
                "{kind:?} {path} names that file alone"
            );
        }
        Ok(())
    }

    /// The change set one taken observation resolves to against `previous`.
    fn change_set_of(
        root: &std::path::Path,
        validation: &IndexValidation,
        previous: &Arc<PublishedWorkspace>,
    ) -> ChangeSet {
        let mut request = validation.take_pending();
        request.previous = Some(Arc::clone(previous));
        request.change_set(root, &ConfigurationState::accept(root))
    }

    /// The paths an incremental change set names, or nothing for a whole one.
    fn changed_paths(change_set: &ChangeSet) -> Option<Vec<String>> {
        match change_set {
            ChangeSet::Full => None,
            ChangeSet::Incremental(changes) => Some(
                changes
                    .iter()
                    .map(|(path, _)| path.as_str().to_owned())
                    .collect(),
            ),
        }
    }

    /// Windows reports a directory's last-write time moving as `Modify(Any)` on the
    /// directory whenever an entry inside it is added or removed. A directory the
    /// publication holds files below is known one without a disk probe, so that report has
    /// no impact, and the file created in it is the rebuild's one change.
    #[test]
    fn a_directory_modified_beside_a_file_created_in_it_rebuilds_that_file_alone() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = installed_state(directory.path(), &validation)?;
        let previous = Arc::clone(&state.blocking_read().current);
        let roots = super::WatchRoots::resolve(directory.path())?;
        let folder_modified = event_at(
            EventKind::Modify(ModifyKind::Any),
            &roots.canonical().join("src"),
        );
        assert_eq!(
            super::watch_event_impact(&roots, &validation, &folder_modified),
            super::WatchImpact::None,
            "a directory's own modify report names nothing to read"
        );

        fs::write(directory.path().join("src/fresh.rs"), "pub fn fresh() {}\n")?;
        let created = event_at(
            EventKind::Create(CreateKind::File),
            &roots.canonical().join("src/fresh.rs"),
        );
        super::report_watch_outcome(&roots, &validation, Ok(created));
        super::report_watch_outcome(&roots, &validation, Ok(folder_modified));
        let change_set = change_set_of(directory.path(), &validation, &previous);
        assert_eq!(
            changed_paths(&change_set),
            Some(vec!["src/fresh.rs".to_owned()]),
            "{change_set:?}"
        );
        Ok(())
    }

    /// A directory the publication holds nothing below reaches the rebuild as a path when
    /// its own modify report arrives, and reading a directory as a file fails. It holds no
    /// file, so it adds nothing to the change set: alone it changes nothing, and beside a
    /// file created in it the change names that file alone.
    #[test]
    fn a_directory_modified_with_nothing_indexed_below_adds_nothing_to_the_change_set() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::create_dir_all(directory.path().join("docs"))?;
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let state = installed_state(directory.path(), &validation)?;
        let previous = Arc::clone(&state.blocking_read().current);
        let roots = super::WatchRoots::resolve(directory.path())?;
        let folder = roots.canonical().join("docs");
        let folder_modified = event_at(EventKind::Modify(ModifyKind::Any), &folder);

        super::report_watch_outcome(&roots, &validation, Ok(folder_modified.clone()));
        let alone = change_set_of(directory.path(), &validation, &previous);
        assert_eq!(changed_paths(&alone), Some(Vec::new()), "{alone:?}");

        fs::write(folder.join("first.rs"), "pub fn first() {}\n")?;
        let created = event_at(
            EventKind::Create(CreateKind::File),
            &folder.join("first.rs"),
        );
        super::report_watch_outcome(&roots, &validation, Ok(created));
        super::report_watch_outcome(&roots, &validation, Ok(folder_modified));
        let beside = change_set_of(directory.path(), &validation, &previous);
        assert_eq!(
            changed_paths(&beside),
            Some(vec!["docs/first.rs".to_owned()]),
            "{beside:?}"
        );
        Ok(())
    }

    /// A per-file byte bound small enough that [`LEFT_OUT_FILES`]'s oversized file passes it.
    ///
    /// Under the default `[search.text] large_files = "split"` a file past the bound is held
    /// as unparsed text with a digest; `"skip"` leaves it out before its bytes are read.
    const SMALL_MAX_FILE: &str =
        "[providers.syntax]\nmax_file = \"60b\"\n\n[search.text]\nlarge_files = \"skip\"\n";

    /// Files the catalog leaves out before digesting their bytes: one past
    /// [`SMALL_MAX_FILE`], and one holding a NUL byte.
    const LEFT_OUT_FILES: [(&str, &[u8]); 2] = [
        (
            "oversized.rs",
            b"// oversized source past the sixty byte bound of this workspace\n",
        ),
        ("binary.rs", b"source\0bytes\n"),
    ];

    /// A publication over a workspace holding `lib.rs` and the left-out file `name`,
    /// under [`SMALL_MAX_FILE`].
    fn left_out_publication(
        root: &std::path::Path,
        name: &str,
        bytes: &[u8],
    ) -> TestResult<Arc<PublishedWorkspace>> {
        fs::write(root.join("rift.toml"), SMALL_MAX_FILE)?;
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(root.join(name), bytes)?;
        let previous = stable_candidate(root, 0)?;
        let path = rift_core::ProjectPath::new(name)?;
        assert!(
            previous.reads.file_digest(&path).is_none()
                && previous.reads.file_warning(&path).is_some(),
            "{name} is left out with a warning and no digest"
        );
        Ok(previous)
    }

    /// The candidate one rebuild naming `path` builds over `previous`, with its change set.
    fn rebuilt_naming(
        root: &std::path::Path,
        previous: &Arc<PublishedWorkspace>,
        path: &rift_core::ProjectPath,
    ) -> TestResult<(Arc<PublishedWorkspace>, ChangeSet)> {
        let request = RebuildRequest {
            epoch: previous.epoch + 1,
            work: super::PendingWork::naming([path.clone()]),
            previous: Some(Arc::clone(previous)),
            cancellation: CancellationToken::new(),
        };
        match build_workspace_candidate(root, WorkspaceIndexLimits::default(), &request)? {
            WorkspaceCandidate::Stable {
                published,
                change_set,
            } => Ok((published, change_set)),
            WorkspaceCandidate::ConfigurationChanged => {
                Err("fixture configuration must remain stable".into())
            }
        }
    }

    #[test]
    fn visible_digests_track_unclassified_add_change_and_delete() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("rift.toml"), SMALL_MAX_FILE)?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let path = rift_core::ProjectPath::new("opaque.unknown")?;
        let previous = stable_candidate(directory.path(), 0)?;
        assert_eq!(previous.visible_digests.get(&path), None);

        let first = b"unclassified one\n";
        fs::write(directory.path().join(path.as_str()), first)?;
        let (added, added_changes) = rebuilt_naming(directory.path(), &previous, &path)?;
        assert_eq!(
            added.visible_digests.get(&path),
            Some(rift_index::FileDigest::of(first)),
            "publication records unclassified file bytes"
        );
        assert!(matches!(added_changes, ChangeSet::Incremental(changes) if changes.len() == 1));

        let second = b"unclassified two\n";
        fs::write(directory.path().join(path.as_str()), second)?;
        let (modified, change_set) = rebuilt_naming(directory.path(), &added, &path)?;
        assert_eq!(
            modified.visible_digests.get(&path),
            Some(rift_index::FileDigest::of(second)),
            "publication replaces unclassified digest"
        );
        assert!(matches!(change_set, ChangeSet::Incremental(changes) if changes.len() == 1));

        fs::remove_file(directory.path().join(path.as_str()))?;
        let (removed, change_set) = rebuilt_naming(directory.path(), &modified, &path)?;
        assert_eq!(removed.visible_digests.get(&path), None);
        assert!(matches!(change_set, ChangeSet::Incremental(changes) if changes.len() == 1));
        Ok(())
    }

    #[test]
    fn complete_visible_catalog_matches_live_capture_for_unindexed_files() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(
            root.join("rift.toml"),
            "[languages.rust]\nenabled = false\n\n[search.text]\ninclude = [\"**/*.txt\"]\n",
        )?;
        let entries: [(&str, &[u8]); 4] = [
            ("disabled.rs", b"pub fn disabled() {}\n"),
            ("binary.bin", b"source\0bytes"),
            ("invalid.bin", &[0xff, 0xfe]),
            ("opaque.unknown", b"unclassified bytes"),
        ];
        for (name, bytes) in entries {
            fs::write(root.join(name), bytes)?;
        }
        let mut previous = stable_candidate(root, 0)?;
        let policy = previous
            .source_policy
            .as_deref()
            .ok_or("complete source policy")?;
        assert_eq!(
            previous.visible_digests.as_ref(),
            &policy.visible_digests()?
        );
        for (name, bytes) in entries {
            let path = ProjectPath::new(name)?;
            assert_eq!(
                previous.reads.file_digest(&path),
                None,
                "{name} is unindexed"
            );
            assert_eq!(
                previous.visible_digests.get(&path),
                Some(rift_index::FileDigest::of(bytes))
            );
            fs::write(root.join(name), b"changed raw bytes\0")?;
            let (next, _) = rebuilt_naming(root, &previous, &path)?;
            let policy = next
                .source_policy
                .as_deref()
                .ok_or("complete source policy")?;
            assert_eq!(next.visible_digests.as_ref(), &policy.visible_digests()?);
            assert_eq!(
                previous.visible_digests.get(&path),
                Some(rift_index::FileDigest::of(bytes))
            );
            previous = next;
        }
        Ok(())
    }

    #[test]
    fn complete_visible_catalog_replaces_visibility_with_configuration() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(root.join("notes.txt"), "visible bytes\n")?;
        fs::write(root.join("rift.toml"), "")?;
        let original = stable_candidate(root, 0)?;
        let notes = ProjectPath::new("notes.txt")?;
        assert!(original.visible_digests.get(&notes).is_some());

        fs::write(
            root.join("rift.toml"),
            "[source]\nexclude = [\"notes.txt\"]\n",
        )?;
        let excluded = stable_candidate(root, 1)?;
        assert_eq!(excluded.visible_digests.get(&notes), None);
        let policy = excluded
            .source_policy
            .as_deref()
            .ok_or("complete source policy")?;
        assert_eq!(
            excluded.visible_digests.as_ref(),
            &policy.visible_digests()?
        );
        assert!(
            original.visible_digests.get(&notes).is_some(),
            "earlier publication retains its catalog"
        );
        Ok(())
    }

    /// A file left out before its bytes were digested has no digest, and a deleted path
    /// has none either. The warning the publication records tells them apart, so the
    /// rebuild that observes the deletion removes the path and its warning with it.
    #[test]
    fn a_deleted_left_out_file_leaves_no_warning_behind() -> TestResult {
        for (name, bytes) in LEFT_OUT_FILES {
            let directory = tempfile::tempdir()?;
            let previous = left_out_publication(directory.path(), name, bytes)?;
            let path = rift_core::ProjectPath::new(name)?;
            fs::remove_file(directory.path().join(name))?;
            let (published, change_set) = rebuilt_naming(directory.path(), &previous, &path)?;

            assert_eq!(
                changed_paths(&change_set),
                Some(vec![name.to_owned()]),
                "{name}: the deletion is a change"
            );
            assert_eq!(
                published.reads.file_warning(&path),
                None,
                "{name}: the deleted file's warning leaves the index"
            );
        }
        Ok(())
    }

    /// A left-out file written again with the same bytes is still on disk and still left
    /// out, so it keeps its warning. Captured raw digests compare binary files; oversized
    /// files compare their refusal. Neither unchanged file requires an index rebuild.
    #[test]
    fn a_left_out_file_written_with_the_same_bytes_keeps_its_warning() -> TestResult {
        for (name, bytes) in LEFT_OUT_FILES {
            let directory = tempfile::tempdir()?;
            let previous = left_out_publication(directory.path(), name, bytes)?;
            let path = rift_core::ProjectPath::new(name)?;
            fs::write(directory.path().join(name), bytes)?;
            let (published, change_set) = rebuilt_naming(directory.path(), &previous, &path)?;

            let expected = previous.reads.file_warning(&path).cloned();
            assert_eq!(
                published.reads.file_warning(&path).cloned(),
                expected,
                "{name}: the warning stays"
            );
            assert_eq!(changed_paths(&change_set), Some(Vec::new()), "{name}");
            assert!(Arc::ptr_eq(&published.reads, &previous.reads));
            assert!(Arc::ptr_eq(
                &published.visible_digests,
                &previous.visible_digests
            ));
        }
        Ok(())
    }

    /// A path observed before a publication held files below it can name a directory that
    /// has left the disk since. Reading the path finds nothing, so the change set reads the
    /// whole workspace instead of keeping the files the publication holds below it.
    #[test]
    fn a_change_set_naming_a_directory_gone_from_the_disk_reads_whole() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("pkg"))?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("pkg/mod.rs"),
            "pub fn departed() {}\n",
        )?;
        let previous = stable_candidate(directory.path(), 0)?;
        fs::remove_dir_all(directory.path().join("pkg"))?;
        let request = RebuildRequest {
            epoch: 1,
            work: super::PendingWork::naming([rift_core::ProjectPath::new("pkg")?]),
            previous: Some(previous),
            cancellation: CancellationToken::new(),
        };
        let configuration = ConfigurationState::accept(directory.path());
        assert_eq!(
            request.change_set(directory.path(), &configuration),
            ChangeSet::Full
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
            super::workspace_capture(),
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
    fn a_whole_workspace_rebuild_rescans_under_unchanged_index_configuration() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let previous = stable_candidate(directory.path(), 0)?;
        fs::create_dir_all(directory.path().join("fresh"))?;
        fs::write(directory.path().join("fresh/mod.rs"), "pub fn fresh() {}\n")?;
        let limits = WorkspaceIndexLimits::default();
        let rebuild = |epoch: u64| -> TestResult<Arc<PublishedWorkspace>> {
            let request = super::RebuildRequest {
                epoch,
                work: super::PendingWork::whole_workspace(),
                previous: Some(Arc::clone(&previous)),
                cancellation: CancellationToken::new(),
            };
            match build_workspace_candidate(directory.path(), limits, &request)? {
                WorkspaceCandidate::Stable {
                    published,
                    change_set,
                } => {
                    assert_eq!(
                        change_set,
                        ChangeSet::Full,
                        "a whole observation names no paths"
                    );
                    Ok(published)
                }
                WorkspaceCandidate::ConfigurationChanged => {
                    Err("fixture configuration must remain stable".into())
                }
            }
        };

        let rescanned = rebuild(1)?;
        let fresh = stable_candidate(directory.path(), 1)?;
        assert_eq!(
            rescanned.reads.tree_revision(),
            fresh.reads.tree_revision(),
            "the rescan answers for the tree a full build answers for"
        );
        let params = serde_json::from_value(serde_json::json!({"name": "fresh"}))?;
        assert_eq!(rescanned.reads.get_symbol(&params)?.hits.len(), 1);

        fs::write(
            directory.path().join("rift.toml"),
            "[providers.syntax]\nmax_file = \"60b\"\n",
        )?;
        let reconfigured = rebuild(2)?;
        assert_ne!(
            reconfigured.configuration.fingerprint, previous.configuration.fingerprint,
            "an index-owned change builds under the new configuration"
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
            cancellation: CancellationToken::new(),
        };

        let limits = WorkspaceIndexLimits::default();
        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), limits, &request)?
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
            cancellation: CancellationToken::new(),
        };

        let limits = WorkspaceIndexLimits::default();
        let WorkspaceCandidate::Stable {
            published: candidate,
            ..
        } = build_workspace_candidate(directory.path(), limits, &request)?
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
        let configurations = ["[server]\nnum_workers = 2\n", "[server]\nnum_workers = 3\n"];
        for configuration in configurations {
            let directory = tempfile::tempdir()?;
            fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
            let previous = stable_candidate(directory.path(), 0)?;
            fs::write(directory.path().join("rift.toml"), configuration)?;
            let request = RebuildRequest {
                epoch: 1,
                work: super::PendingWork::naming([rift_core::ProjectPath::new("rift.toml")?]),
                previous: Some(Arc::clone(&previous)),
                cancellation: CancellationToken::new(),
            };

            let WorkspaceCandidate::Stable {
                published: candidate,
                change_set,
            } = build_workspace_candidate(
                directory.path(),
                WorkspaceIndexLimits::default(),
                &request,
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
                cancellation: CancellationToken::new(),
            };

            let WorkspaceCandidate::Stable {
                published: candidate,
                change_set,
            } = build_workspace_candidate(
                directory.path(),
                WorkspaceIndexLimits::default(),
                &request,
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
    fn git_control_access_events_do_not_invalidate_the_workspace() {
        use notify::event::{AccessKind, AccessMode};

        // Issue #500: repository reads must not schedule another repository read.
        let root = std::path::PathBuf::from("/rift-workspace");
        for git_directory in [root.join(".git"), std::path::PathBuf::from("/rift-git")] {
            let roots = super::WatchRoots {
                canonical: root.clone(),
                watched: root.clone(),
                git_directory: Some(git_directory.clone()),
            };
            let (validation, mut receiver) =
                IndexValidation::new(WorkspaceIndexLimits::default().files_max());
            for access in [
                AccessKind::Any,
                AccessKind::Read,
                AccessKind::Open(AccessMode::Any),
                AccessKind::Close(AccessMode::Read),
                AccessKind::Close(AccessMode::Write),
            ] {
                let head =
                    Event::new(EventKind::Access(access)).add_path(git_directory.join("HEAD"));
                let lock = Event::new(EventKind::Access(access))
                    .add_path(git_directory.join("index.lock"));
                super::report_watch_outcome(&roots, &validation, Ok(head));
                super::report_watch_outcome(&roots, &validation, Ok(lock));
            }
            assert_eq!(validation.observed_epoch(), 0);
            assert!(receiver.try_recv().is_err());
            let pending = validation.take_pending().work;
            assert!(!pending.covers_whole_workspace());
            assert_eq!(pending.paths().count(), 0);
        }
    }

    #[test]
    fn git_control_changes_and_rescans_invalidate_the_workspace() {
        use notify::event::{AccessKind, AccessMode, DataChange, Flag};

        let root = std::path::PathBuf::from("/rift-workspace");
        for git_directory in [root.join(".git"), std::path::PathBuf::from("/rift-git")] {
            let roots = super::WatchRoots {
                canonical: root.clone(),
                watched: root.clone(),
                git_directory: Some(git_directory.clone()),
            };
            let (validation, mut receiver) =
                IndexValidation::new(WorkspaceIndexLimits::default().files_max());
            let head_write = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(git_directory.join("HEAD"));
            let lock_create = Event::new(EventKind::Create(CreateKind::File))
                .add_path(git_directory.join("index.lock"));
            let lock_remove = Event::new(EventKind::Remove(RemoveKind::File))
                .add_path(git_directory.join("index.lock"));
            let rescan = Event::new(EventKind::Access(AccessKind::Open(AccessMode::Any)))
                .add_path(git_directory.join("HEAD"))
                .set_flag(Flag::Rescan);
            for (index, event) in [head_write, lock_create, lock_remove, rescan]
                .into_iter()
                .enumerate()
            {
                super::report_watch_outcome(&roots, &validation, Ok(event));
                assert_eq!(validation.observed_epoch(), index as u64 + 1);
                assert!(receiver.try_recv().is_ok());
                assert!(validation.take_pending().work.covers_whole_workspace());
            }
        }
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
            super::watch_event_impact(&super::WatchRoots::at(root), &validation, &event),
            super::WatchImpact::None,
            "an access event never reaches the inclusion predicate"
        );
        assert_eq!(
            super::watch_path_impact(
                &super::WatchRoots::at(root),
                &validation,
                EventKind::Access(AccessKind::Any),
                &path
            ),
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
            super::workspace_capture(),
        )?;
        assert!(matches!(outcome, super::CapturedRebuild::Superseded));
        assert!(
            validation.superseded_after(0).is_none(),
            "an unbuilt candidate cannot let reads answer stale"
        );
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
        assert!(
            validation.superseded_after(0).is_none(),
            "configuration movement cannot let reads answer stale"
        );
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
        let watcher = unwatched(directory.path(), &validation)?;
        let blocking = crate::server::BlockingExecutor::isolated(1, 60_000);
        blocking.operations.close();
        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            super::IndexSupervisorContext {
                root: directory.path().to_path_buf(),
                limits: WorkspaceIndexLimits::default(),
                published: Arc::clone(&published),

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
        let watcher = unwatched(directory.path(), &validation)?;
        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            super::IndexSupervisorContext {
                root: directory.path().join("vanished"),
                limits: WorkspaceIndexLimits::default(),
                published: Arc::clone(&published),

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

    /// How many declarations named `name` one snapshot answers.
    fn declarations_named(published: &PublishedWorkspace, name: &str) -> TestResult<usize> {
        let params = serde_json::from_value(serde_json::json!({ "name": name }))?;
        Ok(published.reads.get_symbol(&params)?.hits.len())
    }

    /// The startup record names the installed epoch after every selected file is prepared.
    #[test]
    fn startup_publication_is_recorded_once_after_complete_preparation() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        let (context, _invalidations) = initial_preparation_context(root)?;
        let partial = Arc::clone(&context.published.blocking_read().current);
        let complete = stable_candidate(root, 0)?;
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);
        let publish = |candidate| {
            super::publish_preparation_after(
                root,
                &context.published,
                &context.validation,
                candidate,
                None,
            )
        };

        assert_eq!(publish(&partial), RebuildOutcome::Published);
        assert!(queued_records(&mut drain).is_empty());
        let epoch = context
            .validation
            .observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        assert_eq!(publish(&complete), RebuildOutcome::Published);
        let records = queued_records(&mut drain);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].component(), "index");
        assert_eq!(records[0].operation(), "index.publish");
        assert_eq!(records[0].message(), "index snapshot published");
        let fields: serde_json::Value = serde_json::from_str(records[0].fields())?;
        assert_eq!(fields["trigger"], "startup");
        assert_eq!(fields["epoch"], epoch.to_string());
        let published = context.published.blocking_read();
        assert!(published.current.preparation.is_none());
        assert_eq!(published.current.epoch, epoch);
        drop(published);
        assert_eq!(publish(&complete), RebuildOutcome::Superseded);
        assert!(queued_records(&mut drain).is_empty());
        Ok(())
    }

    #[test]
    fn superseded_preparation_emits_no_startup_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        let (context, _invalidations) = initial_preparation_context(root)?;
        let complete = stable_candidate(root, 0)?;
        context.validation.observe_whole_workspace()?;
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        assert_eq!(
            super::publish_preparation_after(
                root,
                &context.published,
                &context.validation,
                &complete,
                None,
            ),
            RebuildOutcome::Superseded
        );
        assert!(
            context
                .published
                .blocking_read()
                .current
                .preparation
                .is_some()
        );
        assert!(queued_records(&mut drain).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn preparation_worker_records_only_successful_startup_publication() -> TestResult {
        for (missing, cancelled) in [(false, false), (true, false), (false, true)] {
            let directory = tempfile::tempdir()?;
            let root = directory.path();
            fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
            let (context, _invalidations) = initial_preparation_context(root)?;
            let initial = super::discover_initial_workspace(&context)
                .await?
                .ok_or("initial discovery was cancelled")?;
            if missing {
                fs::remove_file(root.join("lib.rs"))?;
            }
            let cancellation = context.validation.cancellation.clone();
            if cancelled {
                cancellation.cancel();
            }
            let batch = complete_initial_batch(&context, initial);
            let (sink, mut drain) = crate::logs::log_capture();
            let subscriber = tracing_subscriber::registry().with(sink);
            let outcome = tokio::task::spawn_blocking(move || {
                tracing::subscriber::with_default(subscriber, || {
                    super::prepare_initial_batch(&cancellation, batch)
                })
            })
            .await?;
            let publications = queued_records(&mut drain)
                .into_iter()
                .filter(|record| record.operation() == "index.publish")
                .collect::<Vec<_>>();
            if missing || cancelled {
                assert!(outcome.is_err());
                assert!(context.published.read().await.current.preparation.is_some());
                assert!(publications.is_empty());
            } else {
                assert_eq!(outcome?.2, RebuildOutcome::Published);
                assert!(context.published.read().await.current.preparation.is_none());
                assert_eq!(publications.len(), 1);
                let fields: serde_json::Value = serde_json::from_str(publications[0].fields())?;
                assert_eq!(fields["trigger"], "startup");
            }
        }
        Ok(())
    }

    fn complete_initial_batch(
        context: &super::IndexSupervisorContext,
        initial: super::InitialWorkspacePreparation,
    ) -> super::InitialWorkspaceBatch {
        super::InitialWorkspaceBatch {
            preparation: initial.preparation,
            target: initial.total,
            complete: true,
            total: initial.total,
            root: context.root.clone(),
            state: Arc::clone(&context.published),
            validation: Arc::clone(&context.validation),
            configuration: initial.configuration,
            source_policy: initial.source_policy,
            previous_capture: initial.last,
            map_source_paths: initial.map_source_paths,
            map_text_paths: initial.map_text_paths,
            lexical: None,
        }
    }

    /// A file edited after discovery is captured before its first prepared publication.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_file_edited_after_discovery_is_in_the_first_prepared_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        let lib = rift_core::ProjectPath::new("lib.rs")?;
        let (context, _invalidations) = initial_preparation_context(root)?;
        let initial = super::discover_initial_workspace(&context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        fs::write(root.join("lib.rs"), "pub fn edited() {}\n")?;
        let epoch = context.validation.observe_paths([lib.clone()])?;
        super::prepare_initial_workspace_from(&context, initial).await?;
        let startup = Arc::clone(&context.published.read().await.current);

        assert_eq!(
            startup.epoch, epoch,
            "preparation captures path event before publishing the file"
        );
        assert_eq!(
            declarations_named(&startup, "edited")?,
            1,
            "prepared reads use the current file bytes"
        );
        assert_eq!(declarations_named(&startup, "beacon")?, 0);
        assert!(startup.preparation.is_none());
        Ok(())
    }

    /// A configuration change after discovery makes the prepared snapshot wait for a full
    /// rebuild under the accepted policy.
    #[tokio::test]
    async fn a_configuration_written_after_discovery_rebuilds_the_prepared_workspace() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(root.join("other.rs"), "pub fn lantern() {}\n")?;
        let lib = rift_core::ProjectPath::new("lib.rs")?;
        let other = rift_core::ProjectPath::new("other.rs")?;
        let (context, _invalidations) = initial_preparation_context(root)?;
        let initial = super::discover_initial_workspace(&context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        let excluding = "[source]\nexclude = [\"other.rs\"]\n";
        fs::write(root.join("rift.toml"), excluding)?;
        context.validation.observe_whole_workspace()?;
        super::prepare_initial_workspace_from(&context, initial).await?;
        let request = context.validation.take_pending();
        assert!(request.work.covers_whole_workspace());
        assert_eq!(
            super::rebuild_workspace(&context, request, super::workspace_capture()).await?,
            RebuildOutcome::Published
        );
        let published = Arc::clone(&context.published.read().await.current);
        assert_eq!(
            published.configuration.fingerprint,
            super::configuration_fingerprint(root)
        );
        assert!(published.reads.file_digest(&lib).is_some());
        assert!(
            published.reads.file_digest(&other).is_none(),
            "the written policy excludes other.rs"
        );
        Ok(())
    }

    /// A directory removed after discovery refuses its selected batch, then full rebuild
    /// publishes the remaining workspace.
    #[tokio::test]
    async fn a_directory_renamed_away_after_discovery_rebuilds_the_workspace() -> TestResult {
        for folder in FOLDER_SPELLINGS {
            renamed_away_after_discovery(folder).await?;
        }
        Ok(())
    }

    /// Renames `folder` out after discovery, then proves the full rebuild drops its files.
    async fn renamed_away_after_discovery(folder: &str) -> TestResult {
        let directory = tempfile::tempdir()?;
        let elsewhere = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::create_dir_all(root.join(folder))?;
        fs::write(root.join(folder).join("mod.rs"), "pub fn departed() {}\n")?;
        let (context, _invalidations) = initial_preparation_context(root)?;
        let initial = super::discover_initial_workspace(&context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        let destination = elsewhere.path().join(folder);
        fs::rename(root.join(folder), destination)?;
        context.validation.observe_whole_workspace()?;
        let error = super::prepare_initial_workspace_from(&context, initial)
            .await
            .expect_err("a renamed discovered directory must refuse its selected paths");
        assert_eq!(error.descriptor().code(), "temporarily_unavailable");
        let request = context.validation.take_pending();
        assert!(request.work.covers_whole_workspace());
        assert_eq!(
            super::rebuild_workspace(&context, request, super::workspace_capture()).await?,
            RebuildOutcome::Published
        );
        let published = Arc::clone(&context.published.read().await.current);
        assert_eq!(declarations_named(&published, "departed")?, 0, "{folder}");
        assert_eq!(declarations_named(&published, "beacon")?, 1, "{folder}");
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

        let units = published.reads.index_documents();
        let guide_units = units.iter().filter(|unit| names_guide(unit)).count();
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
        for unit in units.iter().filter(|unit| names_guide(unit)) {
            assert!(
                ranked.iter().any(|one| one == unit.identity()),
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

            validation: Arc::clone(validation),
            blocking: BlockingExecutor::for_configuration(&ServerConfiguration::default())
                .expect("the default server configuration builds a worker pool"),
            population: None,
            lexical,
        };
        let capture = super::workspace_capture();
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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

    #[tokio::test]
    async fn a_commit_releases_its_publication_while_the_store_holds_the_write() -> TestResult {
        for (whole_owed, change) in [(false, false), (false, true), (true, true)] {
            let directory = tempfile::tempdir()?;
            let published = candidate_declaring(directory.path(), 0, "beacon")?;
            let revision = published.reads.tree_revision().to_owned();
            let retired = Arc::downgrade(&published);
            let write = if change {
                change_naming_lib()?
            } else {
                super::lexical_write(&published, &ChangeSet::Full)
            };
            let double = StoreDouble::new();
            let cancellation = CancellationToken::new();
            let _cancel = cancellation.clone().drop_guard();
            let lane = LexicalLane::spawn_over(
                Arc::clone(&double),
                super::lexical_double::UNBOUNDED,
                BlockingExecutor::isolated(2, 60_000),
                cancellation.clone(),
                Arc::from(super::lexical_double::PRODUCT_VERSION),
            );
            if whole_owed {
                lane.queue
                    .locked()
                    .owe_whole("the previous transaction failed".to_owned());
            }
            lane.request(write, published);
            // A store holding nothing it can keep is cleared before a whole write lands.
            let expected = if change && !whole_owed {
                vec![("apply", revision.clone())]
            } else {
                vec![("clear", String::new()), ("apply", revision.clone())]
            };
            assert_eq!(double.calls_within_bound(expected.len()).await?, expected);
            tokio::time::timeout(Duration::from_secs(3), async {
                while retired.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert_eq!(lane.commit_state(&revision), LexicalCommitState::Committing);
            assert_eq!(double.dropped_while_held(), 0);
            double.release_one();
            commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;
            cancellation.cancel();
            ended_within_bound(&lane).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_prepared_change_owes_a_commit_when_worker_admission_is_closed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let revision = published.reads.tree_revision().to_owned();
        let double = StoreDouble::new();
        let blocking = BlockingExecutor::isolated(1, 60_000);
        blocking.operations.close();
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            blocking,
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(change_naming_lib()?, published);
        let cause = owed_within_bound(&lane, &revision).await?;
        assert!(cause.contains("lexical unit derivation"), "{cause}");
        assert!(
            double.calls().is_empty(),
            "admission refusal writes no rows"
        );
        cancellation.cancel();
        ended_within_bound(&lane).await?;
        Ok(())
    }

    /// The file rows the trigram index lacks for `tree_revision`, as a `beacon` pattern read
    /// of `index` counts them; zero once the index holds every row.
    async fn rows_lacking_trigrams(index: &SearchIndex, tree_revision: &str) -> TestResult<u64> {
        let pattern = Pattern::parse("beacon", 1 << 20)?;
        let prefilter = pattern
            .prefilter()
            .ok_or("`beacon` requires its trigrams")?;
        let read = index
            .pattern_candidates(tree_revision, prefilter, true, 10_000)
            .await?;
        let RevisionScoped::Matched(candidates) = read else {
            return Err(format!("the store holds {tree_revision}: {read:?}").into());
        };
        Ok(candidates
            .unindexed()
            .map_or(0, |unindexed| unindexed.total() - unindexed.prepared()))
    }

    /// Polls `index` until its trigram index holds every row of `tree_revision`.
    async fn trigrams_caught_up_within_bound(
        index: &SearchIndex,
        tree_revision: &str,
    ) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if rows_lacking_trigrams(index, tree_revision).await? == 0 {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err("the lane never caught the trigram index up".into())
    }

    /// A write lands while the trigram batch after it is held, so no commit waits for the
    /// trigram index, and the held batch then indexes every row the write stored. A lane
    /// started over rows an earlier lane stored and never indexed indexes them with no
    /// write handed to it.
    #[tokio::test]
    async fn a_write_lands_while_its_trigram_batch_is_held_and_a_new_lane_resumes_it() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let revision = published.reads.tree_revision().to_owned();
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let double = StoreDouble::new();
        double.attach(Arc::clone(&index));
        double.hold_trigrams_after_writes(1);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        double.release_one();
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;
        assert!(
            rows_lacking_trigrams(&index, &revision).await? > 0,
            "the write stored rows the held batch has not indexed"
        );
        cancellation.cancel();
        ended_within_bound(&lane).await?;

        double.release_trigrams();
        let passed = double.trigram_batches();
        let resumed = CancellationToken::new();
        let _cancel = resumed.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            resumed.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        trigrams_caught_up_within_bound(&index, &revision).await?;
        assert_eq!(double.applied().len(), 1, "no second write was handed");
        assert!(
            double.trigram_batches() > passed,
            "the new lane ran a batch"
        );
        resumed.cancel();
        ended_within_bound(&lane).await?;
        Ok(())
    }

    /// A write handed while a trigram batch is running runs before the next batch: with
    /// every batch after the running one held, the write still lands, and the rows it
    /// stored wait for the next batch.
    #[tokio::test]
    async fn a_held_write_runs_before_the_next_trigram_batch() -> TestResult {
        let directory = tempfile::tempdir()?;
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        let first_revision = first.reads.tree_revision().to_owned();
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let double = StoreDouble::new();
        double.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        double.release_one();
        lane.request(
            super::lexical_write(&first, &ChangeSet::Full),
            Arc::clone(&first),
        );
        commit_state_within_bound(&lane, &first_revision, LexicalCommitState::Settled).await?;
        trigrams_caught_up_within_bound(&index, &first_revision).await?;

        double.hold_trigrams();
        let arrived = double.trigram_arrivals();
        let second = candidate_declaring(directory.path(), 1, "beacon")?;
        let second_revision = second.reads.tree_revision().to_owned();
        double.release_one();
        lane.request(super::lexical_write(&second, &ChangeSet::Full), second);
        commit_state_within_bound(&lane, &second_revision, LexicalCommitState::Settled).await?;
        tokio::time::timeout(LANE_WAIT_MAX, double.trigram_arrivals_reach(arrived + 1)).await??;
        assert_eq!(
            double.trigram_arrivals(),
            arrived + 1,
            "the batch after the second write waits at the gate"
        );

        let third = candidate_declaring(directory.path(), 2, "gamma")?;
        let third_revision = third.reads.tree_revision().to_owned();
        double.release_one();
        lane.request(super::lexical_write(&third, &ChangeSet::Full), third);
        double.release_trigram_batch();
        commit_state_within_bound(&lane, &third_revision, LexicalCommitState::Settled).await?;
        assert!(
            rows_lacking_trigrams(&index, &third_revision).await? > 0,
            "the rows the third write stored wait for a batch still held"
        );
        double.release_trigrams();
        trigrams_caught_up_within_bound(&index, &third_revision).await?;
        cancellation.cancel();
        ended_within_bound(&lane).await?;
        Ok(())
    }

    /// A trigram batch the store refuses is recorded as a warning and owes nothing more:
    /// the lane waits for the next write, which owes a batch again.
    #[tokio::test]
    async fn a_refused_trigram_batch_is_recorded_and_the_next_write_owes_another() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);
        let double = StoreDouble::new();
        double.refuse_trigrams();
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        tokio::time::timeout(LANE_WAIT_MAX, double.trigram_arrivals_reach(1)).await??;
        double.release_one();
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        tokio::time::timeout(LANE_WAIT_MAX, double.trigram_arrivals_reach(2)).await??;

        assert_eq!(
            double.applied().len(),
            1,
            "the write ran between the batches"
        );
        assert_eq!(double.trigram_batches(), 0, "the store took no batch");
        let refused = queued_records(&mut drain)
            .into_iter()
            .find(|record| {
                record
                    .message()
                    .contains("could not take a batch of file rows")
            })
            .ok_or("the refused batch is recorded")?;
        assert_eq!(refused.level(), "warn");
        assert_eq!(refused.component(), "search");
        Ok(())
    }

    #[tokio::test]
    async fn a_whole_write_compares_the_recorded_digests_and_writes_only_what_moved() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("kept.rs"), "pub fn keptalpha() {}\n")?;
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        // The store lives outside the captured tree, as `.rift/db` does: the second capture
        // runs after the open, and a file the open writes beside the database would join
        // the tree as a moved file.
        let state = tempfile::tempdir()?;
        let index = Arc::new(search_index(&state.path().join("search.db")).await?);
        let double = StoreDouble::new();
        double.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        double.release_one();
        lane.request(
            super::lexical_write(&first, &ChangeSet::Full),
            Arc::clone(&first),
        );
        commit_state_within_bound(
            &lane,
            first.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        let every = double
            .applied()
            .pop()
            .ok_or("the first whole write reached the store")?;
        assert!(
            every.len() >= 2,
            "a store holding nothing takes every file: {every:?}"
        );

        let second = candidate_declaring(directory.path(), 1, "secondgamma")?;
        let revision = second.reads.tree_revision().to_owned();
        double.release_one();
        lane.request(super::lexical_write(&second, &ChangeSet::Full), second);
        commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;

        assert_eq!(
            double.applied().pop(),
            Some(vec![rift_core::ProjectPath::new("lib.rs")?]),
            "the comparison writes the one file whose bytes moved"
        );
        assert_eq!(
            double
                .calls()
                .iter()
                .filter(|(form, _)| *form == "clear")
                .count(),
            1,
            "rows the same derivation stamped are kept, never cleared"
        );
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(revision.as_str())
        );
        assert!(
            !ranked_at(&index, &revision, "secondgamma", 8)
                .await?
                .is_empty()
        );
        assert!(
            ranked_at(&index, &revision, "firstbeta", 8)
                .await?
                .is_empty()
        );
        assert!(
            !ranked_at(&index, &revision, "keptalpha", 8)
                .await?
                .is_empty()
        );
        cancellation.cancel();
        ended_within_bound(&lane).await?;
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
            defaults.matches_max(),
            defaults.pool_slots(),
            defaults.busy_timeout_ms(),
        );
        let limits = SearchIndexLimits::builder(lexical).disable_vector().build();
        Ok(SearchIndex::open(database, limits).await?)
    }

    /// One file document whose text is `bytes` long, for the bound cases: the content
    /// bound applies to file text, the one content a document stores.
    fn unit_of(path: &str, identity: &str, bytes: usize) -> TestResult<IndexDocument> {
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, identity)
            .with(SearchableField::FileContent, "x".repeat(bytes));
        let document = IndexDocument::new(
            DocumentIdentity::new(identity)?,
            DocumentLocation::Project(rift_core::ProjectPath::new(path)?),
            DocumentKind::TextFile,
            fields.digest(),
            fields,
        )?;
        Ok(document.at_byte_offset(0))
    }

    /// Whether `document` addresses the chunked guide the text-file cases write.
    fn names_guide(document: &IndexDocument) -> bool {
        document
            .project_path()
            .is_some_and(|path| path.as_str() == "guide.txt")
    }

    #[test]
    fn a_change_leaves_out_every_unit_over_the_bound() -> TestResult {
        let paths = ["a.rs", "b.rs", "c.rs"]
            .into_iter()
            .map(rift_core::ProjectPath::new)
            .collect::<Result<Vec<_>, _>>()?;
        let change = LexicalChange::new(
            paths,
            vec![
                unit_of("a.rs", "small", 8)?,
                unit_of("b.rs", "large", 9)?,
                unit_of("c.rs", "exact", 8)?,
            ],
        );

        let (kept, left_out) = super::within_unit_bound(change, 8);

        assert_eq!(
            kept.inserted()
                .iter()
                .map(|unit| unit.identity().as_str())
                .collect::<Vec<_>>(),
            ["small", "exact"]
        );
        assert_eq!(left_out.len(), 1);
        assert_eq!(
            left_out[0]
                .project_path()
                .map(rift_core::ProjectPath::as_str),
            Some("b.rs")
        );
        Ok(())
    }

    #[test]
    fn a_change_keeps_its_replaced_paths_and_digests_while_leaving_out_the_unit() -> TestResult {
        let path = rift_core::ProjectPath::new("grown.rs")?;
        let digest = rift_index::FileDigest::of(b"grown");
        let change = LexicalChange::new(vec![path.clone()], vec![unit_of("grown.rs", "grown", 9)?])
            .with_recorded(vec![(path.clone(), digest)]);

        let (change, left_out) = super::within_unit_bound(change, 8);

        assert_eq!(change.replaced(), std::slice::from_ref(&path));
        assert!(
            change.inserted().is_empty(),
            "the grown unit is left out: {:?}",
            change.inserted()
        );
        assert_eq!(
            change.recorded(),
            [(path, digest)],
            "the file is not derived again until its bytes change"
        );
        assert_eq!(left_out.len(), 1);
        Ok(())
    }

    /// The lane commits the rest of the set when one file row exceeds the store's unit
    /// bound, stamps the revision, and records the unit it left out. The declaration
    /// inside that file keeps its own row, which holds no source. The store's own refusal
    /// is what this replaces: `lexical.rs` still refuses such a unit handed to it directly.
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
        assert_eq!(
            blob.iter()
                .map(DocumentIdentity::as_str)
                .collect::<Vec<_>>(),
            ["rift://symbol/rust/blob.rs/BLOB"],
            "the oversized file row is absent and its declaration's row stays"
        );
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
            cancellation: CancellationToken::new(),
        };
        let limits = WorkspaceIndexLimits::default();
        let WorkspaceCandidate::Stable {
            published: second,
            change_set,
        } = build_workspace_candidate(directory.path(), limits, &request)?
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

    /// Publications of one tree whose `lib.rs` declares a different name each time.
    fn declaring_publications(
        root: &std::path::Path,
        count: u64,
    ) -> TestResult<Vec<Arc<PublishedWorkspace>>> {
        (0..count)
            .map(|epoch| candidate_declaring(root, epoch, &format!("declared{epoch}")))
            .collect()
    }

    #[test]
    fn a_backlog_merges_every_write_handed_while_one_waits_and_answers_for_each() -> TestResult {
        let directory = tempfile::tempdir()?;
        let publications = declaring_publications(directory.path(), 4)?;
        let revision = |epoch: usize| publications[epoch].reads.tree_revision().to_owned();
        let commit = |epoch: usize| -> TestResult<super::LexicalCommit> {
            Ok(super::LexicalCommit::new(
                change_naming_lib()?,
                Arc::clone(&publications[epoch]),
            ))
        };
        let mut backlog = super::LexicalBacklog::default();
        assert!(
            backlog.hand(commit(0)?).is_none(),
            "an empty lane holds the write as it is"
        );
        let released = backlog.hand(commit(1)?).ok_or("a second write merges")?;
        assert!(
            Arc::ptr_eq(&released, &publications[0]),
            "the merge releases the older publication"
        );
        assert!(backlog.hand(commit(2)?).is_some());
        let held = backlog.held.as_ref().ok_or("one write is held")?;
        assert!(
            Arc::ptr_eq(&held.published, &publications[2]),
            "the merged write derives from the newest publication"
        );
        assert!(matches!(held.write, super::LexicalWrite::Paths(_)));
        for epoch in 0..3 {
            assert_eq!(
                backlog.state_of(&revision(epoch)),
                LexicalCommitState::Committing,
                "every merged revision is committing"
            );
        }

        let (_taken, whole_owed) = backlog.take_next().ok_or("the merged write is held")?;
        assert!(!whole_owed);
        assert_eq!(
            backlog.state_of(&revision(0)),
            LexicalCommitState::Committing,
            "a running write answers for every revision merged into it"
        );
        backlog.owe_whole("the store refused".to_owned());
        backlog.running.clear();
        assert_eq!(
            backlog.state_of(&revision(2)),
            LexicalCommitState::Owed {
                cause: "the store refused".to_owned()
            }
        );
        assert!(backlog.hand(commit(3)?).is_none());
        let (_taken, whole_owed) = backlog.take_next().ok_or("the next write is held")?;
        assert!(whole_owed, "the owed comparison becomes the next write's");
        backlog.running.clear();
        assert_eq!(
            backlog.state_of(&revision(3)),
            LexicalCommitState::Settled,
            "nothing held, nothing running, nothing owed"
        );
        Ok(())
    }

    #[test]
    fn a_merged_write_stays_whole_once_either_side_is_whole_or_its_paths_pass_the_bound()
    -> TestResult {
        let path = |name: &str| rift_core::ProjectPath::new(name);
        let named = |names: &[&str]| -> TestResult<LexicalWrite> {
            Ok(LexicalWrite::Paths(
                names
                    .iter()
                    .map(|name| path(name))
                    .collect::<Result<_, _>>()?,
            ))
        };
        let merged = named(&["a.rs"])?.merged_with(named(&["b.rs", "a.rs"])?);
        let LexicalWrite::Paths(paths) = merged else {
            return Err("two named writes merge into their union".into());
        };
        assert_eq!(paths.len(), 2);
        assert!(matches!(
            LexicalWrite::Whole.merged_with(named(&["a.rs"])?),
            LexicalWrite::Whole
        ));
        assert!(matches!(
            named(&["a.rs"])?.merged_with(LexicalWrite::Whole),
            LexicalWrite::Whole
        ));
        let many = (0..=LEXICAL_HELD_PATHS_MAX)
            .map(|index| path(&format!("f{index}.rs")))
            .collect::<Result<_, _>>()?;
        assert!(
            matches!(
                LexicalWrite::Paths(many).merged_with(named(&["a.rs"])?),
                LexicalWrite::Whole
            ),
            "a union past the bound compares the whole publication instead"
        );
        assert!(
            named(&[])?.is_empty(),
            "a write naming no path owes the store nothing"
        );
        assert!(!named(&["a.rs"])?.is_empty());
        assert!(!LexicalWrite::Whole.is_empty());
        Ok(())
    }

    #[test]
    fn a_merged_write_answers_for_the_newest_revisions_within_the_bound() -> TestResult {
        let directory = tempfile::tempdir()?;
        let publications = declaring_publications(directory.path(), 2)?;
        let mut held =
            super::LexicalCommit::new(change_naming_lib()?, Arc::clone(&publications[0]));
        held.answers = (0..LEXICAL_HELD_REVISIONS_MAX)
            .map(|index| format!("revision{index}"))
            .collect();
        let newer = super::LexicalCommit::new(change_naming_lib()?, Arc::clone(&publications[1]));
        let (merged, _released) = held.merged_with(newer);
        assert_eq!(merged.answers.len(), LEXICAL_HELD_REVISIONS_MAX);
        assert!(
            !merged.answers_for("revision0"),
            "the oldest revision falls out first"
        );
        assert!(merged.answers_for(publications[1].reads.tree_revision()));
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
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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

    /// A whole write reads the digests the store recorded before it opens a transaction, so a
    /// store that refuses the read leaves the comparison owed with the store's own cause,
    /// and the next publication compares again once the store answers.
    #[tokio::test]
    async fn a_refused_recorded_digest_read_leaves_the_whole_comparison_owed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        let second = candidate_declaring(directory.path(), 1, "secondgamma")?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );

        double.refuse_reads();
        lane.request(
            super::lexical_write(&first, &ChangeSet::Full),
            Arc::clone(&first),
        );
        let cause = owed_within_bound(&lane, first.reads.tree_revision()).await?;
        assert!(
            cause.contains("the double refuses reads"),
            "the owed cause is the store's own: {cause}"
        );
        assert!(
            lane.owes_whole(),
            "a refused read leaves the comparison owed"
        );
        assert!(
            double.calls().is_empty(),
            "no transaction opens without the recorded digests"
        );

        double.accept_reads();
        double.release_one();
        lane.request(change_naming_lib()?, Arc::clone(&second));
        let calls = double.calls_within_bound(2).await?;
        assert_eq!(
            calls,
            vec![
                ("clear", String::new()),
                ("apply", second.reads.tree_revision().to_owned()),
            ],
            "the next publication compares its whole tree with the store"
        );
        commit_state_within_bound(
            &lane,
            second.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        cancellation.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn a_failing_commit_leads_to_a_whole_comparison_on_the_next_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        let first = candidate_declaring(directory.path(), 0, "firstbeta")?;
        let second = candidate_declaring(directory.path(), 1, "secondgamma")?;
        let third = candidate_declaring(directory.path(), 2, "thirddelta")?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        double.release_one();
        lane.request(
            super::lexical_write(&first, &ChangeSet::Full),
            Arc::clone(&first),
        );
        double.calls_within_bound(2).await?;
        commit_state_within_bound(
            &lane,
            first.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;

        double.refuse_changes();
        double.release_one();
        lane.request(change_naming_lib()?, Arc::clone(&second));
        let cause = owed_within_bound(&lane, second.reads.tree_revision()).await?;
        assert!(
            cause.contains("the double refuses changes"),
            "the owed cause is the store's own: {cause}"
        );
        assert!(
            lane.owes_whole(),
            "a failed change leaves a whole comparison owed"
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

        double.accept_changes();
        double.release_one();
        lane.request(change_naming_lib()?, Arc::clone(&third));
        let calls = double.calls_within_bound(5).await?;
        assert_eq!(
            calls,
            vec![
                ("clear", String::new()),
                ("apply", first.reads.tree_revision().to_owned()),
                ("apply", second.reads.tree_revision().to_owned()),
                ("clear", String::new()),
                ("apply", third.reads.tree_revision().to_owned()),
            ],
            "the publication after a failed change compares its whole tree with the store"
        );
        commit_state_within_bound(
            &lane,
            third.reads.tree_revision(),
            LexicalCommitState::Settled,
        )
        .await?;
        assert!(
            !lane.owes_whole(),
            "a landed whole comparison pays what was owed"
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
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        let write = change_naming_lib()?;
        let deadline = commit_deadline(1);
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
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        // A store holding nothing it can keep is cleared before a whole write lands, so the
        // transaction waits at the gate once the apply follows the clear.
        assert_eq!(
            double.calls_within_bound(2).await?,
            vec![
                ("clear", String::new()),
                ("apply", published.reads.tree_revision().to_owned()),
            ]
        );
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
            return Err("an aborted transaction leaves a whole comparison owed".into());
        };
        assert!(
            cause.contains("the lexical lane ended while the transaction ran"),
            "the owed cause names the cancellation: {cause}"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_lane_aborts_a_transaction_past_its_deadline() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = candidate_declaring(directory.path(), 0, "beacon")?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        // A store holding nothing it can keep is cleared before a whole write lands, so the
        // transaction waits at the gate once the apply follows the clear.
        assert_eq!(
            double.calls_within_bound(2).await?,
            vec![
                ("clear", String::new()),
                ("apply", published.reads.tree_revision().to_owned()),
            ]
        );
        assert_eq!(
            double.dropped_while_held(),
            0,
            "the held write's future lives while the lane runs"
        );

        tokio::time::sleep(LEXICAL_COMMIT_TIMEOUT + Duration::from_millis(1)).await;
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
            return Err("an aborted transaction leaves a whole comparison owed".into());
        };
        assert!(
            cause.contains("the lexical lane ended while the transaction ran"),
            "the owed cause names the cancellation: {cause}"
        );
        Ok(())
    }

    /// Bounds that write every file in a transaction of its own.
    const ONE_FILE_PER_PART: super::LexicalLaneBounds = super::LexicalLaneBounds {
        unit_bytes_max: usize::MAX,
        transaction_units_max: 1,
        transaction_bytes_max: usize::MAX,
    };

    /// A tree of three declaring files and the publication of it.
    fn three_file_publication(root: &std::path::Path) -> TestResult<Arc<PublishedWorkspace>> {
        for (file, declaration) in [
            ("a.rs", "alphaone"),
            ("b.rs", "betatwo"),
            ("c.rs", "gammathree"),
        ] {
            fs::write(root.join(file), format!("pub fn {declaration}() {{}}\n"))?;
        }
        stable_candidate(root, 0)
    }

    #[tokio::test]
    async fn a_write_past_the_transaction_bound_commits_in_parts_and_stamps_the_last() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        let published = three_file_publication(directory.path())?;
        let revision = published.reads.tree_revision().to_owned();
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let double = StoreDouble::new();
        double.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            ONE_FILE_PER_PART,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            published,
        );

        double.release_one();
        double.calls_within_bound(3).await?;
        assert_eq!(
            index.tree_revision().await?,
            None,
            "a part before the last stamps no publication"
        );
        assert_eq!(lane.commit_state(&revision), LexicalCommitState::Committing);

        for _part in 0..16 {
            double.release_one();
        }
        commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;
        let calls = double.calls();
        let applies: Vec<&String> = calls
            .iter()
            .filter(|(form, _)| *form == "apply")
            .map(|(_, stamped)| stamped)
            .collect();
        assert!(applies.len() >= 3, "every file commits alone: {calls:?}");
        let (last, earlier) = applies.split_last().ok_or("the write reached the store")?;
        assert!(
            earlier.iter().all(|stamped| stamped.is_empty()),
            "{calls:?}"
        );
        assert_eq!(**last, revision, "the last part stamps the publication");
        for declaration in ["alphaone", "betatwo", "gammathree"] {
            assert!(
                !ranked_at(&index, &revision, declaration, 8)
                    .await?
                    .is_empty()
            );
        }
        cancellation.cancel();
        ended_within_bound(&lane).await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_stop_between_parts_keeps_whole_files_and_the_next_start_writes_the_rest()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = three_file_publication(directory.path())?;
        let revision = published.reads.tree_revision().to_owned();
        let derivation = super::derivation_revision(
            super::lexical_double::PRODUCT_VERSION,
            &published.configuration,
        );
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let stopped = StoreDouble::new();
        stopped.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&stopped),
            ONE_FILE_PER_PART,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            Arc::clone(&published),
        );
        stopped.release_one();
        stopped.calls_within_bound(3).await?;
        cancellation.cancel();
        ended_within_bound(&lane).await?;
        assert_eq!(
            stopped.dropped_while_held(),
            1,
            "the stop aborts the held part"
        );
        assert_eq!(index.tree_revision().await?, None);
        let kept = index
            .recorded_lexical_files(&derivation)
            .await?
            .ok_or("the committed part recorded its file")?;
        assert_eq!(kept.len(), 1, "one part landed before the stop");

        let restarted = StoreDouble::new();
        restarted.attach(Arc::clone(&index));
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&restarted),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        restarted.release_one();
        lane.request(
            super::lexical_write(&published, &ChangeSet::Full),
            published,
        );
        commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;
        let written = restarted.applied().pop().ok_or("the restart wrote once")?;
        for (path, _digest) in kept.iter() {
            assert!(
                !written.contains(path),
                "{path} was already written: {written:?}"
            );
        }
        assert!(
            restarted.calls().iter().all(|(form, _)| *form != "clear"),
            "the restart keeps the rows the stopped write left"
        );
        assert_eq!(
            index.tree_revision().await?.as_deref(),
            Some(revision.as_str())
        );
        for declaration in ["alphaone", "betatwo", "gammathree"] {
            assert!(
                !ranked_at(&index, &revision, declaration, 8)
                    .await?
                    .is_empty()
            );
        }
        cancellation.cancel();
        Ok(())
    }

    /// Distinct full analyzer digests used to prove persisted derivation changes.
    pub(crate) const ANALYZER_A: &str =
        "006b8433ba06679f06a3c7f0743d65634d32c340000000000000000000000000000";
    pub(crate) const ANALYZER_B: &str =
        "006b8433ba06679f06a3c7f0743d65634d32c340000000000000000000000000001";

    /// Spawns a lane over `index` deriving under `analyzer_revision`, hands it one
    /// whole write of `published`, and returns the store double once the write settled.
    async fn whole_write_under(
        index: &Arc<SearchIndex>,
        published: &Arc<PublishedWorkspace>,
        analyzer_revision: &str,
    ) -> TestResult<Arc<StoreDouble>> {
        let revision = published.reads.tree_revision().to_owned();
        let store = StoreDouble::new();
        store.attach(Arc::clone(index));
        let cancellation = CancellationToken::new();
        let _cancel = cancellation.clone().drop_guard();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&store),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(analyzer_revision),
        );
        store.release_one();
        lane.request(
            super::lexical_write(published, &ChangeSet::Full),
            Arc::clone(published),
        );
        commit_state_within_bound(&lane, &revision, LexicalCommitState::Settled).await?;
        Ok(store)
    }

    /// A restart preserves stored rows while their analyzer and configuration agree.
    #[tokio::test]
    async fn a_restart_keeps_rows_under_one_analyzer_and_rederives_under_another() -> TestResult {
        let directory = tempfile::tempdir()?;
        let published = three_file_publication(directory.path())?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);

        let first = whole_write_under(&index, &published, ANALYZER_A).await?;
        let first_written = first.applied().concat();
        assert_eq!(first_written.len(), 3, "the first start derives every file");

        let restarted = whole_write_under(&index, &published, ANALYZER_A).await?;
        assert!(
            restarted.calls().iter().all(|(form, _)| *form != "clear"),
            "the same analyzer keeps the store: {:?}",
            restarted.calls()
        );
        assert!(
            restarted.applied().concat().is_empty(),
            "no stored row is derived again: {:?}",
            restarted.applied()
        );

        let changed = whole_write_under(&index, &published, ANALYZER_B).await?;
        assert!(
            changed.calls().iter().any(|(form, _)| *form == "clear"),
            "another analyzer clears the store: {:?}",
            changed.calls()
        );
        let mut rederived = changed.applied().concat();
        rederived.sort();
        let mut expected = first_written;
        expected.sort();
        assert_eq!(
            rederived, expected,
            "another analyzer derives every file again"
        );
        Ok(())
    }

    #[test]
    fn the_derivation_revision_moves_with_the_full_analyzer_and_each_index_owned_table()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let accepted = |text: Option<&str>| -> TestResult<super::ConfigurationState> {
            let path = directory.path().join("rift.toml");
            match text {
                Some(text) => fs::write(&path, text)?,
                None => {
                    let _ = fs::remove_file(&path);
                }
            }
            Ok(super::ConfigurationState::accept(directory.path()))
        };
        let base = accepted(None)?;
        let revision = super::derivation_revision(ANALYZER_A, &base);
        assert_eq!(revision, super::derivation_revision(ANALYZER_A, &base));
        assert_ne!(revision, super::derivation_revision(ANALYZER_B, &base));
        for table in [
            "[source]\nfiles = 1000\n",
            "[search.text]\nmax_chunk = \"2kb\"\n",
            "[search.lexical]\ntransaction_units = 200\n",
            "[providers.syntax]\nmax_file = \"60b\"\n",
            "[languages.rust]\nenabled = false\n",
        ] {
            let state = accepted(Some(table))?;
            assert!(state.accepted.is_ok(), "{table} must be accepted");
            assert_ne!(
                super::derivation_revision(ANALYZER_A, &state),
                revision,
                "{table} decides the rows"
            );
        }
        let server = accepted(Some("[server]\nnum_workers = 2\n"))?;
        assert!(server.accepted.is_ok());
        assert_eq!(
            super::derivation_revision(ANALYZER_A, &server),
            revision,
            "a table that derives nothing keeps the stored rows"
        );
        Ok(())
    }

    #[test]
    fn a_syntax_bound_change_is_an_index_configuration_change() -> TestResult {
        let directory = tempfile::tempdir()?;
        let before = super::ConfigurationState::accept(directory.path());
        fs::write(
            directory.path().join("rift.toml"),
            "[providers.syntax]\nmax_file = \"60b\"\n",
        )?;
        let after = super::ConfigurationState::accept(directory.path());
        assert!(after.accepted.is_ok());
        assert!(before.index_configuration_differs(&after));
        Ok(())
    }

    #[tokio::test]
    async fn writes_handed_while_one_runs_merge_into_one_write_answering_for_each() -> TestResult {
        let directory = tempfile::tempdir()?;
        let publications = declaring_publications(directory.path(), 4)?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);

        // The first write runs and holds the lane at the store's gate.
        lane.request(change_naming_lib()?, Arc::clone(&publications[0]));
        double.calls_within_bound(1).await?;
        for publication in &publications[1..] {
            lane.request(change_naming_lib()?, Arc::clone(publication));
        }
        assert!(!lane.owes_whole(), "merging held writes owes nothing");
        for publication in &publications {
            assert_eq!(
                lane.commit_state(publication.reads.tree_revision()),
                LexicalCommitState::Committing
            );
        }
        let recorded = queued_records(&mut drain);
        assert!(
            recorded
                .iter()
                .any(|record| record.message().contains("merged this write")),
            "the merge is recorded"
        );

        double.release_one();
        double.release_one();
        let newest = &publications[3];
        let calls = double.calls_within_bound(2).await?;
        assert_eq!(
            calls,
            vec![
                ("apply", publications[0].reads.tree_revision().to_owned()),
                ("apply", newest.reads.tree_revision().to_owned()),
            ],
            "the held writes land as one, stamped with the newest revision"
        );
        for publication in &publications[1..] {
            commit_state_within_bound(
                &lane,
                publication.reads.tree_revision(),
                LexicalCommitState::Settled,
            )
            .await?;
        }
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
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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

    /// A write merged into the held one wakes the waiters, which read every merged revision
    /// as still committing rather than settled.
    #[tokio::test]
    async fn a_merging_write_wakes_the_waiters_on_the_landing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let publications = declaring_publications(directory.path(), 3)?;
        let double = StoreDouble::new();
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn_over(
            Arc::clone(&double),
            super::lexical_double::UNBOUNDED,
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        lane.request(change_naming_lib()?, Arc::clone(&publications[0]));
        double.calls_within_bound(1).await?;
        lane.request(change_naming_lib()?, Arc::clone(&publications[1]));
        let held = publications[1].reads.tree_revision();
        let landed = lane.landed();

        lane.request(change_naming_lib()?, Arc::clone(&publications[2]));
        tokio::time::timeout(LEXICAL_COMMIT_TIMEOUT / 10, landed)
            .await
            .map_err(|_| "a merging hand-off wakes the waiters")?;
        assert_eq!(
            lane.commit_state(held),
            LexicalCommitState::Committing,
            "the woken waiter reads the merged revision as still committing"
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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
            Arc::from(super::lexical_double::PRODUCT_VERSION),
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

    /// Settles an empty observation before a test holds the supervisor's worker pool.
    /// The published epoch proves startup work has finished and the event loop is serving.
    async fn supervisor_started_within_bound(
        validation: &IndexValidation,
        published: &RwLock<IndexState>,
    ) -> TestResult<u64> {
        let epoch = validation.observe_paths([])?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let changed = validation.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if published.read().await.snapshot().0.epoch >= epoch {
                    return;
                }
                changed.await;
            }
        })
        .await
        .map_err(|_| "the supervisor must settle its startup observation within three seconds")?;
        Ok(epoch)
    }

    /// Polls `index` until it carries `revision`, which the lane's pass stamps.
    ///
    /// # Errors
    ///
    /// Returns the stamp the store still carried once the bound runs out.
    /// One index whose vector ranking is enabled but holds no model, so a pass records the
    /// declaration count it was handed as its readiness rather than embedding anything.
    ///
    /// That count is the population lane's observable: with the tier disabled a pass leaves
    /// no trace at all, and the lexical stamp now belongs to the lexical lane.
    async fn counting_index(database: &std::path::Path) -> TestResult<SearchIndex> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default()).build();
        let index = SearchIndex::open(database, limits).await?;
        assert_eq!(
            index.pass_readiness(),
            VectorReadiness::Preparing {
                prepared: 0,
                total: 0
            }
        );
        Ok(index)
    }

    /// The identities one revision-qualified store answer placed in its precise phase,
    /// refusing an answer the store could not place under `tree_revision`.
    async fn ranked_at(
        index: &SearchIndex,
        tree_revision: &str,
        query: &str,
        limit: u32,
    ) -> TestResult<Vec<DocumentIdentity>> {
        let parsed = ParsedQuery::parse(query)?;
        let ranking = index
            .rank(tree_revision, &parsed, QueryPhase::Precise, limit)
            .await?;
        match ranking {
            RevisionScoped::Matched(ranking) => Ok(ranking
                .inputs()
                .iter()
                .flat_map(RankingInput::order)
                .map(|ranked| ranked.identity().clone())
                .collect()),
            other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
        }
    }

    /// Waits until the lane's readiness names `total` declarations.
    async fn described_within_bound(index: &SearchIndex, total: u64) -> TestResult {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            if index.pass_readiness() == (VectorReadiness::Preparing { prepared: 0, total }) {
                return Ok(());
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        Err(format!(
            "the lane never recorded {total} declarations; readiness is {:?}",
            index.pass_readiness()
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

        let units = published.reads.index_documents();
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

        let units = newest.reads.index_documents();
        let described = newest.reads.described_units(&units).len() as u64;
        lane.request(Arc::clone(&earlier));
        lane.request(Arc::clone(&newest));
        described_within_bound(&index, described).await?;

        cancellation.cancel();
        Ok(())
    }

    /// Reads a lane that has settled takes before it accepts that nothing else lands.
    ///
    /// The lane owns one task, so a request it never took cannot arrive later. A few
    /// reads past the settled count are enough to catch one that did.
    const SETTLED_READS: usize = 5;

    /// Polls until the lane's readiness names `total` declarations, then holds it there.
    ///
    /// Reaching the count proves the pass ran; staying at it proves no later pass
    /// replaced that tree with another one.
    async fn settled_on_described(index: &SearchIndex, total: u64) -> TestResult {
        described_within_bound(index, total).await?;
        for _attempt in 0..SETTLED_READS {
            assert_eq!(
                index.pass_readiness(),
                VectorReadiness::Preparing { prepared: 0, total },
                "a pass for another tree replaced the one the lane settled on"
            );
            tokio::time::sleep(LANE_POLL).await;
        }
        Ok(())
    }

    /// Two publications, the older asked for second, which is the order the vector
    /// preparation can ask in: it reads the current publication before it asks, so a
    /// publication landing in between leaves it holding the older one.
    ///
    /// The lane must stay on the newer tree. Running the older one would embed a tree no
    /// request is answered from, and no later ask would correct it: the newer publication
    /// has already been sent, so nothing else will ask for it.
    #[tokio::test(flavor = "current_thread")]
    async fn an_older_publication_does_not_displace_the_newest_one() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn lantern() {}\n",
        )?;
        let earlier = stable_candidate(directory.path(), 0)?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let newest = stable_candidate(directory.path(), 1)?;
        let index = Arc::new(counting_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = PopulationLane::spawn(Arc::clone(&index), cancellation.clone());

        lane.request(Arc::clone(&newest));
        lane.request(Arc::clone(&earlier));
        settled_on_described(&index, 1).await?;

        cancellation.cancel();
        Ok(())
    }

    /// The same publication asked for twice runs twice. Startup asks once as it publishes
    /// and the vector preparation asks again once its model is held, and that second ask
    /// is what gives a workspace nobody writes to its vectors.
    ///
    /// The two candidates carry one epoch and different trees, which no publication
    /// sequence mints: it is how a test tells which of two asks at one epoch the lane ran.
    #[tokio::test(flavor = "current_thread")]
    async fn a_publication_asked_for_again_at_one_epoch_runs_again() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn lantern() {}\n",
        )?;
        let asked = stable_candidate(directory.path(), 1)?;
        let index = Arc::new(counting_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = PopulationLane::spawn(Arc::clone(&index), cancellation.clone());

        lane.request(Arc::clone(&asked));
        described_within_bound(&index, 2).await?;

        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let asked_again = stable_candidate(directory.path(), 1)?;
        lane.request(Arc::clone(&asked_again));
        described_within_bound(&index, 1).await?;

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
        let units = earlier.reads.index_documents();
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
            index.pass_readiness(),
            VectorReadiness::Preparing {
                prepared: 0,
                total: described
            },
            "a request the ended lane refused must leave the previous pass's readiness alone"
        );
        Ok(())
    }

    #[tokio::test]
    async fn disabled_population_keeps_lexical_ranking_and_chunk_warnings() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("guide.txt"), "word ".repeat(1000))?;
        fs::write(
            directory.path().join("rift.toml"),
            "[search.text]\nmax_chunk = \"1kb\"\n",
        )?;
        let published = stable_candidate(directory.path(), 0)?;
        let index = Arc::new(search_index(&directory.path().join("search.db")).await?);
        let cancellation = CancellationToken::new();
        let lane = LexicalLane::spawn(
            Arc::clone(&index),
            BlockingExecutor::isolated(2, 60_000),
            cancellation.clone(),
            Arc::from(super::lexical_double::PRODUCT_VERSION),
        );
        committed_through(
            &lane,
            &index,
            super::lexical_write(&published, &ChangeSet::Full),
            &published,
        )
        .await?;
        let revision = published.reads.tree_revision();
        let before = ranked_at(&index, revision, "beacon", 8).await?;
        assert!(
            !before.is_empty(),
            "the lexical lane must publish the declaration"
        );
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);
        super::populate_search(&index, &published, rift_search::Embedding::Every).await;
        assert_eq!(index.pass_readiness(), VectorReadiness::Disabled);
        assert_eq!(index.tree_revision().await?.as_deref(), Some(revision));
        assert_eq!(ranked_at(&index, revision, "beacon", 8).await?, before);
        let records = queued_records(&mut drain);
        let warning = records
            .iter()
            .find(|record| {
                record.message().contains("was indexed in chunks")
                    && record.fields().contains("guide.txt")
            })
            .ok_or("a disabled vector ranking must still report the chunked guide")?;
        assert_eq!(warning.level(), "warn");
        assert_eq!(warning.component(), "search");
        cancellation.cancel();
        Ok(())
    }

    /// One search index over `database` with the vector ranking off, so a test drives the
    /// full-text half without acquiring model weights.
    async fn search_index(database: &std::path::Path) -> TestResult<SearchIndex> {
        let limits = SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_vector()
            .build();
        let index = SearchIndex::open(database, limits).await?;
        assert_eq!(index.pass_readiness(), VectorReadiness::Disabled);
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
        let watcher = unwatched(directory.path(), &validation)?;

        let blocking = crate::server::BlockingExecutor::isolated(1, 60_000);
        let supervisor = tokio::spawn(super::run_index_supervisor(
            watcher,
            invalidations,
            cancellation_context(directory.path(), &validation, &published, &blocking),
        ));
        let startup_epoch = supervisor_started_within_bound(&validation, &published).await?;

        // One blocking slot, held by a placeholder so the supervisor's own rebuild for
        // the next epoch is forced to queue behind it - a deterministic gate between the
        // supervisor capturing that epoch and `accept_rebuild` checking it, exactly where a
        // second observation must land to supersede the rebuild.
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

        let first_epoch = validation
            .observe_whole_workspace()
            .map_err(|error| format!("first observation must land: {error:?}"))?;
        assert_eq!(first_epoch, startup_epoch + 1);
        assert!(
            pending_work_taken_within_bound(&validation).await,
            "the supervisor must take the rebuild epoch before the test moves it"
        );

        // Moves the epoch again with no invalidation signal, so no second rebuild cycle is
        // ever triggered - only the already-queued rebuild observes this move.
        let moved = validation.observed_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(moved, first_epoch + 1);

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
            snapshot.epoch, startup_epoch,
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
    ) -> super::IndexSupervisorContext {
        super::IndexSupervisorContext {
            root: root.to_path_buf(),
            limits: WorkspaceIndexLimits::default(),
            published: Arc::clone(published),

            validation: Arc::clone(validation),
            blocking: blocking.clone(),
            population: None,
            lexical: None,
        }
    }

    /// A capture that reports it started, blocks until the test releases it, and then
    /// scans the workspace as production would.
    ///
    /// The release is a rendezvous: it succeeds only while the capture still waits in
    /// it, so a successful release proves the capture was blocking at that moment.
    fn held_capture() -> (
        impl super::CaptureWorkspace + Clone + Send + 'static,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (started, started_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_receiver) = std::sync::mpsc::sync_channel::<()>(0);
        let release_receiver = Arc::new(std::sync::Mutex::new(release_receiver));
        let scan = super::workspace_capture();
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
        let context = cancellation_context(directory.path(), &validation, &published, &blocking);
        let (capture, mut started, release) = held_capture();
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
        assert!(
            validation.superseded_after(snapshot.epoch).is_none(),
            "cancellation cannot let reads answer stale"
        );
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
        let watcher = unwatched(directory.path(), &validation)?;
        let blocking = BlockingExecutor::isolated(1, 60_000);
        let context = cancellation_context(directory.path(), &validation, &published, &blocking);
        let (capture, mut started, release) = held_capture();
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
                build_workspace_candidate(root, limits, request)
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
        let work = fixture.validation.take_pending().work;
        assert_eq!(
            super::finish_rebuild(
                fixture.root(),
                &fixture.state,
                &fixture.validation,
                &fixture.after,
                &ChangeSet::Full,
                work,
                None,
            ),
            RebuildOutcome::Cancelled
        );
        assert!(
            fixture
                .validation
                .superseded_after(fixture.before.epoch)
                .is_none(),
            "cancelled publication cannot let reads answer stale"
        );
        let state = fixture.state.blocking_read();
        assert_workspace_identity(&state.current, &fixture.before);
        Ok(())
    }

    type RelationshipRow = (String, String, String, String, u64, u64, Option<String>);

    /// Complete indexed facts exposed by one immutable read publication.
    #[derive(Debug, PartialEq)]
    struct PublishedFacts {
        digests: rift_index::WorkspaceDigests,
        map: WorkspaceMap,
        documents: Vec<IndexDocument>,
        documentation: serde_json::Value,
        symbols: BTreeSet<String>,
        names: BTreeSet<String>,
        symbol_reads: BTreeMap<String, Vec<serde_json::Value>>,
        relationships: BTreeSet<RelationshipRow>,
    }

    fn published_facts(published: &PublishedWorkspace) -> TestResult<PublishedFacts> {
        let reads = &published.reads;
        let documents = reads.index_documents();
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        for document in documents
            .iter()
            .filter(|document| document.kind() == DocumentKind::Symbol)
        {
            ids.insert(SymbolId::new(document.identity().as_str())?);
            if let Some(name) = document.fields().get(SearchableField::Name) {
                names.insert(name.to_owned());
            }
        }

        let mut symbol_reads = BTreeMap::new();
        for name in &names {
            let mut pages = Vec::new();
            let mut page_index = 0;
            loop {
                let params = serde_json::from_value(serde_json::json!({
                    "name": name,
                    "scope": "local",
                    "include": ["source", "documentation"],
                    "limit": 64,
                    "page_index": page_index,
                }))?;
                let result = reads.get_symbol(&params)?;
                if !result.warnings.is_empty() {
                    return Err(format!(
                        "symbol read is incomplete for {name}: {:?}",
                        result.warnings
                    )
                    .into());
                }
                let total_pages = result.pagination.total_pages;
                if total_pages > 32 {
                    return Err(format!("symbol read exceeded 32 pages: {name}").into());
                }
                pages.push(serde_json::to_value(&result)?);
                page_index += 1;
                if page_index >= total_pages {
                    break;
                }
                if page_index > 32 {
                    return Err(format!("symbol read exceeded 32 pages: {name}").into());
                }
            }
            symbol_reads.insert(name.clone(), pages);
        }

        let store = reads.relationships();
        if !store.is_complete() || store.dropped_edges() != 0 {
            return Err(format!(
                "relationship store is incomplete: dropped_edges={}",
                store.dropped_edges()
            )
            .into());
        }
        let mut relationships = BTreeSet::new();
        for id in &ids {
            for edge in store.outgoing(id).iter().chain(store.incoming(id)) {
                let occurrence = edge.occurrence();
                let range = occurrence.range();
                relationships.insert((
                    edge.from().as_str().to_owned(),
                    edge.to().as_str().to_owned(),
                    serde_json::to_string(&edge.facet())?,
                    occurrence.unit().to_string(),
                    range.start(),
                    range.end(),
                    occurrence.node().map(|node| node.0.clone()),
                ));
            }
        }
        if relationships.iter().any(|(from, to, ..)| {
            !ids.iter().any(|id| id.as_str() == from) || !ids.iter().any(|id| id.as_str() == to)
        }) {
            return Err("relationship edge names a symbol absent from index documents".into());
        }

        Ok(PublishedFacts {
            digests: reads.workspace_digests(),
            map: reads.workspace_map(),
            documents,
            documentation: serde_json::to_value(reads.documentation_snapshot().index())?,
            symbols: ids.iter().map(ToString::to_string).collect(),
            names,
            symbol_reads,
            relationships,
        })
    }

    async fn publication_matching(
        published: &RwLock<IndexState>,
        validation: &IndexValidation,
        expected: &rift_index::WorkspaceDigests,
    ) -> TestResult<Arc<PublishedWorkspace>> {
        for _attempt in 0..LANE_ATTEMPTS_MAX {
            let current = Arc::clone(&published.read().await.current);
            if current.reads.workspace_digests() == *expected {
                return Ok(current);
            }
            tokio::time::sleep(LANE_POLL).await;
        }
        let state = published.read().await;
        let observed = state.current.reads.workspace_digests();
        let published_epoch = state.current.epoch;
        let failure = state
            .failure
            .as_ref()
            .map(|(epoch, error)| (*epoch, error.to_string()));
        drop(state);
        let pending = validation
            .publication_lane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Err(format!(
            "the native watcher did not publish the complete changed tree: \
             expected={expected:?}, observed={observed:?}, \
             published_epoch={published_epoch}, observed_epoch={}, \
             watch_failed={}, supervisor_running={}, failure={failure:?}, pending={pending:?}",
            validation.observed_epoch(),
            validation.watch_failed.load(Ordering::Acquire),
            validation.supervisor_running.load(Ordering::Acquire),
        )
        .into())
    }

    /// A failed observation reports the publication, pending work, and recorded failure.
    #[tokio::test(start_paused = true)]
    async fn native_publication_timeout_reports_observed_state() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::write(root.join("lib.rs"), "pub fn old() {}\n")?;
        let before = stable_candidate(root, 0)?;
        fs::write(root.join("lib.rs"), "pub fn new() {}\n")?;
        let expected = stable_candidate(root, 1)?.reads.workspace_digests();
        let (validation, _invalidations) = IndexValidation::new(4);
        let epoch = validation.observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let state = RwLock::new(IndexState {
            current: before,
            failure: Some((
                epoch,
                Arc::new(ReadFault::unavailable("test rebuild", "held failure")),
            )),
        });
        let error = publication_matching(&state, &validation, &expected)
            .await
            .expect_err("a changed tree must not match the old publication");
        let message = error.to_string();
        for field in [
            "expected=WorkspaceDigests",
            "observed=WorkspaceDigests",
            "published_epoch=0",
            "observed_epoch=1",
            "watch_failed=false",
            "supervisor_running=",
            "held failure",
            "pending=PendingWork",
            "lib.rs",
        ] {
            assert!(
                message.contains(field),
                "missing diagnostic {field}: {message}"
            );
        }
        Ok(())
    }

    /// A native workspace watcher publishes complete facts equal to a cold build.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn native_watcher_publication_matches_cold_indexed_facts() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        fs::create_dir(root.join("src"))?;
        fs::write(
            root.join("src/lib.rs"),
            "pub struct Beacon;\npub fn old() {}\n",
        )?;
        fs::write(root.join("src/removed.rs"), "pub struct Removed;\n")?;
        fs::write(root.join("README.md"), "See [Beacon](src/lib.rs#Beacon).\n")?;

        let (context, invalidations) = initial_preparation_context(root)?;
        let validation = Arc::clone(&context.validation);
        let watcher = super::workspace_watcher(root, &validation)?;
        let initial = super::discover_initial_workspace(&context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        super::prepare_initial_workspace_from(&context, initial).await?;
        let state = Arc::clone(&context.published);
        let startup = Arc::clone(&state.read().await.current);
        assert!(
            !startup
                .reads
                .documentation_snapshot()
                .index()
                .references
                .is_empty(),
            "fixture must publish documentation references"
        );
        let startup_facts = published_facts(&startup)?;
        let cold_a = stable_candidate(root, 0)?;
        assert!(
            startup_facts.relationships.is_empty(),
            "fixture semantic edges are empty"
        );
        let cold_a_facts = published_facts(&cold_a)?;
        assert_eq!(
            startup_facts, cold_a_facts,
            "startup publication facts must equal an independent cold build"
        );

        let supervisor = tokio::spawn(super::run_index_supervisor(watcher, invalidations, context));

        let observed = async {
            fs::write(
                root.join("src/lib.rs"),
                "pub struct Beacon;\npub fn new() {}\n",
            )?;
            fs::remove_file(root.join("src/removed.rs"))?;
            fs::write(root.join("src/added.rs"), "pub struct Added;\n")?;
            fs::write(root.join("README.md"), "See [new](src/lib.rs#new).\n")?;
            let cold_b = stable_candidate(root, 0)?;
            assert!(
                !cold_b
                    .reads
                    .documentation_snapshot()
                    .index()
                    .references
                    .is_empty(),
                "changed fixture must retain documentation references"
            );
            let expected_facts = published_facts(&cold_b)?;
            let expected = &expected_facts.digests;
            let publication = publication_matching(&state, &validation, expected).await?;
            let watched_facts = published_facts(&publication)?;

            assert!(
                publication.epoch > startup.epoch,
                "native watcher must advance publication"
            );
            assert_eq!(
                watched_facts, expected_facts,
                "watched publication facts must equal an independent cold build"
            );
            assert!(
                watched_facts.names.contains("new") && watched_facts.names.contains("Added"),
                "new declarations must be present in the watched publication"
            );
            assert!(
                !watched_facts.names.contains("old") && !watched_facts.names.contains("Removed"),
                "removed declarations must be absent from the watched publication"
            );
            Ok::<(), Box<dyn Error>>(())
        }
        .await;
        validation.cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(5), supervisor).await??;
        observed
    }
}
