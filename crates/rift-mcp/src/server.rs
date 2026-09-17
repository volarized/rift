use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rift_core::SourceVisibility;
use rift_dependency::DependencyCatalog;
use rift_index::{
    DependencyIndex, DependencyIndexLimits, LexicalIndexLimits, LogStore, PackageSelection,
    PathChanges, WorkspaceDigests, WorkspaceIndexLimits, capture_digests_with_languages,
};
use rift_protocol::configuration::{
    Duration as WireDuration, LspConfiguration, SEARCH_BUSY_TIMEOUT_MS_MAX, SEARCH_POOL_SLOTS_MAX,
    SERVER_NUM_WORKERS_MAX, SearchConfiguration, SemanticSearchConfiguration, SemanticSource,
    ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_protocol::lock::ProductIdentity;
use rift_protocol::read::{
    Digest, GetSymbolParams, GetSymbolResult, Language, NodesParams, NodesResult, Pagination,
    ProjectPath, ReadWarning, SearchParams, SearchResult, SearchScope,
};
use rift_protocol::workspace::{
    WORKSPACE_SOURCE_UNITS_MAX, WorkspaceLanguageSummary, WorkspaceLspSummary,
    WorkspaceResourcePage, WorkspaceSourceUnit,
};
use rift_search::{
    AcquisitionLimits, FusedRanking, ModelSource, RankedUnit, RevisionScoped, SearchError,
    SearchIndex, SearchIndexLimits, SemanticReadiness,
};
use rift_server::{
    DependencyStore, EnginePool, EngineReferences, LspProcessKey, ReadError, ReadFault,
    ReadService, resolve_engine_references, uses_engine_references, wire_digest,
};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    Implementation, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, ServerCapabilities,
    ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::dependency::{DependencyLane, DependencyRequest};
use crate::failure::WireFailure;
use crate::parameters::Parameters;
use crate::resource;
use crate::storage::WorkspaceStorage;
use crate::validation::{
    ConfigurationFingerprint, ConfigurationState, INDEX_CAPTURE_ATTEMPTS_MAX, IndexState,
    IndexSupervisor, IndexSupervisorContext, IndexValidation, LexicalCommitState, LexicalLane,
    LexicalWrite, PopulationLane, PublishedWorkspace, configuration_fingerprint, initial_workspace,
    run_index_supervisor, workspace_watcher,
};

/// Semantic candidates one file may contribute to a fused ranking.
///
/// Provisional: the `[search.semantic] per_file_max` key replaces it once that key lands,
/// and [`search_index_limits`] is the one place that reads it, so the swap is one line
/// there and nowhere else. Three lets a module that genuinely answers the query place its
/// overloads without one file's declarations filling the whole candidate list on their own.
const SEMANTIC_PER_FILE_MAX: u64 = 3;

/// Files a workspace holds before [`PREPARATION_SPAN_LARGE`] applies.
const PREPARATION_FILES_LARGE: u64 = 10_000;
/// Files a workspace holds before [`PREPARATION_SPAN_MEDIUM`] applies.
const PREPARATION_FILES_MEDIUM: u64 = 5_000;
/// Files a workspace holds before [`PREPARATION_SPAN_SMALL`] applies.
const PREPARATION_FILES_SMALL: u64 = 1_000;

/// Preparing a workspace past [`PREPARATION_FILES_LARGE`]: a couple of minutes.
const PREPARATION_SPAN_LARGE: WireDuration = WireDuration::from_millis(120_000);
/// Preparing a workspace past [`PREPARATION_FILES_MEDIUM`]: around a minute.
const PREPARATION_SPAN_MEDIUM: WireDuration = WireDuration::from_millis(60_000);
/// Preparing a workspace past [`PREPARATION_FILES_SMALL`]: several seconds.
const PREPARATION_SPAN_SMALL: WireDuration = WireDuration::from_millis(10_000);
/// Preparing a workspace no larger than [`PREPARATION_FILES_SMALL`]: a few seconds.
const PREPARATION_SPAN_MINIMAL: WireDuration = WireDuration::from_millis(3_000);

/// Wait before a model file's second download attempt. No `[search.semantic]` key sets it;
/// each further attempt doubles the wait up to [`MODEL_RETRY_DELAY_LIMIT`].
const MODEL_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Wait no model-download retry grows past. No `[search.semantic]` key sets it: the
/// download's own `download_timeout` is the budget an operator tunes.
const MODEL_RETRY_DELAY_LIMIT: Duration = Duration::from_secs(30);

/// Bounded Tokio acceptance for blocking filesystem and parser work.
#[derive(Clone, Debug)]
pub(crate) struct BlockingExecutor {
    pub(crate) operations: Arc<Semaphore>,
    pub(crate) queue_timeout_ms: u64,
}

impl BlockingExecutor {
    /// Sizes the workspace's pool and queue wait from one accepted
    /// `[server]` table.
    pub(crate) fn for_configuration(server: &ServerConfiguration) -> Self {
        // Acceptance bounds the value to 1..=SERVER_NUM_WORKERS_MAX, so
        // the clamp only guards the usize conversion.
        let workers = usize::try_from(server.num_workers.min(SERVER_NUM_WORKERS_MAX))
            .unwrap_or(1)
            .max(1);
        Self {
            operations: Arc::new(Semaphore::new(workers)),
            queue_timeout_ms: server.worker_queue_timeout.milliseconds(),
        }
    }

    /// Creates an isolated executor for deterministic capacity tests.
    #[cfg(test)]
    pub(crate) fn isolated(operations_max: usize, queue_timeout_ms: u64) -> Self {
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

    /// Runs one blocking operation after queued, bounded acceptance.
    pub(crate) async fn run<Output>(
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

/// Resolves the workspace root the server serves into an absolute path.
///
/// The CLI serves the process working directory, which it names `.`, and every
/// filesystem operation below the root resolves against that same directory, so
/// a relative root reads correctly. A language engine does not: it is
/// addressed in `file://` URIs, which carry no working directory, so a relative
/// root refuses every engine-backed operation. Resolution happens here, where the
/// server takes ownership of the root, rather than at each entry point that could
/// hand one over.
///
/// The operation is lexical: the working directory is prepended, `.` segments drop
/// out, and each `..` cancels the segment before it. Symbolic links keep the
/// spelling the caller used - a link is never followed - because a language engine
/// answers under the root it was handed, and the two must still be the same path.
/// A `..` that follows a link therefore cancels the link's own segment rather than
/// the directory it points at, which is how [`ProjectPath`] already reads a path,
/// and why it refuses `..` in every address below the root.
///
/// [`ProjectPath`]: rift_protocol::path::ProjectPath
fn absolute_root(root: &Path) -> Result<PathBuf, ReadError> {
    let absolute = std::path::absolute(root)
        .map_err(|error| ReadFault::task("workspace root resolution", error.to_string()))?;
    let mut segments: Vec<Component<'_>> = Vec::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            // A `..` above the root has nothing to cancel: `/..` is `/`.
            Component::ParentDir => {
                if matches!(segments.last(), Some(Component::Normal(_))) {
                    segments.pop();
                }
            }
            named => segments.push(named),
        }
    }
    Ok(segments.iter().collect())
}

/// Sizes the lexical search index's unit bound, connection pool, and busy-wait budget from
/// one accepted `[search]` table, keeping this release's fixed unit-byte, query-term, and
/// match-count bounds.
fn lexical_index_limits(search: &SearchConfiguration) -> LexicalIndexLimits {
    let defaults = LexicalIndexLimits::default();
    // Acceptance bounds pool_slots to 1..=SEARCH_POOL_SLOTS_MAX and busy_timeout to
    // SEARCH_BUSY_TIMEOUT_MS_MIN..=SEARCH_BUSY_TIMEOUT_MS_MAX, so these clamps only guard the
    // narrowing conversion into the adapter's `u32` fields.
    let pool_slots = u32::try_from(search.pool_slots.min(SEARCH_POOL_SLOTS_MAX))
        .unwrap_or(1)
        .max(1);
    let busy_timeout_ms = u32::try_from(
        search
            .busy_timeout
            .milliseconds()
            .min(SEARCH_BUSY_TIMEOUT_MS_MAX),
    )
    .unwrap_or(1_000);
    LexicalIndexLimits::new(
        LexicalIndexLimits::accepted_units_max(search.lexical.units_max),
        defaults.unit_bytes_max(),
        defaults.query_terms_max(),
        defaults.matches_max(),
        pool_slots,
        busy_timeout_ms,
    )
}

/// Where one `[search.semantic]` table's weights come from, and what fetching them may
/// spend.
#[derive(Clone, Debug)]
struct ModelAcquisition {
    source: ModelSource,
    limits: AcquisitionLimits,
}

/// The acquisition one `[search.semantic]` table describes, or nothing when the tier is off
/// or its `model` value is one [`ModelSource`] refuses.
///
/// Acceptance bounds `model` by byte length and by the form its declared source sets;
/// `ModelSource` enforces the narrower rule. A value that passes the first and fails the
/// second is a warning and a disabled semantic tier, never a startup failure: the workspace
/// still has a full-text tier.
fn model_acquisition(
    semantic: &SemanticSearchConfiguration,
    root: &Path,
) -> Option<ModelAcquisition> {
    if semantic.disabled {
        return None;
    }
    let source = match semantic_model_source(semantic, root) {
        Ok(source) => source,
        Err(error) => {
            let model = semantic.model.as_str();
            tracing::warn!(
                component = "search",
                operation = "search.prepare",
                model,
                error = %error,
                "the semantic model could not be read; the workspace serves the full-text \
                 tier alone"
            );
            return None;
        }
    };
    Some(ModelAcquisition {
        source,
        limits: acquisition_limits(semantic),
    })
}

/// The model one `[search.semantic]` table names, read as its `source` key declares: a hub
/// repository, or a directory resolved against the workspace root.
///
/// # Errors
///
/// Returns `model_source_invalid` naming the value and the form that was expected.
fn semantic_model_source(
    semantic: &SemanticSearchConfiguration,
    root: &Path,
) -> Result<ModelSource, SearchError> {
    match semantic.source {
        SemanticSource::Hf => ModelSource::repository(&semantic.model),
        SemanticSource::Directory => ModelSource::directory(&semantic.model, root),
    }
}

/// What one model acquisition may spend, from the `[search.semantic]` table.
///
/// Acceptance bounds `download_attempts` to 1 through 10, so the clamp only guards the
/// narrowing conversion. The retry delay and its ceiling are this release's fixed values.
fn acquisition_limits(semantic: &SemanticSearchConfiguration) -> AcquisitionLimits {
    let attempts = u32::try_from(semantic.download_attempts)
        .unwrap_or(1)
        .max(1);
    AcquisitionLimits::new(
        Duration::from_millis(semantic.download_timeout.milliseconds()),
        attempts,
        MODEL_RETRY_DELAY,
        MODEL_RETRY_DELAY_LIMIT,
    )
}

/// Sizes one search index from an accepted `[search]` table and the acquisition its
/// `[search.semantic]` half resolved to.
///
/// `acquisition` is absent when the semantic tier is off or its `model` value could not be
/// read; either way the tier is disabled here, so the index reports `Disabled` rather than
/// a preparation that never runs.
fn search_index_limits(
    search: &SearchConfiguration,
    acquisition: Option<&ModelAcquisition>,
) -> SearchIndexLimits {
    let builder = SearchIndexLimits::builder(lexical_index_limits(search))
        .weights(search.lexical.weight, search.semantic.weight)
        .fusion_k(search.fusion_k)
        .candidates(search.semantic.candidates)
        .max_vectors(search.semantic.max_vectors)
        .batch_declarations(search.semantic.batch_declarations)
        .max_tokens(search.semantic.max_tokens)
        .per_file_max(SEMANTIC_PER_FILE_MAX);
    if acquisition.is_some() {
        builder.build()
    } else {
        builder.disable_semantic().build()
    }
}

/// Attaches the search tiers to the process's one workspace database.
///
/// [`WorkspaceStorage`] opens the database once for every store in the process. The server
/// serves identifier search alone when the database did not open.
fn open_search_index(
    storage: &WorkspaceStorage,
    limits: SearchIndexLimits,
) -> Option<Arc<SearchIndex>> {
    let database = storage.database()?;
    match SearchIndex::attached(database, limits) {
        Ok(index) => Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(
                component = "search",
                operation = "search.open",
                error = %error,
                "the search tiers could not attach to the workspace database; the server \
                 starts without the search index"
            );
            None
        }
    }
}

/// Loads the semantic model behind the answers, so startup waits on nothing.
///
/// A search arriving while this runs is answered by the full-text tier alone and carries the
/// preparation warning. A loaded model then asks the population lane for the pass that
/// embeds the published set, through [`embed_prepared`], because the run's first pass may
/// have run before any model was held.
///
/// The task ends when the server does. It races the same cancellation token the index
/// supervisor runs under, which the last server clone's drop guard cancels.
fn spawn_semantic_preparation(
    index: Arc<SearchIndex>,
    acquisition: ModelAcquisition,
    published: Arc<RwLock<IndexState>>,
    population: PopulationLane,
    cancellation: CancellationToken,
) {
    tokio::spawn(async move {
        let prepared = tokio::select! {
            () = cancellation.cancelled() => return,
            prepared = index.prepare(&acquisition.source, acquisition.limits) => prepared,
        };
        match prepared {
            Ok(()) => embed_prepared(&published, &population).await,
            Err(error) => tracing::warn!(
                component = "search",
                operation = "search.prepare",
                error = %error,
                "the semantic tier could not be prepared; the workspace serves the full-text \
                 tier alone for the life of this server"
            ),
        }
    });
}

/// Asks the population lane for the pass that embeds the published set, once the model is
/// loaded.
///
/// The run's first pass may already have replaced the unit set with no model held, so this
/// request is what gives the workspace its vectors. Without it nothing would embed until
/// the next filesystem event, and a workspace nobody writes to would never rank
/// semantically. The lane runs that pass rather than this task, so a pass a change or the
/// supervisor already asked for is never run twice over.
async fn embed_prepared(published: &RwLock<IndexState>, population: &PopulationLane) {
    tracing::info!(
        component = "search",
        operation = "search.prepare",
        "the semantic tier is prepared"
    );
    let (current, _) = published.read().await.snapshot();
    population.request(current);
}

/// The workspace policy one read of committed source applies: what a revision
/// snapshot and a revision comparison both need before they touch git objects.
struct RevisionRead {
    root: std::path::PathBuf,
    limits: rift_index::WorkspaceIndexLimits,
    visibility: SourceVisibility,
    text_inclusion: rift_core::TextFileInclusion,
    languages: rift_core::LanguageFileSelections,
    history: rift_protocol::configuration::HistoryConfiguration,
}

/// One current-tree request's publication, and the warning it carries when that
/// publication is served in spite of a recorded rebuild failure.
struct ResolvedWorkspace {
    published: Arc<PublishedWorkspace>,
    stale: Option<ReadWarning>,
}

impl ResolvedWorkspace {
    /// A publication that answers for the tree as it stands.
    const fn current(published: Arc<PublishedWorkspace>) -> Self {
        Self {
            published,
            stale: None,
        }
    }
}

/// A read result's warnings, so the one gate every current-tree read passes can add the
/// `stale_index` warning without knowing the result's shape.
pub(crate) trait ReadAnswer {
    /// The warnings this answer carries.
    fn warnings_mut(&mut self) -> &mut Vec<ReadWarning>;
}

impl ReadAnswer for GetSymbolResult {
    fn warnings_mut(&mut self) -> &mut Vec<ReadWarning> {
        &mut self.warnings
    }
}

impl ReadAnswer for SearchResult {
    fn warnings_mut(&mut self) -> &mut Vec<ReadWarning> {
        &mut self.warnings
    }
}

impl ReadAnswer for NodesResult {
    fn warnings_mut(&mut self) -> &mut Vec<ReadWarning> {
        &mut self.warnings
    }
}

/// Most bytes one read warning's `detail` carries: the bound the served schema advertises
/// for it, applied to a failure's rendering before it reaches the wire.
const WARNING_DETAIL_BYTES_MAX: usize = 4096;

/// Most moved paths one `stale_index` detail names before it counts the rest.
const STALE_INDEX_PATHS_MAX: usize = 5;

/// What a read must resolve before its next request-time capture.
#[derive(Clone, Copy)]
enum ReadWait {
    /// The first capture checks content even when observations are ahead.
    Capture,
    /// Changed source waits for publication or a successful superseded capture.
    Rebuild,
    /// Changed configuration waits for acceptance and publication.
    Configuration,
}

/// Why an answer is served from a publication the request-time capture found behind the
/// tree.
enum StaleIndexReason<'a> {
    /// The rebuild recorded past that publication failed.
    RebuildFailed(&'a RecordedRebuildFailure),
    /// A successful capture after the publication was superseded by later changes.
    RebuildSuperseded {
        /// The successful candidate's filesystem epoch.
        epoch: u64,
        /// What this request's exact capture found ahead of the publication.
        changes: &'a PathChanges,
    },
    /// Every reconciliation attempt found the tree moved again, so the request never
    /// captured the tree the publication was built for.
    TreeKeptMoving {
        /// What the last attempt's capture found ahead of the publication.
        changes: &'a PathChanges,
    },
}

impl StaleIndexReason<'_> {
    /// The reason's own rendering, with what the answer was served from, bounded by
    /// `WARNING_DETAIL_BYTES_MAX`.
    fn detail(
        &self,
        index_tree_revision: &Digest,
        captured_tree_revision: &Digest,
        moved: CaptureMovement,
    ) -> String {
        let served = served_from(index_tree_revision, captured_tree_revision, moved);
        let detail = match self {
            Self::RebuildFailed(failure) => failure.detail(&served),
            Self::RebuildSuperseded { epoch, changes } => {
                let found = moved_paths(changes).map_or_else(
                    || "the configuration file moved".to_owned(),
                    |named| format!("{named} moved"),
                );
                format!(
                    "a successful index capture at epoch {epoch} was superseded before \
                     publication; the request-time capture found {found}{served}; \
                     the index supervisor keeps rebuilding the changed files"
                )
            }
            Self::TreeKeptMoving { changes } => {
                let found = moved_paths(changes).map_or_else(
                    || "the configuration file moved".to_owned(),
                    |named| format!("{named} moved"),
                );
                format!(
                    "the workspace changed again on every one of \
                     {INDEX_CAPTURE_ATTEMPTS_MAX} bounded reconciliation attempts, and \
                     the last capture found {found}{served}; the rebuild those changes \
                     asked for publishes next"
                )
            }
        };
        bounded_detail(detail, WARNING_DETAIL_BYTES_MAX)
    }
}

/// The `stale_index` warning an answer served from `published` carries, naming the tree
/// revision `captured` folds to and, when that is the published one, what `moved`
/// instead. `reason` says why the publication answered.
fn stale_index_warning(
    published: &PublishedWorkspace,
    captured: &WorkspaceDigests,
    moved: CaptureMovement,
    reason: &StaleIndexReason<'_>,
) -> ReadWarning {
    let captured_tree_revision = captured
        .tree_revision()
        .unwrap_or_else(|| unreachable!("a classified capture folds its tree revision"));
    let index_tree_revision = Digest(published.reads.tree_revision().to_owned());
    let captured_tree_revision = wire_digest(captured_tree_revision);
    let detail = reason.detail(&index_tree_revision, &captured_tree_revision, moved);
    ReadWarning::StaleIndex {
        index_tree_revision,
        captured_tree_revision,
        detail,
    }
}

/// What the answer was served from: the published snapshot when the syntax-indexed files
/// still fold to its tree revision, and that snapshot rather than the captured tree
/// revision when they do not.
fn served_from(
    index_tree_revision: &Digest,
    captured_tree_revision: &Digest,
    moved: CaptureMovement,
) -> String {
    if index_tree_revision == captured_tree_revision {
        format!(
            "; the syntax-indexed files still fold to tree revision {index}, and {moved}, \
             so the answer was served from the published snapshot",
            index = index_tree_revision.0,
            moved = moved.clause(),
        )
    } else {
        format!(
            ", so the answer was served from the snapshot at tree revision {index} rather \
             than the captured tree revision {captured}",
            index = index_tree_revision.0,
            captured = captured_tree_revision.0,
        )
    }
}

/// The paths one comparison found moved, at most `STALE_INDEX_PATHS_MAX` of them with
/// the rest counted, or nothing when no path moved.
///
/// Shared with the startup capture's own refusal, so both name what moved in one
/// spelling under one bound.
pub(crate) fn moved_paths(changes: &PathChanges) -> Option<String> {
    let mut paths = changes.iter().map(|(path, _)| path.as_str());
    let named: Vec<&str> = paths.by_ref().take(STALE_INDEX_PATHS_MAX).collect();
    if named.is_empty() {
        return None;
    }
    let remaining = paths.count();
    let named = named.join(", ");
    Some(if remaining == 0 {
        named
    } else {
        format!("{named} and {remaining} more")
    })
}

/// The request-time capture a test forces in place of reading the tree.
#[cfg(test)]
type ForcedTreeCapture = dyn Fn(&PublishedWorkspace) -> Result<(WorkspaceDigests, ConfigurationFingerprint), ReadError>
    + Send
    + Sync;

/// The forced capture one server holds, absent in every served run.
///
/// Reaching every retry arm of the bounded reconciliation loop by racing real filesystem
/// events is not reproducible; forcing the capture the loop runs makes each arm a plain
/// function call.
#[cfg(test)]
#[derive(Clone, Default)]
struct ForcedCapture(Arc<std::sync::Mutex<Option<Arc<ForcedTreeCapture>>>>);

#[cfg(test)]
impl ForcedCapture {
    /// The forced capture, when one is installed.
    fn installed(&self) -> Option<Arc<ForcedTreeCapture>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
impl std::fmt::Debug for ForcedCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ForcedCapture")
    }
}

#[cfg(test)]
impl RiftMcp {
    /// Forces `capture` as the request-time capture every later request runs.
    fn force_capture(
        &self,
        capture: impl Fn(
            &PublishedWorkspace,
        ) -> Result<(WorkspaceDigests, ConfigurationFingerprint), ReadError>
        + Send
        + Sync
        + 'static,
    ) {
        *self
            .forced_capture
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(capture));
    }
}

/// What a request-time capture found moved against the publication a read is served
/// from, when the two do not match: the recorded files, the configuration file, or both.
///
/// The `stale_index` detail names it when the syntax-indexed files still fold to the
/// published tree revision, since the two digests the warning carries are then equal
/// and say nothing about what moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureMovement {
    /// Recorded files moved; the configuration file did not.
    RecordedFiles,
    /// The configuration file moved; no recorded file did.
    Configuration,
    /// Recorded files and the configuration file moved.
    RecordedFilesAndConfiguration,
}

impl CaptureMovement {
    /// Classifies the request's already-computed path changes and configuration movement.
    /// Nothing when its capture matches the publication.
    fn from_changes(changes: &PathChanges, configuration_moved: bool) -> Option<Self> {
        let recorded_files_moved = !changes.is_empty();
        match (recorded_files_moved, configuration_moved) {
            (true, false) => Some(Self::RecordedFiles),
            (false, true) => Some(Self::Configuration),
            (true, true) => Some(Self::RecordedFilesAndConfiguration),
            (false, false) => None,
        }
    }

    /// The clause naming what moved, for a detail whose two tree revisions are equal:
    /// every recorded file that moved then lies outside the syntax-indexed ones.
    const fn clause(self) -> &'static str {
        match self {
            Self::RecordedFiles => "recorded files outside them moved",
            Self::Configuration => "the configuration file moved",
            Self::RecordedFilesAndConfiguration => {
                "recorded files outside them and the configuration file moved"
            }
        }
    }
}

/// The rebuild failure recorded past the served publication, as one request meets it.
#[derive(Clone, Debug)]
struct RecordedRebuildFailure {
    /// The filesystem epoch whose rebuild failed.
    epoch: u64,
    /// The filesystem epoch the tree is at now.
    observed_epoch: u64,
    error: Arc<ReadError>,
}

impl RecordedRebuildFailure {
    /// The `stale_index` warning an answer served from `published` carries after this
    /// failure, naming the tree revision `captured` folds to and, when that is the
    /// published one, what `moved` instead.
    fn stale_index(
        &self,
        published: &PublishedWorkspace,
        captured: &WorkspaceDigests,
        moved: CaptureMovement,
    ) -> ReadWarning {
        stale_index_warning(
            published,
            captured,
            moved,
            &StaleIndexReason::RebuildFailed(self),
        )
    }

    /// The failure's own rendering with its causes, naming the failed and observed
    /// epochs, what `served` says the answer came from, and the retry.
    ///
    /// [`StaleIndexReason::detail`] renders `served` and applies
    /// `WARNING_DETAIL_BYTES_MAX` to the whole line.
    fn detail(&self, served: &str) -> String {
        use std::fmt::Write as _;

        let mut detail = format!(
            "the index rebuild for filesystem epoch {failed} failed and the tree is at epoch \
             {observed}{served}: {error}",
            failed = self.epoch,
            observed = self.observed_epoch,
            error = self.error,
        );
        for cause in rift_core::causes(&*self.error) {
            let _ = write!(detail, "; caused by: {cause}");
        }
        detail.push_str("; the next filesystem event retries the rebuild");
        detail
    }
}

/// `detail` cut to at most `bytes_max` bytes on a character boundary.
fn bounded_detail(mut detail: String, bytes_max: usize) -> String {
    if detail.len() <= bytes_max {
        return detail;
    }
    let mut boundary = bytes_max;
    while !detail.is_char_boundary(boundary) {
        boundary -= 1;
    }
    detail.truncate(boundary);
    detail
}

/// The ranking one search request merges, and what the search index's own state adds to
/// that answer's warnings.
///
/// The search store is read under the tree revision the request captured. A store that
/// does not hold that tree because the lexical lane is still committing it, or missed a
/// commit, contributes no ranking and says so in a warning naming the revision; a store
/// that has moved past the captured tree contributes no ranking and no warning, because
/// the request recaptures the newer publication instead. A store that will not answer at
/// all until an operator acts carries the same warning with that reason.
#[derive(Debug, Default)]
struct SearchRanking {
    units: Vec<RankedUnit>,
    warnings: Vec<ReadWarning>,
}

impl SearchRanking {
    /// No ranking at all, for the reason `detail` states.
    fn unavailable(detail: &str) -> Self {
        Self {
            units: Vec::new(),
            warnings: vec![ReadWarning::LexicalRankingUnavailable {
                detail: detail.to_owned(),
            }],
        }
    }
}

/// What one revision-qualified store answer means for the request that captured
/// `tree_revision`, given where that revision stands with the lexical lane.
///
/// Nothing means the store has moved past the captured tree to a newer publication, which
/// asks the request to capture the publication the store already answers for. A store
/// that does not hold the captured tree while the lane is still committing it, or after
/// the lane missed a commit, ranks nothing and says which, naming the revision. A ranking
/// the lexical store cut at its bound carries that cut as a warning, since hits past the
/// bound never reach a page.
fn ranking_of(
    searched: RevisionScoped<FusedRanking>,
    readiness: SemanticReadiness,
    files: u64,
    tree_revision: &str,
    commit_state: LexicalCommitState,
) -> Option<SearchRanking> {
    match (searched, commit_state) {
        (RevisionScoped::Matched(ranking), _) => {
            let mut warnings = readiness_warnings(readiness, files);
            warnings.extend(ranking.lexical_truncated_at().map(lexical_truncated));
            Some(SearchRanking {
                units: ranking.into_units(),
                warnings,
            })
        }
        (_, LexicalCommitState::Committing) => Some(SearchRanking::unavailable(&format!(
            "the lexical index is still committing tree revision {tree_revision}, so the \
             answer was ranked by identifier matching alone; resend the request once the \
             commit lands"
        ))),
        (_, LexicalCommitState::Owed { cause }) => {
            Some(SearchRanking::unavailable(&bounded_detail(
                format!(
                    "the lexical index missed a commit and replaces its whole unit set under \
                     the next publication, so the answer for tree revision {tree_revision} \
                     was ranked by identifier matching alone: {cause}"
                ),
                WARNING_DETAIL_BYTES_MAX,
            )))
        }
        (RevisionScoped::OtherRevision(_), LexicalCommitState::Settled) => None,
        (RevisionScoped::NoRevision, LexicalCommitState::Settled) => {
            Some(SearchRanking::unavailable(&format!(
                "the lexical index holds no indexed tree and no commit is under way for tree \
                 revision {tree_revision}, so the answer was ranked by identifier matching \
                 alone; rift://logs names what the lexical lane did, and a restart retries it"
            )))
        }
    }
}

/// Where `tree_revision` stands with `lane` once the commit for it has landed, or once
/// `budget` ran out with the lane still holding it: `Committing` in the second case
/// alone.
///
/// The wake-up is subscribed before each state read, so a landing between the read and
/// the await is never missed: tokio's `Notified` "is guaranteed to receive wakeups from
/// `notify_waiters()` as soon as it has been created, even if it has not yet been
/// polled". A landing for another revision - the one running ahead of a held write -
/// wakes the wait, which reads the state again under the same deadline; the loop runs
/// once per landing inside the budget.
///
/// # Cancel safety
///
/// Dropping the future drops its subscription; the lane is never written.
async fn commit_landed(
    lane: &LexicalLane,
    tree_revision: &str,
    budget: Duration,
) -> LexicalCommitState {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let landed = lane.landed();
        let commit_state = lane.commit_state(tree_revision);
        if commit_state != LexicalCommitState::Committing {
            return commit_state;
        }
        if tokio::time::timeout_at(deadline, landed).await.is_err() {
            return commit_state;
        }
    }
}

/// The warning a lexical ranking cut at `matches_max` carries: hits past that bound never
/// reach a page, so the caller narrows `query` rather than paging on.
fn lexical_truncated(matches_max: u32) -> ReadWarning {
    ReadWarning::LexicalRankingTruncated {
        matches_max: u64::from(matches_max),
    }
}

/// The warnings one search index's readiness adds to an answer, for a workspace of `files`
/// files.
///
/// `Ready` and `Disabled` add none: the first ranks with both tiers, and the second is the
/// workspace's own decision, which a caller does not need told on every answer.
fn readiness_warnings(readiness: SemanticReadiness, files: u64) -> Vec<ReadWarning> {
    match readiness {
        SemanticReadiness::Disabled | SemanticReadiness::Ready => Vec::new(),
        SemanticReadiness::Preparing { prepared, total } => {
            vec![semantic_preparing(files, prepared, total)]
        }
        SemanticReadiness::Unavailable => vec![ReadWarning::SemanticRankingUnavailable {
            detail: "the semantic ranking's model could not be loaded, so the answer was \
                     ranked lexically alone; correct `[search.semantic]` and start the \
                     server again"
                .to_owned(),
        }],
    }
}

/// The preparation warning, with the wait scaled by what is still missing.
fn semantic_preparing(files: u64, prepared: u64, total: u64) -> ReadWarning {
    ReadWarning::SemanticIndexPreparing {
        prepared,
        total,
        ready_in: ready_in(files, prepared, total),
        detail: format!(
            "{prepared} of {total} declarations carry a vector, so the answer was ranked \
             lexically alone; resend the request once the semantic tier has caught up"
        ),
    }
}

/// How long preparing a whole workspace of `files` files takes, as a declared step rule
/// rather than a measurement: past [`PREPARATION_FILES_LARGE`] a couple of minutes, past
/// [`PREPARATION_FILES_MEDIUM`] around a minute, past [`PREPARATION_FILES_SMALL`] several
/// seconds, and anything smaller a few seconds.
///
/// Nothing times a real pass. The rule states the order of magnitude a reader can plan
/// around, and it is not timing data.
const fn preparation_span(files: u64) -> WireDuration {
    if files > PREPARATION_FILES_LARGE {
        PREPARATION_SPAN_LARGE
    } else if files > PREPARATION_FILES_MEDIUM {
        PREPARATION_SPAN_MEDIUM
    } else if files > PREPARATION_FILES_SMALL {
        PREPARATION_SPAN_SMALL
    } else {
        PREPARATION_SPAN_MINIMAL
    }
}

/// The wait before the semantic ranking joins an answer: the whole workspace's declared
/// span, scaled by the declarations still to embed over the declarations the set holds, so
/// the value shrinks as the pass runs.
///
/// A `total` of zero is not a preparing tier, but the arithmetic does not lean on that: the
/// division is checked and the whole span answers instead. The product runs in `u128`, where
/// two `u64` factors cannot overflow, and the quotient is at most the span itself, so the
/// conversion back is only fallible on paper.
///
/// The result is an estimate a caller reports. Nothing may be scheduled against it.
fn ready_in(files: u64, prepared: u64, total: u64) -> WireDuration {
    let whole = preparation_span(files);
    let remaining = total.saturating_sub(prepared);
    let scaled = u128::from(whole.milliseconds())
        .saturating_mul(u128::from(remaining))
        .checked_div(u128::from(total));
    match scaled.and_then(|milliseconds| u64::try_from(milliseconds).ok()) {
        Some(milliseconds) => WireDuration::from_millis(milliseconds),
        None => whole,
    }
}

/// The `[search.semantic]` table every unit-test fixture in this crate declares.
///
/// Rift ships the semantic tier on, so a fixture carrying no `rift.toml` would acquire the
/// default model from the hub. A hermetic suite must not write into the developer's own
/// Hugging Face cache, and on a runner with no network a default-on tier would spend its
/// whole retry budget inside a detached task nobody waits on. The integration suites
/// declare the same table from `tests/hermetic_search.rs`: a unit test and an integration
/// test are two crates, and one value shared between them would have to leave the
/// library's public surface.
///
/// Three fixtures do not use it. Two drive `[search.semantic]` themselves and neither
/// reaches a network. The third serves a `rift.toml` acceptance refuses, where
/// [`RiftMcp::build`]'s own gate is what holds the acquisition back.
#[cfg(test)]
pub(crate) const SEMANTIC_DISABLED: &str = "[search.semantic]\ndisabled = true\n";

/// Writes `root`'s `rift.toml`: the disabling table, then `configuration`.
///
/// A table header ends where the next one begins, so a fixture's own table follows
/// unchanged and still proves whatever it carries.
///
/// # Errors
///
/// Returns the write's own failure.
#[cfg(test)]
pub(crate) fn hermetic_workspace(root: &Path, configuration: &str) -> std::io::Result<()> {
    let contents = format!("{SEMANTIC_DISABLED}{configuration}");
    std::fs::write(root.join("rift.toml"), contents)
}

/// Workspace MCP server: reads serve an immutable snapshot rebuilt after source changes.
///
/// Clones share every piece of server state. The HTTP transport clones one
/// server per request, so index-supervisor cancellation keys off the last
/// clone's drop, never each clone's.
#[derive(Clone, Debug)]
pub struct RiftMcp {
    root: PathBuf,
    identity: ProductIdentity,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<IndexState>>,
    validation: Arc<IndexValidation>,
    /// Cancels the index supervisor when the last clone drops. The
    /// supervisor task and the watcher hold [`IndexValidation`] directly,
    /// never this guard, so the guard's drop is what ends them.
    #[expect(dead_code, reason = "held for its cancel-on-last-drop effect")]
    supervisor_cancellation: Arc<tokio_util::sync::DropGuard>,
    blocking: BlockingExecutor,
    /// The workspace's search index, absent when it could not be opened at
    /// startup; `search` then serves identifier matching alone and says so in
    /// every answer's warnings.
    search_index: Option<Arc<SearchIndex>>,
    /// The lexical lane, absent exactly when [`Self::search_index`] is. A rebuild commits
    /// through it before its snapshot becomes current.
    lexical: Option<LexicalLane>,
    /// The dependency index every published snapshot answers a dependency-scoped
    /// lookup from. The lane fills it behind the answers.
    #[cfg(test)]
    dependencies: Arc<DependencyStore>,
    /// The workspace's recorded diagnostics, absent when the store could not be
    /// opened at startup; `rift://logs` then answers with that reason rather
    /// than refusing. The handle is the read side alone: the drain task that
    /// writes records holds its own.
    logs: Option<Arc<LogStore>>,
    engines: Arc<EngineHold>,
    /// The capture a test forces in place of reading the tree, so every retry arm of the
    /// bounded reconciliation loop is reached without racing filesystem events.
    #[cfg(test)]
    forced_capture: ForcedCapture,
    tool_router: ToolRouter<Self>,
}

/// The search tier one server opens at startup: the index, the lanes over it, and the
/// model acquisition the semantic ranking waits on. Every part is absent when the index
/// could not be opened.
struct SearchTier {
    acquisition: Option<ModelAcquisition>,
    search_index: Option<Arc<SearchIndex>>,
    lexical: Option<LexicalLane>,
    population: Option<PopulationLane>,
}

/// One server built and not yet supervised: the watcher, the invalidations it feeds, and
/// the supervisor's context, held apart from the server until [`Self::supervised`] starts
/// the task that consumes them.
struct AssembledServer {
    server: RiftMcp,
    watcher: notify::RecommendedWatcher,
    invalidations: tokio::sync::mpsc::Receiver<()>,
    context: IndexSupervisorContext,
}

impl AssembledServer {
    /// Starts the index supervisor over the held parts and returns the serving server.
    async fn supervised(self) -> RiftMcp {
        let task = tokio::spawn(run_index_supervisor(
            self.watcher,
            self.invalidations,
            self.context,
        ));
        let mut held = self.server.validation.task.lock().await;
        *held = Some(task);
        drop(held);
        self.server
    }
}

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl RiftMcp {
    /// Builds server from one direct-workspace snapshot, applying the
    /// accepted `rift.toml`'s `[source]` policy to the initial index and its
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
    /// Dropping this future discards construction. An accepted blocking scan
    /// finishes in the bounded executor before releasing its capacity permit.
    pub async fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        Self::build_at(absolute_root(root)?, limits, None).await
    }

    /// Builds against storage already opened by the serving process.
    pub(crate) async fn build_with_storage(
        root: &Path,
        limits: WorkspaceIndexLimits,
        storage: WorkspaceStorage,
    ) -> Result<Self, ReadError> {
        Self::build_at(absolute_root(root)?, limits, Some(storage)).await
    }

    async fn resolve_storage(root: &Path, storage: Option<WorkspaceStorage>) -> WorkspaceStorage {
        match storage {
            Some(storage) => storage,
            None => WorkspaceStorage::open(root).await,
        }
    }

    /// Accepts startup configuration without blocking the serving runtime.
    async fn startup_configuration(root: &Path) -> Result<ConfigurationState, ReadError> {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || ConfigurationState::accept(&root))
            .await
            .map_err(|error| ReadFault::task("configuration acceptance", error.to_string()))
    }

    /// Builds from one resolved root and its process-owned storage.
    async fn build_at(
        root: PathBuf,
        limits: WorkspaceIndexLimits,
        storage: Option<WorkspaceStorage>,
    ) -> Result<Self, ReadError> {
        let assembled = Self::assemble(root, limits, storage, LexicalLane::spawn).await?;
        Ok(assembled.supervised().await)
    }

    /// Builds every part of one server except its index supervisor task, with the lexical
    /// lane `spawn_lexical` opens over the search index.
    ///
    /// [`AssembledServer::supervised`] starts the supervisor. A test that drives the
    /// published state itself holds the parts instead, and one that gates the lexical
    /// store spawns the lane over that store.
    async fn assemble(
        root: PathBuf,
        limits: WorkspaceIndexLimits,
        storage: Option<WorkspaceStorage>,
        spawn_lexical: impl FnOnce(Arc<SearchIndex>, BlockingExecutor, CancellationToken) -> LexicalLane,
    ) -> Result<AssembledServer, ReadError> {
        let identity = crate::identity::product_identity()
            .await
            .map_err(|error| ReadFault::task("product identity", error.to_string()))?;
        let startup_configuration = Self::startup_configuration(&root).await?;
        let blocking =
            BlockingExecutor::for_configuration(&startup_configuration.server_configuration());
        let (validation, invalidations) =
            IndexValidation::new(startup_configuration.index_limits(limits)?.files_max());
        let watcher = Self::start_watcher(&root, &validation, &blocking).await?;
        let dependencies = Arc::new(DependencyStore::new(DependencyIndex::planned(
            &DependencyCatalog::default(),
            DependencyIndexLimits::default(),
            PackageSelection::default(),
        )));
        let (published, lexical_write) =
            initial_workspace(&root, limits, &validation, &blocking, &dependencies).await?;
        let cancellation = validation.cancellation.clone();
        let dependency_lane =
            Self::spawn_dependency_lane(&dependencies, &published, &blocking, cancellation)?;
        // Direct construction delays the database open until the initial scan proves the
        // workspace root. A serving process supplies the owner it opened for foreground log
        // capture; that path creates `.rift` only below an already-existing root.
        let storage = Self::resolve_storage(&root, storage).await;
        let SearchTier {
            acquisition,
            search_index,
            lexical,
            population,
        } = Self::open_search_tier(
            &startup_configuration,
            &root,
            &storage,
            &validation,
            &published,
            lexical_write,
            |index, cancellation| spawn_lexical(index, blocking.clone(), cancellation),
        );
        // The log store shares the database owner without depending on index readiness. Its
        // reads use committed WAL snapshots, so `rift://logs` can answer while a rebuild is
        // still preparing the next publication.
        let logs = storage.logs();
        let published = Arc::new(RwLock::new(IndexState {
            current: published,
            failure: None,
        }));
        let context = IndexSupervisorContext {
            root: root.clone(),
            limits,
            published: Arc::clone(&published),
            validation: Arc::clone(&validation),
            blocking: blocking.clone(),
            population: population.clone(),
            lexical: lexical.clone(),
            dependencies: Arc::clone(&dependencies),
            dependency_lane: dependency_lane.clone(),
        };
        if let (Some(index), Some(lane), Some(acquisition)) =
            (search_index.as_ref(), population.as_ref(), acquisition)
        {
            spawn_semantic_preparation(
                Arc::clone(index),
                acquisition,
                Arc::clone(&published),
                lane.clone(),
                validation.cancellation.clone(),
            );
        }
        let supervisor_cancellation = Arc::new(validation.cancellation.clone().drop_guard());
        let (definitions, bindings) = startup_configuration.lsp_runtime_configuration();
        let engines = Arc::new(EngineHold::new(root.clone(), definitions, bindings));
        let server = Self {
            root: root.clone(),
            identity,
            limits,
            published,
            validation,
            supervisor_cancellation,
            blocking,
            search_index,
            lexical,
            #[cfg(test)]
            dependencies,
            logs,
            engines,
            #[cfg(test)]
            forced_capture: ForcedCapture::default(),
            tool_router: Self::tool_router(),
        };
        Ok(AssembledServer {
            server,
            watcher,
            invalidations,
            context,
        })
    }

    /// Starts the filesystem watcher over `root` on the worker pool.
    async fn start_watcher(
        root: &Path,
        validation: &Arc<IndexValidation>,
        blocking: &BlockingExecutor,
    ) -> Result<notify::RecommendedWatcher, ReadError> {
        let watch_root = root.to_path_buf();
        let watch_validation = Arc::clone(validation);
        blocking
            .run("workspace watch setup", move || {
                workspace_watcher(&watch_root, &watch_validation)
            })
            .instrument(tracing::info_span!(
                "index.watch",
                component = "index",
                operation = "watch.setup"
            ))
            .await
    }

    /// Plans the dependency store over the initial publication's catalog and plan, and
    /// spawns the lane that fills it.
    ///
    /// The store is planned before anything is served, so a lookup answered ahead of the
    /// lane's first pass reports the packages still pending rather than an empty catalog.
    fn spawn_dependency_lane(
        dependencies: &Arc<DependencyStore>,
        published: &PublishedWorkspace,
        blocking: &BlockingExecutor,
        cancellation: CancellationToken,
    ) -> Result<DependencyLane, ReadError> {
        let request = DependencyRequest::for_publication(published);
        let mut index = dependencies.write()?;
        request.follow(&mut index);
        drop(index);
        let lane = DependencyLane::spawn(Arc::clone(dependencies), blocking.clone(), cancellation);
        lane.request(request);
        Ok(lane)
    }

    /// Opens the search tier over `storage` and hands the lexical lane the initial write.
    ///
    /// Nothing here waits for a pass. The lexical lane commits the initial unit set behind
    /// the first answers, and a `search` that arrives before that transaction lands is told
    /// so and answers from identifier matching; awaiting the commit here held every first
    /// request on a large workspace until a transaction of every unit ended. The population
    /// lane runs the run's first pass afterwards, which establishes the vector set, because
    /// a store found on disk was written by an earlier process, possibly under another
    /// model.
    fn open_search_tier(
        startup_configuration: &ConfigurationState,
        root: &Path,
        storage: &WorkspaceStorage,
        validation: &IndexValidation,
        published: &Arc<PublishedWorkspace>,
        lexical_write: LexicalWrite,
        spawn_lexical: impl FnOnce(Arc<SearchIndex>, CancellationToken) -> LexicalLane,
    ) -> SearchTier {
        let search_configuration = startup_configuration.search_configuration();
        // While `rift.toml` is invalid every request is refused until it is fixed, and the
        // table naming the model is the very part that could not be read. Acquiring the
        // shipped default would spend a download on a server that answers nothing, so the
        // tier waits for a workspace whose configuration was accepted.
        let acquisition = startup_configuration
            .is_accepted()
            .then(|| model_acquisition(&search_configuration.semantic, root))
            .flatten();
        let search_limits = search_index_limits(&search_configuration, acquisition.as_ref());
        let search_index = open_search_index(storage, search_limits);
        let lexical = search_index
            .as_ref()
            .map(|index| spawn_lexical(Arc::clone(index), validation.cancellation.clone()));
        if let Some(lane) = lexical.as_ref() {
            lane.request(lexical_write, Arc::clone(published));
        }
        let population = search_index
            .as_ref()
            .map(|index| PopulationLane::spawn(Arc::clone(index), validation.cancellation.clone()));
        if let Some(lane) = population.as_ref() {
            lane.request(Arc::clone(published));
        }
        SearchTier {
            acquisition,
            search_index,
            lexical,
            population,
        }
    }

    /// Exact identity advertised by this server.
    #[must_use]
    pub(crate) fn product_identity(&self) -> &ProductIdentity {
        &self.identity
    }

    /// Returns owned supervisor shutdown access for transport adapters.
    pub(crate) fn index_supervisor(&self) -> IndexSupervisor {
        IndexSupervisor {
            validation: Arc::clone(&self.validation),
        }
    }

    /// The `[server]` table from the currently published acceptance, or the
    /// default table while `rift.toml` is invalid.
    pub(crate) async fn server_configuration(&self) -> ServerConfiguration {
        self.published
            .read()
            .await
            .current
            .configuration
            .server_configuration()
    }

    /// Finds declarations and their source by exact symbol name. Each hit
    /// carries the declaration and its source excerpt unless `include` omits
    /// `source`. `include: ["history"]` adds each hit's version-control timeline,
    /// walked from the served revision. `rev` serves the lookup from a
    /// version-control revision instead of the current tree. `scope` reaches
    /// past the project tree: `dependencies` answers from the public
    /// declarations of the cataloged packages alone, `all` from both, project
    /// hits first. Use `search` when the name is not exactly known.
    #[tool]
    async fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<Json<GetSymbolResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.get_symbol(&params))
            .await
    }

    /// Searches indexed declarations and source lines by lexical `query`, merged with
    /// full-text matches from included `[search.text]` files and declaration bodies, and by a
    /// bounded relationship `traversal` from one seed symbol. `change` answers the
    /// declarations two committed revisions hold differently, in place of `query` and
    /// `traversal`. `rev` searches a version-control revision instead of the current tree,
    /// and never combines with `traversal` or `change`. `scope` reaches past the project
    /// tree: `dependencies` answers `query` from the public declarations of the cataloged
    /// packages alone, `all` from both, ordered together. Use `get_symbol` when the
    /// declaration name is known.
    ///
    /// For a current-tree search, the published workspace is resolved exactly once and
    /// threaded through both the search index's revision check and the executed
    /// `ReadService::search` call: a concurrent rebuild between two separate resolutions
    /// could otherwise validate ranked units against one snapshot and merge them into
    /// results computed from another.
    #[tool]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        if let Some(change) = params.change.clone() {
            return self.change_search(params, change).await;
        }
        let Some(rev) = params.rev.clone() else {
            return self.current_tree_search(params).await;
        };
        // The search index only ever holds the current tree, so a revision-addressed
        // search never consults it.
        self.read_at(Some(rev), move |reads| reads.search(&params, &[]))
            .await
    }

    /// Compares the two committed revisions `change` names and answers the declarations
    /// they hold differently.
    ///
    /// The comparison reads both sides from the workspace's git objects with no checkout,
    /// under the same `[source]` policy and bounds a revision read applies, and
    /// `[providers.history] enabled = false` refuses it the same way. Neither side is the
    /// current tree, so the search index never takes part.
    ///
    /// A `traversal` riding beside the comparison walks the current publication's
    /// relationship graph, so that publication is resolved once here and handed to the
    /// comparison along with the revision policy.
    async fn change_search(
        &self,
        params: SearchParams,
        change: rift_protocol::read::SearchChange,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let resolved = self.published_workspace(wire::ErrorPhase::Read).await?;
        let revision_read = self.revision_read(&resolved.published)?;
        let reads = Arc::clone(&resolved.published.reads);
        let RevisionRead {
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            history: _,
        } = revision_read;
        self.blocking
            .run("revision comparison read", move || {
                rift_server::search_change(
                    &root,
                    &params,
                    &change,
                    limits,
                    &visibility,
                    (&text_inclusion, &languages),
                    &reads,
                )
            })
            .await
            .map(Json)
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))
    }

    /// Ranks and reads one current-tree search against one publication.
    ///
    /// The store answers only for the tree it was stamped with, so a store that has moved
    /// past the captured publication ends this attempt rather than ranking rows the answer
    /// cannot place. The next attempt captures the publication the store already holds, and
    /// the bound is the same one `reconcile_workspace` applies to a tree that keeps moving.
    /// Each attempt's wait for a lexical commit in flight gets the full
    /// `[server] readiness_timeout`: the workspace wait's own deadline stays inside
    /// `published_workspace`, so what remains of it is not at hand here.
    ///
    /// A request whose attempts are spent answers from the publication it holds, with the
    /// ranked tier reported unavailable. Capturing the publication the store holds is what
    /// the attempts already try and cannot reach while rebuilds keep landing: the lexical
    /// lane stamps the store as each candidate publishes, `published_workspace` resolves
    /// the publication separately, and under back-to-back rebuilds the second trails the
    /// first every time the pair is read. `ReadService::search` answers identifier
    /// matching from the publication itself, so an answer without a ranking is still one
    /// snapshot's own rows; the ranked lane contributes the ordering alone, and its
    /// absence is what the warning states.
    async fn current_tree_search(
        &self,
        params: SearchParams,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let budget = self.readiness_timeout().await;
        let mut resolved = self.published_workspace(wire::ErrorPhase::Read).await?;
        let mut ranking = self.ranking(&params, &resolved.published, budget).await?;
        for _retry in 1..INDEX_CAPTURE_ATTEMPTS_MAX {
            if ranking.is_some() {
                break;
            }
            resolved = self.published_workspace(wire::ErrorPhase::Read).await?;
            ranking = self.ranking(&params, &resolved.published, budget).await?;
        }
        let (resolved, ranking, references) = tokio::time::timeout(
            budget,
            Box::pin(self.current_tree_references(resolved, ranking, &params, budget)),
        )
        .await
        .map_err(|_| {
            ReadFault::unavailable("engine references", "request deadline exceeded")
                .tool_error(wire::ErrorPhase::Read)
        })??;
        let SearchRanking { units, warnings } = ranking.unwrap_or_else(|| {
            SearchRanking::unavailable(
                "the lexical index is stamped for a publication newer than the one this \
                 request captured, and the workspace kept publishing across the bounded \
                 attempts, so the answer was ranked by identifier matching alone; resend \
                 the request once the rebuilds settle",
            )
        });
        let executed = params;
        let mut answer = self
            .current_tree_read(&resolved, move |reads| {
                reads.search_with_references(&executed, &units, &references)
            })
            .await?;
        answer.0.warnings.extend(warnings);
        Ok(answer)
    }

    /// Repeats engine reads against a fresh publication when their captured tree moves.
    ///
    /// The caller bounds this entire exchange, including publication and ranking waits,
    /// by the same readiness deadline as the original engine request.
    async fn current_tree_references(
        &self,
        mut resolved: ResolvedWorkspace,
        mut ranking: Option<SearchRanking>,
        params: &SearchParams,
        budget: Duration,
    ) -> Result<(ResolvedWorkspace, Option<SearchRanking>, EngineReferences), ErrorData> {
        for attempt in 0..INDEX_CAPTURE_ATTEMPTS_MAX {
            if let Some(references) = self.engine_references(&resolved, params).await? {
                return Ok((resolved, ranking, references));
            }
            if attempt + 1 < INDEX_CAPTURE_ATTEMPTS_MAX {
                resolved = self.published_workspace(wire::ErrorPhase::Read).await?;
                ranking = self.ranking(params, &resolved.published, budget).await?;
            }
        }
        Err(ReadFault::unavailable(
            "engine references",
            "source or configuration kept changing during the bounded reference reads",
        )
        .tool_error(wire::ErrorPhase::Read))
    }

    /// Resolves references against one unchanged current-tree publication.
    ///
    /// `None` asks the caller to capture a fresh publication after source or configuration
    /// movement. A stale publication uses its index and keeps its existing stale warning.
    async fn engine_references(
        &self,
        resolved: &ResolvedWorkspace,
        params: &SearchParams,
    ) -> Result<Option<EngineReferences>, ErrorData> {
        if params.traversal.is_none() || resolved.stale.is_some() {
            return Ok(Some(EngineReferences::default()));
        }
        let engines = self.engine_pool_for(&resolved.published).await;
        if !uses_engine_references(&resolved.published.reads, &engines, params)
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?
        {
            return Ok(Some(EngineReferences::default()));
        }
        if !self.engine_tree_matches(&resolved.published).await? {
            return Ok(None);
        }
        let references = Box::pin(resolve_engine_references(
            &resolved.published.reads,
            &engines,
            params,
        ))
        .await
        .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        if self.engine_tree_matches(&resolved.published).await? {
            Ok(Some(references))
        } else {
            Ok(None)
        }
    }

    /// Whether the captured source and configuration still match the publication.
    async fn engine_tree_matches(&self, published: &PublishedWorkspace) -> Result<bool, ErrorData> {
        let (digests, configuration) = self
            .capture_tree(published)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        Ok(digests.fingerprint() == published.fingerprint
            && configuration == published.configuration.fingerprint)
    }

    /// Runs the search index for one request against `published` - the exact snapshot the
    /// caller also runs `ReadService::search` against, never a separately resolved one.
    ///
    /// Returns nothing when the store has moved past `published`'s tree to a newer
    /// publication, which asks the caller to capture the publication the store already
    /// answers for. Every other outcome ranks: an index that could not be opened, one the
    /// lexical lane is still committing this tree into once `budget` ran out, and one
    /// that missed a commit warn `lexical_ranking_unavailable` and leave identifier
    /// search to answer alone. A query-term limit the index refuses surfaces as this
    /// request's own `limit_exceeded` error, never a silent degrade. A `dependencies`
    /// scope never consults the index: the ranked lane serves the project alone, so
    /// nothing about it rides that answer.
    async fn ranking(
        &self,
        params: &SearchParams,
        published: &PublishedWorkspace,
        budget: Duration,
    ) -> Result<Option<SearchRanking>, ErrorData> {
        if params.scope == SearchScope::Dependencies {
            return Ok(Some(SearchRanking::default()));
        }
        let Some(index) = self.search_index.as_ref() else {
            return Ok(Some(SearchRanking::unavailable(
                "the workspace search database could not be opened, so the answer was ranked \
                 by identifier matching alone; the server log names the open failure, and a \
                 restart retries it",
            )));
        };
        // An absent or empty query is refused by `ReadService::search` itself; warning
        // about a tier that was never consulted would only crowd that refusal.
        let Some(query) = params.query.as_deref().filter(|query| !query.is_empty()) else {
            return Ok(Some(SearchRanking::default()));
        };
        let tree_revision = published.reads.tree_revision();
        let (searched, commit_state) = self
            .store_answer(index, tree_revision, query, budget)
            .await?;
        Ok(ranking_of(
            searched,
            index.readiness(),
            published.reads.file_count(),
            tree_revision,
            commit_state,
        ))
    }

    /// The store's answer for `tree_revision`, read as deep as [`Self::fetch_limit`].
    async fn read_store(
        &self,
        index: &SearchIndex,
        tree_revision: &str,
        query: &str,
    ) -> Result<RevisionScoped<FusedRanking>, ErrorData> {
        index
            .search(tree_revision, query, self.fetch_limit())
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))
    }

    /// The store's answer for `tree_revision` and where that revision stands with the
    /// lexical lane, after waiting out a commit in flight.
    ///
    /// A store that does not hold the captured tree while the lane is still committing
    /// it is a transient condition: the read waits for that commit to land under
    /// `budget`, then reads the store and the state again once. `Owed` and `Settled`
    /// never wait, since no held transaction can change either. A wait that runs out
    /// leaves `Committing` standing, which is what [`ranking_of`] warns about. Without a
    /// lane the revision counts as settled: nothing could commit it.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future drops the wait and the store read; nothing is written.
    async fn store_answer(
        &self,
        index: &SearchIndex,
        tree_revision: &str,
        query: &str,
        budget: Duration,
    ) -> Result<(RevisionScoped<FusedRanking>, LexicalCommitState), ErrorData> {
        let searched = self.read_store(index, tree_revision, query).await?;
        let Some(lane) = self.lexical.as_ref() else {
            return Ok((searched, LexicalCommitState::Settled));
        };
        let commit_state = lane.commit_state(tree_revision);
        let matched = matches!(searched, RevisionScoped::Matched(_));
        if matched || commit_state != LexicalCommitState::Committing {
            return Ok((searched, commit_state));
        }
        let commit_state = commit_landed(lane, tree_revision, budget).await;
        let searched = self.read_store(index, tree_revision, query).await?;
        Ok((searched, commit_state))
    }

    /// How deep the search index is read for one request: the same `results_max` bound
    /// `ReadService::search`'s indexed lane already uses, so every lane merges into one
    /// candidate pool bounded once, whatever the requested page size. A request's own
    /// `limit` changes only how that one pool is paged, never how deep it is read.
    fn fetch_limit(&self) -> u32 {
        u32::try_from(self.limits.results_max()).unwrap_or(u32::MAX)
    }

    /// Lists the syntax nodes covering one UTF-8 byte position in one file,
    /// outermost first. Each identity carries a witness, so an address taken
    /// from this listing refuses cleanly once the file's bytes drift. `rev`
    /// lists the nodes as of a version-control revision instead of the
    /// current tree. A visible path no syntax provider parses refuses
    /// `capability_unavailable`, naming the extension.
    #[tool]
    async fn nodes(
        &self,
        Parameters(params): Parameters<NodesParams>,
    ) -> Result<Json<NodesResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.nodes(params)).await
    }

    /// Runs one read against the tree the request names - the current
    /// snapshot, or a snapshot built at the request's version-control
    /// revision - behind the acceptance gate every request passes.
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
        Answer: ReadAnswer + Send + 'static,
    {
        let resolved = self.published_workspace(wire::ErrorPhase::Read).await?;
        let Some(rev) = rev else {
            return self.current_tree_read(&resolved, operation).await;
        };
        let RevisionRead {
            root,
            limits,
            visibility,
            text_inclusion,
            languages,
            history,
        } = self.revision_read(&resolved.published)?;
        self.blocking
            .run("revision workspace read", move || {
                let reads = ReadService::at_revision_with_languages(
                    &root,
                    &rev,
                    limits,
                    &visibility,
                    &text_inclusion,
                    &languages,
                    history,
                )?;
                operation(&reads)
            })
            .await
            .map(Json)
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))
    }

    /// The workspace policy one read of committed source applies, derived once from the
    /// accepted configuration: the served root, the index bounds, the `[source]` matcher,
    /// the `[search.text]` selection, the configured language entries, and the
    /// `[providers.history]` table a served snapshot carries forward.
    ///
    /// Committed source is what `[providers.history]` gates, so a workspace that turns the
    /// table off refuses here, before any revision is resolved.
    fn revision_read(&self, published: &PublishedWorkspace) -> Result<RevisionRead, ErrorData> {
        let configuration = published.configuration.accepted(wire::ErrorPhase::Read)?;
        if !configuration.providers.history.enabled {
            return Err(ReadError::from(ReadFault::Unsupported {
                capability: "revision reads (providers.history disabled)".to_owned(),
            })
            .tool_error(wire::ErrorPhase::Read));
        }
        Ok(RevisionRead {
            root: self.root.clone(),
            limits: published
                .configuration
                .index_limits(self.limits)
                .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?,
            visibility: SourceVisibility::from(&configuration.source),
            text_inclusion: rift_core::TextFileInclusion::from(&configuration.search),
            languages: rift_core::LanguageFileSelections::from(&configuration),
            history: configuration.providers.history.clone(),
        })
    }

    /// Runs one read against `resolved`'s current-tree snapshot, behind the acceptance
    /// gate every request passes, and adds the `stale_index` warning when that snapshot
    /// is served in spite of a recorded rebuild failure. Shared by `read_at`'s
    /// current-tree path and `search`, which resolves the publication itself first so the
    /// lexical tier's revision check and the identifier read it merges into can never
    /// straddle two different snapshots.
    async fn current_tree_read<Answer>(
        &self,
        resolved: &ResolvedWorkspace,
        operation: impl FnOnce(&ReadService) -> Result<Answer, ReadError> + Send + 'static,
    ) -> Result<Json<Answer>, ErrorData>
    where
        Answer: ReadAnswer + Send + 'static,
    {
        resolved
            .published
            .configuration
            .accepted(wire::ErrorPhase::Read)?;
        let reads = Arc::clone(&resolved.published.reads);
        let mut answer = self
            .blocking
            .run("current workspace read", move || operation(&reads))
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        if let Some(stale) = resolved.stale.clone() {
            answer.warnings_mut().push(stale);
        }
        Ok(Json(answer))
    }

    /// Returns one atomically published index and configuration policy.
    ///
    /// The wait is bounded by `[server] readiness_timeout` from the last
    /// accepted `rift.toml` - the default table's value while the file is
    /// invalid, since the acceptance failure itself is what a request
    /// meets once the wait ends. The deadline starts here, covering index
    /// validation; a caller that goes on to wait for a specific engine's
    /// readiness spends what remains of the same budget, not a fresh one.
    async fn published_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<ResolvedWorkspace, ErrorData> {
        let timeout = self.readiness_timeout().await;
        let deadline = tokio::time::Instant::now() + timeout;
        let Ok(result) = tokio::time::timeout_at(deadline, self.reconcile_workspace(phase)).await
        else {
            let detail = self.readiness_stall(timeout).await;
            tracing::warn!(
                component = "index",
                operation = "index.readiness",
                detail = detail.as_str(),
                "a request spent its whole readiness budget"
            );
            return Err(ReadFault::unavailable("current workspace read", detail).tool_error(phase));
        };
        result
    }

    /// What the elapsed wait was waiting for, in the words an operator can act
    /// on.
    ///
    /// "readiness deadline elapsed" alone sends a reader to the timeout, which
    /// is almost never the fault: the epochs say whether the index is behind
    /// the filesystem and by how far, and `rift://logs` holds the rebuild
    /// records that go with them.
    async fn readiness_stall(&self, timeout: Duration) -> String {
        let observed = self.validation.observed_epoch();
        let published = {
            let state = self.published.read().await;
            let (current, _failure) = state.snapshot();
            current.epoch
        };
        let waited_ms = timeout.as_millis();
        if published == observed {
            return format!(
                "the index settled at epoch {published}, but workspace validation did not finish \
                 within {waited_ms}ms; read rift://logs for capture and rebuild records"
            );
        }
        format!(
            "the index is {behind} filesystem events behind the tree after {waited_ms}ms \
             (published epoch {published}, observed epoch {observed}); read rift://logs for \
             what the index lane did",
            behind = observed.saturating_sub(published)
        )
    }

    /// The `[server] readiness_timeout` this request's wait is bounded by,
    /// read from whatever configuration is currently published - stale or
    /// not, since a deadline this call needs before validation can even
    /// begin cannot itself wait on that validation.
    async fn readiness_timeout(&self) -> Duration {
        let state = self.published.read().await;
        let (current, _failure) = state.snapshot();
        Duration::from_millis(
            current
                .configuration
                .server_configuration()
                .readiness_timeout
                .milliseconds(),
        )
    }

    /// Reconciles native observations with an exact request-time capture of the tree.
    ///
    /// The capture reads every visible file to decide whether the publication still answers
    /// for this request, so it already knows which files moved. A request that finds the
    /// tree ahead of the publication names those files, and the rebuild it waits for
    /// reparses them alone; only a moved `rift.toml` and a capture that failed ask for the
    /// whole workspace. A read that finds the tree ahead of a publication whose rebuild
    /// failed waits for nothing: it answers from that publication and says so, and the
    /// next filesystem event retries the rebuild.
    ///
    /// The first is the filesystem epoch, which counts observations rather than content.
    /// [`IndexValidation::observe_locked`] increments it for every classified event, and
    /// [`watch_path_impact`] classifies from the path and the event kind without reading a
    /// byte, so a write that lands the bytes a file already held moves the epoch while
    /// every digest stays equal. The capture is the content comparison: a read accepts the
    /// publication whenever the capture folds to its fingerprint under its configuration,
    /// whatever the epoch counter reads.
    ///
    /// The second is the attempt budget. A successful superseded capture already proves
    /// movement: a read checks the tree once and answers stale with that recorded epoch.
    /// A read without that completed work that spends its capture budget answers from the
    /// publication the last attempt resolved and carries a `stale_index` warning naming
    /// what that attempt's capture found ahead. This keeps a workspace under back-to-back
    /// rebuilds readable, and each attempt still asks for a rebuild. Configuration movement
    /// waits for acceptance and publication before the next capture; the last capture
    /// refuses if its configuration moved.
    async fn reconcile_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<ResolvedWorkspace, ErrorData> {
        let mut spent = None;
        let mut wait = ReadWait::Capture;
        for _attempt in 0..INDEX_CAPTURE_ATTEMPTS_MAX {
            let (current, rebuild_failure) = self.await_current_workspace(phase, wait).await?;
            let capture = self.capture_tree(&current).await;
            let (digests, configuration_fingerprint) = match capture {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = self.validation.observe_whole_workspace();
                    return Err(error.tool_error(phase));
                }
            };
            let configuration_matches =
                current.configuration.fingerprint == configuration_fingerprint;
            let tree_matches =
                digests.fingerprint() == current.fingerprint && configuration_matches;
            if tree_matches {
                current.configuration.accepted(phase)?;
                return Ok(ResolvedWorkspace::current(current));
            }
            let changes = PathChanges::between(&current.reads.workspace_digests(), &digests);
            let moved = CaptureMovement::from_changes(&changes, !configuration_matches);
            if let Some(failure) = rebuild_failure {
                current.configuration.accepted(phase)?;
                let stale = moved.map(|moved| failure.stale_index(&current, &digests, moved));
                return Ok(ResolvedWorkspace {
                    published: current,
                    stale,
                });
            }
            let observed = if configuration_matches {
                self.validation
                    .observe_paths(changes.iter().map(|(path, _)| path.clone()))
            } else {
                self.validation.observe_whole_workspace()
            };
            observed.map_err(|error| error.tool_error(phase))?;
            if configuration_matches
                && let Some(epoch) = self.validation.superseded_after(current.epoch)
            {
                current.configuration.accepted(phase)?;
                let stale = moved.map(|moved| {
                    stale_index_warning(
                        &current,
                        &digests,
                        moved,
                        &StaleIndexReason::RebuildSuperseded {
                            epoch,
                            changes: &changes,
                        },
                    )
                });
                return Ok(ResolvedWorkspace {
                    published: current,
                    stale,
                });
            }
            wait = if configuration_matches {
                ReadWait::Rebuild
            } else {
                ReadWait::Configuration
            };
            spent = Some((current, digests, changes, moved));
        }
        if matches!(wait, ReadWait::Rebuild)
            && let Some((current, digests, changes, moved)) = spent
        {
            current.configuration.accepted(phase)?;
            let stale = moved.map(|moved| {
                stale_index_warning(
                    &current,
                    &digests,
                    moved,
                    &StaleIndexReason::TreeKeptMoving { changes: &changes },
                )
            });
            return Ok(ResolvedWorkspace {
                published: current,
                stale,
            });
        }
        Err(ReadFault::unavailable(
            "current workspace read",
            "workspace changed across bounded reconciliation attempts",
        )
        .tool_error(phase))
    }

    /// Captures every visible file's digest and the configuration file's state, under
    /// `current`'s accepted policy, on the worker pool.
    async fn capture_tree(
        &self,
        current: &PublishedWorkspace,
    ) -> Result<(WorkspaceDigests, ConfigurationFingerprint), ReadError> {
        #[cfg(test)]
        if let Some(forced) = self.forced_capture.installed() {
            return forced(current);
        }
        let root = self.root.clone();
        let limits = current.configuration.index_limits(self.limits)?;
        let visibility = current.configuration.source_visibility();
        let text_inclusion = current.configuration.text_inclusion();
        let languages = current.configuration.language_file_selections();
        self.blocking
            .run("workspace fingerprint", move || {
                let digests = capture_digests_with_languages(
                    &root,
                    limits,
                    &visibility,
                    &text_inclusion,
                    &languages,
                )
                .map_err(|error| ReadError::from(ReadFault::Index(error)))?;
                Ok((digests, configuration_fingerprint(&root)))
            })
            .instrument(tracing::debug_span!(
                "index.reconcile",
                component = "index",
                operation = "fingerprint.capture",
                epoch = current.epoch
            ))
            .await
    }

    /// Waits until published and observed epochs agree, or resolves what a recorded
    /// rebuild failure means for this request: a read answers from the publication and
    /// carries the failure.
    ///
    /// A read's first capture compares content even when observations are ahead. Changed
    /// source then waits for publication, unless a successful capture after the current
    /// publication was superseded. That completed work proves continued movement, so the
    /// read reaches its bounded captures and stale answer without waiting for a publication
    /// the moving tree keeps superseding. A later publication makes that exception
    /// obsolete. Configuration movement keeps waiting for the epochs to agree.
    ///
    /// Every other job of this loop stands for a read: a watcher the backend reported
    /// broken refuses, a supervisor that stopped refuses while the epochs disagree, and a
    /// recorded rebuild failure is resolved before the read goes on, so the answer still
    /// carries that failure's `stale_index`.
    async fn await_current_workspace(
        &self,
        phase: wire::ErrorPhase,
        wait: ReadWait,
    ) -> Result<(Arc<PublishedWorkspace>, Option<RecordedRebuildFailure>), ErrorData> {
        loop {
            let changed = self.validation.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let observed_epoch = self.validation.observed_epoch();
            let state = self.published.read().await;
            let (current, failure) = state.snapshot();
            drop(state);
            if self.validation.watch_failed.load(Ordering::Acquire) {
                return Err(ReadFault::unavailable(
                    "current workspace read",
                    "filesystem watcher failed",
                )
                .tool_error(phase));
            }
            if current.epoch == observed_epoch {
                return Ok((current, None));
            }
            // The supervisor is the only publisher. Once it is gone the epochs can never
            // meet again, so waiting for them spends the whole readiness budget to reach
            // the same refusal, once per request, forever.
            if !self.validation.supervisor_running.load(Ordering::Acquire) {
                return Err(ReadFault::unavailable(
                    "current workspace read",
                    format!(
                        "the index supervisor stopped, so the index stays {behind} filesystem \
                         events behind the tree (published epoch {published}, observed epoch \
                         {observed_epoch}); restart the workspace server, and read \
                         rift://logs for what it did before it stopped",
                        behind = observed_epoch.saturating_sub(current.epoch),
                        published = current.epoch,
                    ),
                )
                .tool_error(phase));
            }
            if let Some((failed_epoch, error)) = failure
                && failed_epoch >= current.epoch
            {
                let recorded = RecordedRebuildFailure {
                    epoch: failed_epoch,
                    observed_epoch,
                    error,
                };
                return Ok((current, Some(recorded)));
            }
            let capture = match wait {
                ReadWait::Capture => true,
                ReadWait::Rebuild => self.validation.superseded_after(current.epoch).is_some(),
                ReadWait::Configuration => false,
            };
            if capture {
                return Ok((current, None));
            }
            changed.as_mut().await;
        }
    }
}

impl RiftMcp {
    /// Answers one `rift://logs` read.
    ///
    /// The read takes no part in workspace readiness. A request that waits for
    /// the index to settle is exactly the request whose refusal these records
    /// explain, so making the explanation wait on the same gate would leave the
    /// failure unreadable. The page comes from the last accepted `[logs]`
    /// table, or the default table while `rift.toml` is invalid.
    async fn read_logs(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let page_records = {
            let state = self.published.read().await;
            let (current, _failure) = state.snapshot();
            current.configuration.logs_configuration().page_records
        };
        let query = resource::log_query(uri, page_records)?;
        // The drain writes on a timer, so a read taken right after the request that produced a
        // record would answer without it. This waits for the lane to reach what it has taken.
        crate::logs::settle_for_read().await;
        let Some(store) = self.logs.as_ref() else {
            return Ok(resource::logs_unavailable(
                uri,
                "the workspace log store could not be opened, so this run recorded nothing",
            ));
        };
        match store.recent(&query).await {
            Ok(records) => Ok(resource::rendered_logs(uri, &records)),
            Err(error) => Err(ErrorData::internal_error(
                format!("the log store refused the read: {error}"),
                None,
            )),
        }
    }

    /// Answers one `rift://workspace` read from the current publication.
    async fn read_workspace(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let page_index = resource::workspace_page_index(uri)?;
        let current = Arc::clone(&self.published.read().await.current);
        let configuration = current
            .configuration
            .accepted
            .as_ref()
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?
            .clone();
        let pool = self.engine_pool_for(&current).await;
        let languages = workspace_languages(&current, &configuration, &pool)?;
        let source_digests = current
            .reads
            .visible_workspace_digests()
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        let source = source_digests
            .iter()
            .map(|(path, digest)| {
                let language = current
                    .source_policy
                    .language_for_path(&self.root.join(path.as_str()))
                    .ok()
                    .flatten()
                    .and_then(|effective| {
                        Language::from_identity_segment(effective.identity()).ok()
                    });
                WorkspaceSourceUnit {
                    path: ProjectPath(path.as_str().to_owned()),
                    digest: Digest(file_digest_revision(digest)),
                    language,
                }
            })
            .collect::<Vec<_>>();
        let page_size = WORKSPACE_SOURCE_UNITS_MAX;
        let total_pages = source.len().div_ceil(page_size);
        let start = usize::try_from(page_index)
            .ok()
            .and_then(|page| page.checked_mul(page_size))
            .unwrap_or(source.len());
        let page_source = source.into_iter().skip(start).take(page_size).collect();
        let page = WorkspaceResourcePage {
            configuration_revision: Digest(current.configuration.fingerprint.wire_revision()),
            languages,
            source: page_source,
            pagination: Pagination {
                page_index,
                total_pages: u64::try_from(total_pages).unwrap_or(u64::MAX),
            },
        };
        Ok(resource::rendered_workspace(uri, &page))
    }

    /// Answers the `rift://map` read from the current publication's cached snapshot: a
    /// serialize of the `Arc` [`PublishedWorkspace::map`] already carries, never a
    /// recomputation - the map is rebuilt once per publication.
    async fn read_map(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        let current = Arc::clone(&self.published.read().await.current);
        Ok(resource::rendered_map(uri, &current.map))
    }
}

fn file_digest_revision(digest: rift_index::FileDigest) -> String {
    let mut revision = String::with_capacity(8);
    for byte in &digest.as_bytes()[..4] {
        use std::fmt::Write as _;
        let _ = write!(revision, "{byte:02x}");
    }
    revision
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RiftMcp {
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<rmcp::model::ListToolsResult, ErrorData>> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= rmcp::model::ProtocolVersion::V_2026_07_28);
        std::future::ready(Ok(rmcp::model::ListToolsResult {
            result_type: Some(rmcp::model::ResultType::COMPLETE),
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(rmcp::model::CacheScope::Public),
        }))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourcesResult, ErrorData>> {
        std::future::ready(Ok(ListResourcesResult::with_all_items(
            resource::declared_resources(),
        )))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListResourceTemplatesResult, ErrorData>> {
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(
            resource::declared_templates(),
        )))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if resource::is_workspace_uri(&request.uri) {
            Box::pin(self.read_workspace(&request.uri))
                .await
                .map(Into::into)
        } else if request.uri == resource::MAP_URI {
            self.read_map(&request.uri).await.map(Into::into)
        } else {
            self.read_logs(&request.uri).await.map(Into::into)
        }
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "Read the current workspace: get_symbol and search find \
             declarations, nodes lists syntax nodes at a byte position. \
             The rift://workspace resource reads effective configuration \
             and source files. The rift://logs resource reads the server's own diagnostics, \
             including while a tool refuses.",
        );
        info.meta = Some(crate::identity::identity_meta(&self.identity));
        info
    }
}

/// The engine pool held across requests, replaced when accepted LSP configuration changes.
///
/// Rebuilds recreate the published `ConfigurationState` but never the
/// long-lived services, so the hold compares the published tables against
/// the pool it keeps: unchanged tables reuse the running sessions, and
/// changed tables swap in a fresh pool and shut the replaced one down.
#[derive(Debug)]
pub(crate) struct EngineHold {
    root: PathBuf,
    pool: AsyncMutex<Arc<EnginePool>>,
}

impl EngineHold {
    /// Builds the hold with a pool for startup LSP configuration.
    pub(crate) fn new(
        root: PathBuf,
        definitions: BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: BTreeMap<String, LspProcessKey>,
    ) -> Self {
        let pool = Arc::new(EnginePool::new(&root, definitions, bindings));
        Self {
            root,
            pool: AsyncMutex::new(pool),
        }
    }

    /// The pool serving `engines`: the held pool while its tables are
    /// unchanged, or a replacement built for the new tables.
    ///
    /// A replaced pool's sessions are shut down after the hold's lock is
    /// released, so concurrent callers proceed against the replacement
    /// while the old engines end; a request still holding the replaced
    /// pool finishes its exchange first, because shutdown takes each slot's
    /// own lock.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the swap skips the replaced pool's
    /// graceful shutdown; its children are then killed through the
    /// session's kill-on-drop arming once the last holder drops the pool.
    pub(crate) async fn pool_for(
        &self,
        definitions: BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: BTreeMap<String, LspProcessKey>,
    ) -> Arc<EnginePool> {
        let mut held = self.pool.lock().await;
        if held.built_from(&definitions, &bindings) {
            return Arc::clone(&held);
        }
        let rebuilt = Arc::new(held.reconfigure(&self.root, definitions, bindings));
        let replaced = std::mem::replace(&mut *held, Arc::clone(&rebuilt));
        drop(held);
        replaced.shutdown_replaced_by(&rebuilt).await;
        rebuilt
    }

    /// Ends the held pool's running engines; the pool stays usable and a
    /// later request respawns what it needs.
    pub(crate) async fn shutdown(&self) {
        let held = Arc::clone(&*self.pool.lock().await);
        held.shutdown().await;
    }
}

impl RiftMcp {
    /// The engine pool serving the currently published LSP configuration.
    ///
    /// The hold outlives rebuilds: a publication whose engine tables are
    /// unchanged reuses the running sessions, and one whose tables differ
    /// replaces the pool and shuts the old engines down.
    pub async fn engine_pool(&self) -> Arc<EnginePool> {
        let published = Arc::clone(&self.published.read().await.current);
        self.engine_pool_for(&published).await
    }

    /// The engine pool serving one captured publication's LSP configuration.
    async fn engine_pool_for(&self, published: &PublishedWorkspace) -> Arc<EnginePool> {
        let (definitions, bindings) = published.configuration.lsp_runtime_configuration();
        self.engines.pool_for(definitions, bindings).await
    }

    /// Returns the shared engine hold for transport shutdown paths.
    pub(crate) fn engine_hold(&self) -> Arc<EngineHold> {
        Arc::clone(&self.engines)
    }
}

fn workspace_languages(
    current: &PublishedWorkspace,
    configuration: &WorkspaceConfiguration,
    pool: &EnginePool,
) -> Result<Vec<WorkspaceLanguageSummary>, ErrorData> {
    let policy = current.source_policy.language_policy();
    policy
        .languages()
        .iter()
        .map(|effective| {
            let language = Language::from_identity_segment(effective.identity())
                .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
            let input = configuration.languages.get(effective.identity());
            let enabled = effective.enabled();
            let lsp = input
                .filter(|_input| enabled)
                .and_then(|input| input.lsp.as_ref())
                .map(|lsp| match lsp {
                    rift_protocol::configuration::LanguageLspConfiguration::Named(name) => {
                        (LspProcessKey::Named(name.clone()), name.clone())
                    }
                    rift_protocol::configuration::LanguageLspConfiguration::Inline(_) => (
                        LspProcessKey::Inline(effective.identity().to_owned()),
                        effective.identity().to_owned(),
                    ),
                })
                .map(|(key, process)| WorkspaceLspSummary {
                    process,
                    state: pool
                        .state_for_key(&key)
                        .unwrap_or(rift_protocol::workspace::LspState::Stopped),
                });
            Ok(WorkspaceLanguageSummary {
                language,
                enabled,
                include: effective
                    .include()
                    .iter()
                    .cloned()
                    .map(rift_protocol::read::PathPattern)
                    .collect(),
                exclude: effective
                    .exclude()
                    .iter()
                    .cloned()
                    .map(rift_protocol::read::PathPattern)
                    .collect(),
                execution: enabled && input.is_some_and(|input| input.execution),
                syntax: effective.has_syntax(),
                lsp,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn external_create_of_a_text_file_populates_lexical_search() -> TestResult {
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

        fs::write(
            directory.path().join("notes.txt"),
            "the migration guide covers replacing every legacy unit\n",
        )?;
        search_until_hit(client.peer(), "replacing legacy unit", "notes.txt").await?;

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// One named LSP definition and its exact language binding, built through
    /// serde so the optional keys carry their documented defaults.
    fn lsp_runtime_configuration(
        program: &str,
    ) -> (
        std::collections::BTreeMap<LspProcessKey, LspConfiguration>,
        std::collections::BTreeMap<String, LspProcessKey>,
    ) {
        let key = LspProcessKey::named("ty");
        let configuration = serde_json::from_value(json!({ "command": program }))
            .expect("the LSP configuration fixture deserializes");
        (
            std::collections::BTreeMap::from([(key.clone(), configuration)]),
            std::collections::BTreeMap::from([("python".to_owned(), key)]),
        )
    }

    #[tokio::test]
    async fn engine_hold_reuses_and_replaces_pools_by_runtime_configuration() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (definitions, bindings) = lsp_runtime_configuration("uvx");
        let hold = super::EngineHold::new(directory.path().to_path_buf(), definitions, bindings);
        let (definitions, bindings) = lsp_runtime_configuration("uvx");
        let first = hold.pool_for(definitions, bindings).await;
        let (definitions, bindings) = lsp_runtime_configuration("uvx");
        let second = hold.pool_for(definitions, bindings).await;
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged configuration reuses the held pool"
        );
        let (definitions, bindings) = lsp_runtime_configuration("pyright");
        let replaced = hold.pool_for(definitions, bindings).await;
        assert!(
            !Arc::ptr_eq(&first, &replaced),
            "changed definitions replace the pool"
        );
        let (definitions, bindings) = lsp_runtime_configuration("pyright");
        assert!(replaced.built_from(&definitions, &bindings));
        hold.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn engine_pool_serves_the_published_lsp_configuration() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[lsp.ty]\ncommand = \"uvx\"\n[languages.python]\ninclude = [\"**/*.py\"]\nlsp = \"ty\"\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let pool = server.engine_pool().await;
        let language = |name: &str| rift_protocol::read::Language {
            name: name.to_owned(),
            dialect: None,
        };
        assert!(
            pool.engine_for(&language("python")).is_some(),
            "the accepted binding serves its language"
        );
        assert!(
            pool.engine_for(&language("rust")).is_none(),
            "an unclaimed language answers no engine"
        );
        let again = server.engine_pool().await;
        assert!(
            Arc::ptr_eq(&pool, &again),
            "unchanged published configuration reuses the held pool"
        );
        Ok(())
    }

    /// The design's headline shape - one exact language, one inline command
    /// string, no repeated language list and no argument list - reaches an
    /// engine for that language and for no other.
    #[tokio::test]
    async fn an_inline_command_string_serves_its_exact_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[languages.rust]\nlsp.command = \"rust-analyzer\"\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let pool = server.engine_pool().await;
        let language = |name: &str, dialect: Option<&str>| rift_protocol::read::Language {
            name: name.to_owned(),
            dialect: dialect.map(str::to_owned),
        };
        assert!(
            pool.engine_for(&language("rust", None)).is_some(),
            "an inline command string serves its own language"
        );
        assert!(
            pool.engine_for(&language("typescript", Some("tsx")))
                .is_none(),
            "no other exact language selects that process"
        );
        Ok(())
    }

    /// A language entry turned off contributes neither its inline process
    /// definition nor a binding, so nothing can start it.
    #[tokio::test]
    async fn a_disabled_language_binds_no_engine() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[languages.rust]\nenabled = false\nlsp.command = \"rust-analyzer\"\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let pool = server.engine_pool().await;
        assert!(
            pool.engine_for(&rift_protocol::read::Language {
                name: "rust".to_owned(),
                dialect: None,
            })
            .is_none(),
            "a disabled entry selects no process"
        );
        Ok(())
    }

    #[tokio::test]
    async fn engine_pool_for_a_capture_does_not_adopt_a_later_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[lsp.ty]\ncommand = \"uvx\"\n[languages.python]\ninclude = [\"**/*.py\"]\nlsp = \"ty\"\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        server.validation.cancellation.cancel();
        let earlier = Arc::clone(&server.published.read().await.current);

        super::hermetic_workspace(
            directory.path(),
            "[languages.rust.lsp]\ncommand = \"rust-analyzer\"\n",
        )?;
        let later = stable_candidate(directory.path(), 1)?;
        server.published.write().await.current = Arc::clone(&later);

        let earlier_pool = server.engine_pool_for(&earlier).await;
        let (earlier_definitions, earlier_bindings) =
            earlier.configuration.lsp_runtime_configuration();
        assert!(earlier_pool.built_from(&earlier_definitions, &earlier_bindings));

        let later_pool = server.engine_pool().await;
        let (later_definitions, later_bindings) = later.configuration.lsp_runtime_configuration();
        assert!(later_pool.built_from(&later_definitions, &later_bindings));
        assert!(
            !earlier_pool.built_from(&later_definitions, &later_bindings),
            "captured request keeps its own LSP selection after publication moves"
        );
        Ok(())
    }

    #[tokio::test]
    async fn workspace_resource_reports_named_inline_and_disabled_languages() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("script.rb"), "puts 'beacon'\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[lsp.shared]\ncommand = \"language-server\"\n\
             [languages.ruby]\ninclude = [\"**/*.rb\"]\nexecution = true\nlsp = \"shared\"\n\
             [languages.rust.lsp]\ncommand = \"rust-analyzer\"\n\
             [languages.toml]\nenabled = false\nexecution = true\nlsp = \"shared\"\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let answer = server.read_workspace("rift://workspace").await?;
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = answer
            .contents
            .first()
            .expect("a workspace read answers with one content")
        else {
            unreachable!("a workspace read answers with text");
        };
        let body: serde_json::Value = serde_json::from_str(text)?;
        let languages = body["languages"]
            .as_array()
            .expect("workspace languages are an array");
        let language = |name: &str| {
            languages
                .iter()
                .find(|entry| entry["language"] == serde_json::json!(name))
                .expect("configured language is reported")
        };

        let rust = language("rust");
        assert_eq!(rust["lsp"]["process"], serde_json::json!("rust"));
        assert_eq!(rust["lsp"]["state"], serde_json::json!("stopped"));
        assert_eq!(
            rust["execution"],
            serde_json::json!(false),
            "execution stays off until its own entry enables it"
        );

        let ruby = language("ruby");
        assert_eq!(
            ruby["execution"],
            serde_json::json!(true),
            "an enabled entry carries its own execution permission"
        );
        assert_eq!(ruby["syntax"], serde_json::json!(false));
        assert_eq!(ruby["lsp"]["process"], serde_json::json!("shared"));
        assert_eq!(ruby["lsp"]["state"], serde_json::json!("stopped"));

        let toml = language("toml");
        assert_eq!(toml["enabled"], serde_json::json!(false));
        assert_eq!(toml["execution"], serde_json::json!(false));
        assert!(
            toml.get("lsp").is_none(),
            "disabled language omits LSP state"
        );
        Ok(())
    }
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use rift_index::WorkspaceIndexLimits;

    use rift_protocol::configuration::{
        Duration as WireDuration, LspConfiguration, SearchConfiguration,
        SemanticSearchConfiguration, SemanticSource,
    };
    use rift_protocol::lock::ProductIdentity;
    use rift_protocol::read::{GetSymbolResult, ReadWarning, SearchParams, SearchResult};
    use rift_search::{FusedRanking, ModelSource, RevisionScoped, SemanticReadiness};
    use rift_server::{LspProcessKey, ReadError, ReadFault};

    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    use crate::dependency::empty_dependency_store;
    use crate::validation::RebuildRequest;

    use super::{BlockingExecutor, Parameters, RiftMcp};
    use crate::validation::lexical_double::StoreDouble;
    use crate::validation::{
        LEXICAL_COMMIT_TIMEOUT, LexicalCommitState, LexicalLane, PublishedWorkspace,
        RebuildOutcome, WorkspaceCandidate, build_workspace_candidate, rebuild_workspace,
        record_rebuild_failure, workspace_capture,
    };
    use rift_core::ProjectPath as CoreProjectPath;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    async fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
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

    async fn run_search(server: &RiftMcp, query: &str) -> Result<SearchResult, rmcp::ErrorData> {
        let params: SearchParams = serde_json::from_value(json!({"query": query}))
            .expect("test search parameters must deserialize");
        server
            .search(Parameters(params))
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

    #[tokio::test]
    async fn supervisor_cancellation_keys_off_the_last_clone() -> TestResult {
        let (directory, server) = fixture().await?;
        let validation = Arc::clone(&server.validation);
        let cloned = server.clone();
        drop(server);
        assert!(
            !validation.cancellation.is_cancelled(),
            "the supervisor must keep running while a clone still serves"
        );
        drop(cloned);
        assert!(
            validation.cancellation.is_cancelled(),
            "dropping the last clone must cancel the supervisor"
        );
        drop(directory);
        Ok(())
    }

    #[tokio::test]
    async fn build_skips_one_oversized_file_and_serves_its_warning() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("wide.rs"), "pub fn wide() {}\n")?;
        let limits =
            WorkspaceIndexLimits::new(8, 1, 4096, 8, 32).map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), limits).await?;
        let result = get_symbol(&server, "wide").await?;

        assert!(result.hits.is_empty());
        assert!(result.warnings.iter().any(|warning| matches!(
            warning,
            ReadWarning::SourceUnavailable { unit: Some(unit), detail }
                if unit.0.ends_with("/wide.rs") && detail.contains("file byte limit")
        )));
        Ok(())
    }

    #[tokio::test]
    async fn build_serves_a_workspace_holding_a_file_that_is_not_utf8() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("invalid.rs"), [0xff])?;
        let valid_declaration = "pub fn valid_declaration() {}\n";
        fs::write(directory.path().join("valid.rs"), valid_declaration)?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let result = get_symbol(&server, "valid_declaration")
            .await
            .map_err(|error| format!("the valid file must still serve: {error:?}"))?;
        assert_eq!(result.hits.len(), 1);
        assert!(
            result.warnings.iter().any(|warning| matches!(
                warning,
                ReadWarning::SourceUnavailable { unit: Some(unit), .. } if unit.0.contains("invalid.rs")
            )),
            "the answer must name the file the index omitted: {:?}",
            result.warnings
        );
        drop(directory);
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
        let path = result.hits[0]
            .path
            .as_ref()
            .expect("a project declaration carries path");
        assert!(
            path.0.ends_with("renamed.rs"),
            "hit must resolve to the renamed path: {path:?}"
        );

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
    async fn external_oversized_file_is_skipped_then_recovers_when_bounded() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("lib.rs");
        fs::write(&path, "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
        let tight =
            WorkspaceIndexLimits::new(4, 60, 60, 4, 100).map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;

        let oversized = format!("pub fn oversized() {{}}\n{}", " ".repeat(80));
        fs::write(&path, oversized)?;
        let skipped = get_symbol(&server, "oversized").await?;
        assert!(skipped.hits.is_empty());
        assert!(skipped.warnings.iter().any(|warning| matches!(
            warning,
            ReadWarning::SourceUnavailable { unit: Some(unit), detail }
                if unit.0.ends_with("/lib.rs") && detail.contains("file byte limit")
        )));

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
        assert!(
            recovered,
            "bounded external edit must restore indexed content"
        );
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
        assert!(supervisor.validation.cancellation.is_cancelled());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        supervisor.shutdown(deadline).await?;
        supervisor
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(30))
            .await?;
        assert!(supervisor.validation.task.lock().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn server_table_sizes_blocking_pool_and_queue_wait() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let configured = "[server]\nnum_workers = 2\nworker_queue_timeout = \"1250ms\"\n";
        super::hermetic_workspace(directory.path(), configured)?;
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
            default_table.worker_queue_timeout.milliseconds()
        );
        assert_eq!(
            server.blocking.operations.available_permits() as u64,
            default_table.num_workers
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalid_configuration_builds_default_blocking_policy() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("rift.toml"),
            "[server]\nnum_workers = 0\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let default_table = rift_protocol::configuration::ServerConfiguration::default();
        assert_eq!(
            server.blocking.operations.available_permits() as u64,
            default_table.num_workers
        );
        Ok(())
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
            .expect("queued task must reach acceptance");
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
            .expect("timed operation must reach acceptance");
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
                    capability: "probe".to_owned(),
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
            .expect_err("closed semaphore must fail acceptance");
        assert!(matches!(error.fault(), ReadFault::Task { .. }));
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
            ["get_symbol", "nodes", "search"]
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
        assert_eq!(
            structured["pagination"],
            json!({ "page_index": 0, "total_pages": 1 })
        );

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
    async fn initialize_schema_digest_matches_the_canonical_served_tool_list() -> TestResult {
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

        let schema_document = crate::schema::schema_document();
        let schema_digest = format!("{:x}", Sha256::digest(schema_document.as_bytes()));
        let peer_info = client
            .peer_info()
            .ok_or("initialize must advertise server information")?;
        let initialize = serde_json::to_value(peer_info)?;
        let identity: ProductIdentity =
            serde_json::from_value(initialize["_meta"]["sh.volar/rift"].clone())?;
        assert_eq!(
            identity.schema_digest, schema_digest,
            "initialize must name the digest of the canonical served tool document"
        );

        let document: serde_json::Value = serde_json::from_str(&schema_document)?;
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

    /// A multi-word prose query neither identifier search path can serve: no line contains
    /// the literal phrase, and no declaration name contains it either. `scale_value`'s doc
    /// comment supplies just the word "units" and `guide.txt` supplies "replace" and "all",
    /// so only the lexical search-index tier's per-term matching can produce either hit.
    #[tokio::test]
    async fn client_search_merges_lexical_symbol_and_text_file_hits() -> TestResult {
        let directory = tempfile::tempdir()?;
        let lib_rs = "/// Converts a raw measurement into base units.\npub fn scale_value(value: f64) -> f64 {\n    value * 2.0\n}\n";
        fs::write(directory.path().join("lib.rs"), lib_rs)?;
        let guide_txt = "This document explains how to replace all safely.\n";
        fs::write(directory.path().join("guide.txt"), guide_txt)?;
        super::hermetic_workspace(directory.path(), "")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        // The population lane runs the run's first pass off the request path, so `build`
        // returned before the tier could answer. Poll for the file hit under a bound: the
        // same pass carries the symbol hit asserted below.
        let structured = search_until_hit(client.peer(), "replace all units", "guide.txt").await?;
        let results = structured["results"]
            .as_array()
            .ok_or("results must be an array")?;

        // Neither hit is found through the identifier or line matcher - `guide.txt` never
        // joins `index.files()`, and no source line spells the query's exact phrase - so
        // both reach the answer through the ranked lane alone, tagged `ranked`, never the
        // literal-content claim `content` makes.
        let file_hit = results
            .iter()
            .find(|hit| hit["hit"]["target"] == "file" && hit["path"] == json!("guide.txt"))
            .ok_or_else(|| format!("guide.txt text-file hit missing: {structured:#}"))?;
        assert_eq!(file_hit["matched_by"], json!(["ranked"]));

        let symbol_hit = results
            .iter()
            .find(|hit| {
                hit["hit"]["target"] == "symbol" && hit["hit"]["symbol"]["name"] == "scale_value"
            })
            .ok_or_else(|| format!("scale_value doc-comment hit missing: {structured:#}"))?;
        assert!(
            symbol_hit["matched_by"]
                .as_array()
                .is_some_and(|fields| fields.contains(&json!("ranked"))),
            "the symbol hit must name ranked as a matched field: {symbol_hit:#}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Corrupt bytes fail `SQLite`'s file-format check deterministically. The server starts
    /// without database-backed search or logs and leaves those bytes in place for recovery.
    #[tokio::test]
    async fn build_preserves_a_corrupt_database_and_serves_without_it() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let state_directory = directory.path().join(".rift");
        fs::create_dir_all(&state_directory)?;
        let database_path = state_directory.join("db");
        let corrupt = b"not a sqlite database";
        fs::write(&database_path, corrupt)?;
        super::hermetic_workspace(directory.path(), "")?;

        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())
            .await
            .map_err(|error| format!("corrupt database must not fail startup: {error:?}"))?;

        assert!(server.search_index.is_none());
        assert!(server.logs.is_none());
        let unavailable = serde_json::to_string(&server.read_logs("rift://logs").await?)?;
        assert!(
            unavailable.contains("the workspace log store could not be opened"),
            "{unavailable}"
        );
        assert_eq!(fs::read(database_path)?, corrupt);
        Ok(())
    }

    /// `build` returns before the lexical lane's first transaction ends and before the
    /// population lane runs the run's first pass.
    ///
    /// Awaiting either inside `build` held the first answer on a real workspace: the
    /// population pass for around fifteen seconds, and a whole lexical replace of every
    /// unit for longer than the freshness deadline. The caller sees the fix directly: the
    /// first search a freshly built server answers is answered, ranked by the store when
    /// its transaction has landed and by identifier matching with the warning naming the
    /// revision when it has not, and a later search is ranked by the store.
    ///
    /// `max_chunk` at its enforced minimum against a megabyte of text is what puts a
    /// thousand lexical units in that transaction, so it is real work next to the one
    /// in-process call that follows `build`.
    #[tokio::test]
    async fn build_answers_before_the_store_holds_its_tree() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("guide.txt"),
            "the earlier guide covers every legacy sensor ".repeat(24_000),
        )?;
        super::hermetic_workspace(directory.path(), "[search.text]\nmax_chunk = \"1kb\"\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let first = run_search(&server, "legacy sensor").await?;
        assert!(
            !first.results.is_empty(),
            "the first answer is served whether or not the transaction has landed: {first:#?}"
        );
        if !store_ranked(&first) {
            let revision = server
                .published
                .read()
                .await
                .current
                .reads
                .tree_revision()
                .to_owned();
            let detail = first
                .warnings
                .iter()
                .find_map(|warning| match warning {
                    ReadWarning::LexicalRankingUnavailable { detail } => Some(detail.clone()),
                    _ => None,
                })
                .ok_or("an unranked first answer names the commit it is waiting on")?;
            assert!(
                detail.contains(&format!("still committing tree revision {revision}")),
                "{detail}"
            );
        }
        let ranked = search_after_population(&server, "legacy sensor").await?;
        assert!(
            !ranked.results.is_empty(),
            "the committed unit set is searchable: {ranked:#?}"
        );
        Ok(())
    }

    /// Calls one tool expecting a Rift wire error and returns the JSON-RPC
    /// error object.
    ///
    /// The population lane's first pass is waited for before the call, because a refusal
    /// the search index itself raises - the query-term limit - is only reached once the
    /// revision guard trusts the store. Nothing writes into the fixture, so the store stays
    /// stamped for the call that follows.
    async fn failing_call(
        arguments_value: &serde_json::Value,
        tool: &'static str,
    ) -> TestResult<rmcp::ErrorData> {
        let (_directory, server) = fixture().await?;
        search_after_population(&server, "beacon").await?;
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
        assert!(
            wire.get("causes").is_none(),
            "a failure with no causal chain must omit causes"
        );
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

    /// A member sent in the wrong shape is refused by the member's own name,
    /// with an example of the value it takes.
    ///
    /// The refusal never names a Rust type: `PathSelector` is a name only this
    /// repository holds, and a caller reading it has nothing to look up. The
    /// example comes from the same schema the caller lists, so it can be sent
    /// back verbatim.
    #[tokio::test]
    async fn a_member_in_the_wrong_shape_is_refused_by_name_with_an_example() -> TestResult {
        let data = failing_call(
            &json!({"query": "beacon", "paths": ["crates/rift-server/src/read.rs"]}),
            "search",
        )
        .await?;
        assert_eq!(data.code, ErrorCode(-32000));
        assert_eq!(
            data.message.as_ref(),
            "the request does not match the documented form: tool search, field paths, \
             accepted exclude, force_include, include, \
             example {\"exclude\":[\"src/generated/**\"],\"include\":[\"src/**\"]}; \
             correct the reported field and resend the request"
        );
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("invalid_request"));
        assert_eq!(wire["retry"], json!("never"));
        Ok(())
    }

    /// A member the tool does not serve is refused by naming the members it
    /// does, and an example of the whole request.
    #[tokio::test]
    async fn an_unserved_member_is_refused_by_naming_the_served_ones() -> TestResult {
        let data =
            failing_call(&json!({"query": "beacon", "path": "src/lib.rs"}), "search").await?;
        let message = data.message.as_ref();
        assert!(
            message.starts_with(
                "the request does not match the documented form: tool search, accepted "
            ),
            "{message}"
        );
        assert!(message.contains("paths"), "{message}");
        assert!(message.contains("example {"), "{message}");
        assert!(
            !message.contains("unknown field") && !message.contains("struct"),
            "a refusal never speaks serde's grammar: {message}"
        );
        Ok(())
    }

    /// A closed set's member is refused by listing the values it takes.
    #[tokio::test]
    async fn a_value_outside_a_closed_set_is_refused_by_listing_the_set() -> TestResult {
        let data = failing_call(&json!({"query": "beacon", "target": "nodes"}), "search").await?;
        let message = data.message.as_ref();
        assert!(
            message.contains("field target, accepted all, file, symbol"),
            "{message}"
        );
        assert!(
            !message.contains("unknown variant"),
            "a refusal never speaks serde's grammar: {message}"
        );
        Ok(())
    }

    /// Every served tool's input schema carries an authored example, at its
    /// root and on every object a caller can address inside it.
    ///
    /// A refusal shows the caller an example of the value it should have sent,
    /// read from this schema. An object with no example leaves the caller with
    /// the example of whatever holds it, which is the defect this gate exists
    /// to keep out.
    #[test]
    fn every_served_input_schema_carries_an_example_for_every_object() {
        for tool in crate::schema::tool_listing() {
            let schema = serde_json::Value::Object(tool.input_schema.as_ref().clone());
            assert!(
                schema.get("examples").is_some_and(|examples| examples
                    .as_array()
                    .is_some_and(|examples| !examples.is_empty())),
                "tool {} states no example request",
                tool.name
            );
            let definitions = schema
                .get("$defs")
                .and_then(serde_json::Value::as_object)
                .into_iter()
                .flatten();
            for (name, definition) in definitions {
                if definition.get("properties").is_none() {
                    continue;
                }
                assert!(
                    definition.get("examples").is_some_and(|examples| examples
                        .as_array()
                        .is_some_and(|examples| !examples.is_empty())),
                    "tool {}: {name} states no example value",
                    tool.name
                );
            }
        }
    }

    #[tokio::test]
    async fn client_search_query_term_limit_carries_typed_limit_evidence() -> TestResult {
        // One more distinct term than the lexical adapter's default `query_terms_max`.
        let terms: Vec<String> = (0..33).map(|index| format!("term{index}")).collect();
        let query = terms.join(" ");
        let data = failing_call(&json!({ "query": query }), "search").await?;
        assert_eq!(data.code, ErrorCode(-32000));
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("limit_exceeded"));
        assert_eq!(
            wire["limit"],
            json!({ "field": "query_terms_max", "limit": 32, "required": 33 }),
            "the query-term-limit refusal must carry typed wire evidence: {wire:#}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reads_fail_fast_after_watcher_failure() -> TestResult {
        let (_directory, server) = fixture().await?;
        let _ = server.validation.observe_watch_failure();
        let error = get_symbol(&server, "beacon")
            .await
            .expect_err("a failed watcher must refuse current reads");
        assert!(
            error.message.contains("filesystem watcher failed"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }

    /// Bound on one read that must answer without waiting for a rebuild; the readiness
    /// budget a waiting read spends is far longer.
    const UNWAITED_READ_MAX: Duration = Duration::from_secs(5);

    /// A server whose index supervisor is not running and whose watcher is gone, so the
    /// observations and the failure a test records are the only ones the published state
    /// sees. The invalidation receiver stays open so an observation still lands.
    struct UnsupervisedServer {
        server: RiftMcp,
        context: crate::validation::IndexSupervisorContext,
        _invalidations: tokio::sync::mpsc::Receiver<()>,
    }

    async fn unsupervised_fixture() -> TestResult<(tempfile::TempDir, UnsupervisedServer)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
        let assembled = unsupervised_server(directory.path()).await?;
        Ok((directory, assembled))
    }

    /// Assembles a server over `root` and drops its watcher.
    async fn unsupervised_server(root: &std::path::Path) -> TestResult<UnsupervisedServer> {
        let super::AssembledServer {
            server,
            watcher,
            invalidations,
            context,
        } = RiftMcp::assemble(
            super::absolute_root(root)?,
            WorkspaceIndexLimits::default(),
            None,
            LexicalLane::spawn,
        )
        .await?;
        drop(watcher);
        Ok(UnsupervisedServer {
            server,
            context,
            _invalidations: invalidations,
        })
    }

    #[tokio::test]
    async fn engine_reference_capture_rejects_source_and_configuration_movement() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[languages.rust.lsp]\ncommand = 'missing-engine'\n",
        )?;
        let assembled = unsupervised_server(directory.path()).await?;
        let server = &assembled.server;
        let published = Arc::clone(&server.published.read().await.current);
        assert!(server.engine_tree_matches(&published).await?);
        fs::write(directory.path().join("lib.rs"), "pub fn later() {}\n")?;
        assert!(!server.engine_tree_matches(&published).await?);
        let params = serde_json::from_value(json!({"traversal": {
            "seed": "rift://symbol/rust/lib.rs/beacon",
            "direction": "incoming", "facets": ["references"], "depth": 1
        }}))?;
        let resolved = super::ResolvedWorkspace::current(Arc::clone(&published));
        assert!(
            server
                .engine_references(&resolved, &params)
                .await?
                .is_none(),
            "moved source asks for a fresh publication, not an empty engine answer"
        );
        for override_fields in [
            json!({"direction": "outgoing", "facets": ["references"]}),
            json!({"direction": "incoming", "facets": ["calls"]}),
        ] {
            let mut request = json!({"traversal": {
                "seed": "rift://symbol/rust/lib.rs/beacon", "depth": 1
            }});
            request["traversal"]["direction"] = override_fields["direction"].clone();
            request["traversal"]["facets"] = override_fields["facets"].clone();
            let indexed = serde_json::from_value(request)?;
            assert!(
                server
                    .engine_references(&resolved, &indexed)
                    .await?
                    .is_some(),
                "an indexed traversal must not recapture source for unused engine references"
            );
        }
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        assert!(server.engine_tree_matches(&published).await?);
        super::hermetic_workspace(directory.path(), "[languages.rust]\nenabled = false\n")?;
        assert!(!server.engine_tree_matches(&published).await?);
        Ok(())
    }

    #[tokio::test]
    async fn incoming_traversal_without_an_engine_keeps_its_indexed_snapshot() -> TestResult {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        let published = Arc::clone(&server.published.read().await.current);
        fs::write(directory.path().join("lib.rs"), "pub fn later() {}\n")?;
        let params = serde_json::from_value(json!({"traversal": {
            "seed": "rift://symbol/rust/lib.rs/beacon",
            "direction": "incoming", "facets": ["references"], "depth": 1
        }}))?;
        let resolved = super::ResolvedWorkspace::current(published);
        assert!(
            server
                .engine_references(&resolved, &params)
                .await?
                .is_some(),
            "no selected engine leaves the indexed read unchanged"
        );
        Ok(())
    }

    #[tokio::test]
    async fn engine_reference_recapture_uses_the_changed_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("service.py"),
            "def serve(port: int) -> int:\n    return port\n",
        )?;
        fs::write(
            directory.path().join("main.py"),
            "def caller() -> int:\n    return 0\n",
        )?;
        super::hermetic_workspace(
            directory.path(),
            "[providers.binding]\nenabled = false\n[languages.python.lsp]\nembedded = 'ty'\nretry = { attempts = 2, delay = '1ms', delay_limit = '1ms' }\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let resolved = server
            .published_workspace(rift_protocol::error::ErrorPhase::Read)
            .await?;
        let before = resolved.published.fingerprint.clone();
        let params = serde_json::from_value(json!({"target": "symbol", "traversal": {
            "seed": "rift://symbol/python/service.py/serve",
            "direction": "incoming", "facets": ["references"], "depth": 1
        }}))?;
        fs::write(
            directory.path().join("main.py"),
            "from service import serve\n\ndef caller() -> int:\n    return serve(8080)\n",
        )?;
        assert!(
            server
                .engine_references(&resolved, &params)
                .await?
                .is_none(),
            "the captured publication predates the new caller"
        );
        let budget = server.readiness_timeout().await;
        let (current, _, references) = tokio::time::timeout(
            budget,
            server.current_tree_references(resolved, None, &params, budget),
        )
        .await
        .map_err(|_| "reference recapture exceeded the request deadline")??;
        assert_ne!(
            current.published.fingerprint, before,
            "the loop captures the changed publication"
        );
        let answer = current
            .published
            .reads
            .search_with_references(&params, &[], &references)?;
        let rendered = serde_json::to_value(answer)?;
        assert_eq!(
            rendered["results"][0]["hit"]["symbol"]["name"], "caller",
            "{rendered:#}"
        );
        server.engine_pool().await.shutdown().await;
        Ok(())
    }

    /// Writes a second declaration into the fixture's `lib.rs`, observes that path
    /// `events` times, and records one rebuild failure at the epoch the last observation
    /// reached.
    async fn fail_rebuild_after_events(
        root: &std::path::Path,
        server: &RiftMcp,
        events: u64,
    ) -> TestResult<u64> {
        fs::write(
            root.join("lib.rs"),
            "pub fn beacon() {}\npub fn lantern() {}\n",
        )?;
        fail_rebuild_after_observing(server, "lib.rs", events).await
    }

    /// Observes `path` `events` times and records one rebuild failure at the epoch the
    /// last observation reached.
    async fn fail_rebuild_after_observing(
        server: &RiftMcp,
        path: &str,
        events: u64,
    ) -> TestResult<u64> {
        let mut epoch = 0;
        for _event in 0..events {
            epoch = server
                .validation
                .observe_paths([CoreProjectPath::new(path)?])
                .map_err(|error| format!("observation must land: {error:?}"))?;
        }
        record_failure_at(server, epoch).await?;
        Ok(epoch)
    }

    /// Records one rebuild failure at `epoch`, the one the last observation reached.
    async fn record_failure_at(server: &RiftMcp, epoch: u64) -> TestResult {
        let published = Arc::clone(&server.published);
        let validation = Arc::clone(&server.validation);
        let recorded = tokio::task::spawn_blocking(move || {
            record_rebuild_failure(
                &published,
                &validation,
                epoch,
                ReadFault::unavailable("test rebuild", "injected failure"),
            )
        })
        .await?;
        assert!(
            recorded,
            "the failure must be recorded at the observed epoch"
        );
        Ok(())
    }

    /// The one `stale_index` warning `warnings` carries.
    fn stale_index_of(warnings: &[ReadWarning]) -> TestResult<(&str, &str, &str)> {
        let stale = warnings.iter().filter_map(|warning| match warning {
            ReadWarning::StaleIndex {
                index_tree_revision,
                captured_tree_revision,
                detail,
            } => Some((
                index_tree_revision.0.as_str(),
                captured_tree_revision.0.as_str(),
                detail.as_str(),
            )),
            _ => None,
        });
        let found: Vec<_> = stale.collect();
        match found.as_slice() {
            [one] => Ok(*one),
            _ => Err(format!("exactly one stale_index warning is carried: {warnings:?}").into()),
        }
    }

    #[tokio::test]
    async fn a_read_under_a_rebuild_failure_at_the_observed_epoch_answers_stale() -> TestResult {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        let epoch = fail_rebuild_after_events(directory.path(), server, 3).await?;
        assert_eq!(epoch, 3);

        let answer = tokio::time::timeout(UNWAITED_READ_MAX, run_search(server, "beacon"))
            .await
            .map_err(|_| "a read under a recorded failure must not wait")??;

        assert!(!answer.results.is_empty(), "the published snapshot answers");
        let (index, captured, detail) = stale_index_of(&answer.warnings)?;
        assert_eq!(
            index,
            server.published.read().await.current.reads.tree_revision()
        );
        assert_eq!(
            captured.len(),
            index.len(),
            "both digests take the wire form"
        );
        assert_ne!(
            captured, index,
            "the captured tree is ahead of the snapshot"
        );
        assert!(
            detail.contains("filesystem epoch 3 failed and the tree is at epoch 3"),
            "{detail}"
        );
        assert!(
            detail.contains(&format!(
                "served from the snapshot at tree revision {index} rather than the captured \
                 tree revision {captured}"
            )),
            "{detail}"
        );
        assert!(detail.contains("injected failure"), "{detail}");
        assert!(
            detail.contains("the next filesystem event retries"),
            "{detail}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_read_under_a_rebuild_failure_after_a_text_file_moved_names_the_recorded_files()
    -> TestResult {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        fs::write(directory.path().join("notes.txt"), "beacon notes\n")?;
        fail_rebuild_after_observing(server, "notes.txt", 1).await?;

        let answer = tokio::time::timeout(UNWAITED_READ_MAX, run_search(server, "beacon"))
            .await
            .map_err(|_| "a read under a recorded failure must not wait")??;

        assert!(!answer.results.is_empty(), "the published snapshot answers");
        let (index, captured, detail) = stale_index_of(&answer.warnings)?;
        assert_eq!(captured, index, "a text file folds into no tree revision");
        assert!(
            detail.contains(&format!(
                "the tree is at epoch 1; the syntax-indexed files still fold to tree revision \
                 {index}, and recorded files outside them moved, so the answer was served \
                 from the published snapshot: "
            )),
            "{detail}"
        );
        assert!(!detail.contains("the configuration file moved"), "{detail}");
        assert!(detail.contains("injected failure"), "{detail}");
        Ok(())
    }

    /// The configuration file is itself syntax-indexed unless the `[source]` policy leaves
    /// it out, so this workspace excludes it: a write to it then moves the configuration
    /// and no recorded file.
    #[tokio::test]
    async fn a_read_under_a_rebuild_failure_after_the_configuration_moved_names_the_file()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let excluding = "[source]\nexclude = [\"rift.toml\"]\n";
        super::hermetic_workspace(directory.path(), excluding)?;
        let assembled = unsupervised_server(directory.path()).await?;
        let server = &assembled.server;
        super::hermetic_workspace(directory.path(), &format!("{excluding}# moved\n"))?;
        let epoch = server
            .validation
            .observe_whole_workspace()
            .map_err(|error| format!("observation must land: {error:?}"))?;
        record_failure_at(server, epoch).await?;

        let answer = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(server, "beacon"))
            .await
            .map_err(|_| "a read under a recorded failure must not wait")??;

        assert_eq!(answer.hits.len(), 1, "the published snapshot answers");
        let (index, captured, detail) = stale_index_of(&answer.warnings)?;
        assert_eq!(
            captured, index,
            "an excluded configuration file folds into no tree revision"
        );
        assert!(
            detail.contains(&format!(
                "the syntax-indexed files still fold to tree revision {index}, and the \
                 configuration file moved, so the answer was served from the published \
                 snapshot: "
            )),
            "{detail}"
        );
        assert!(!detail.contains("recorded files"), "{detail}");
        Ok(())
    }

    #[tokio::test]
    async fn a_capture_movement_names_the_recorded_files_and_the_configuration_file() -> TestResult
    {
        use super::CaptureMovement;

        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        let current = Arc::clone(&server.published.read().await.current);
        let published = current.reads.workspace_digests();
        let (matching, configuration) = server.capture_tree(&current).await?;
        let changes = rift_index::PathChanges::between(&published, &matching);
        assert_eq!(
            CaptureMovement::from_changes(
                &changes,
                current.configuration.fingerprint != configuration
            ),
            None,
            "a capture matching its publication moved nothing"
        );
        assert_eq!(
            CaptureMovement::from_changes(
                &changes,
                current.configuration.fingerprint
                    != super::ConfigurationFingerprint::MissingOrUnreadable
            ),
            Some(CaptureMovement::Configuration)
        );

        fs::write(directory.path().join("notes.txt"), "beacon notes\n")?;
        let (with_text, configuration) = server.capture_tree(&current).await?;
        let changes = rift_index::PathChanges::between(&published, &with_text);
        assert_eq!(
            CaptureMovement::from_changes(
                &changes,
                current.configuration.fingerprint != configuration
            ),
            Some(CaptureMovement::RecordedFiles)
        );
        assert_eq!(
            CaptureMovement::from_changes(
                &changes,
                current.configuration.fingerprint
                    != super::ConfigurationFingerprint::MissingOrUnreadable
            ),
            Some(CaptureMovement::RecordedFilesAndConfiguration)
        );
        assert_eq!(
            CaptureMovement::RecordedFilesAndConfiguration.clause(),
            "recorded files outside them and the configuration file moved"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_read_under_a_rebuild_failure_behind_the_observed_epoch_answers_stale() -> TestResult
    {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        fail_rebuild_after_events(directory.path(), server, 3).await?;
        for _event in 0..2 {
            server
                .validation
                .observe_paths([CoreProjectPath::new("lib.rs")?])
                .map_err(|error| format!("observation must land: {error:?}"))?;
        }
        assert_eq!(server.validation.observed_epoch(), 5);

        let answer = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(server, "beacon"))
            .await
            .map_err(|_| "a read under a recorded failure must not wait")??;

        assert_eq!(answer.hits.len(), 1, "the published snapshot answers");
        let (_, _, detail) = stale_index_of(&answer.warnings)?;
        assert!(
            detail.contains("filesystem epoch 3 failed and the tree is at epoch 5"),
            "{detail}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_successful_rebuild_clears_the_stale_warning() -> TestResult {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        fail_rebuild_after_events(directory.path(), server, 3).await?;
        stale_index_of(&run_search(server, "beacon").await?.warnings)?;

        let request = server.validation.take_pending();
        let capture = workspace_capture(&assembled.context.dependencies);
        let outcome = rebuild_workspace(&assembled.context, request, capture).await?;
        assert_eq!(outcome, RebuildOutcome::Published);

        let answer = run_search(server, "lantern").await?;
        assert!(
            !answer.results.is_empty(),
            "the fresh snapshot serves the declaration the failed rebuild missed"
        );
        assert!(
            !answer
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::StaleIndex { .. })),
            "a published rebuild clears the recorded failure: {:?}",
            answer.warnings
        );
        Ok(())
    }

    /// Most one test waits for a request the bounded loop answers on its own.
    const RECONCILED_READ_MAX: std::time::Duration = std::time::Duration::from_secs(60);

    /// The capture a served request runs: every visible file's digest under `current`'s
    /// accepted policy, and the configuration file's state.
    fn captured_tree(
        root: &std::path::Path,
        current: &super::PublishedWorkspace,
    ) -> Result<(super::WorkspaceDigests, super::ConfigurationFingerprint), super::ReadError> {
        let limits = current
            .configuration
            .index_limits(WorkspaceIndexLimits::default())?;
        let visibility = current.configuration.source_visibility();
        let text_inclusion = current.configuration.text_inclusion();
        let languages = current.configuration.language_file_selections();
        let digests = super::capture_digests_with_languages(
            root,
            limits,
            &visibility,
            &text_inclusion,
            &languages,
        )
        .map_err(|error| super::ReadError::from(ReadFault::Index(error)))?;
        Ok((digests, super::configuration_fingerprint(root)))
    }

    /// Forces every request's capture to rewrite `lib.rs` before it reads the tree, so no
    /// attempt ever captures the tree the publication it resolved was built for.
    fn capture_a_tree_that_keeps_moving(server: &RiftMcp, root: &std::path::Path) {
        let root = root.to_path_buf();
        let rounds = Arc::new(std::sync::atomic::AtomicU64::new(0));
        server.force_capture(move |current| {
            let round = rounds.fetch_add(1, Ordering::Relaxed);
            fs::write(
                root.join("lib.rs"),
                format!("pub fn beacon{round}() {{}}\n"),
            )
            .expect("the fixture write must land");
            captured_tree(&root, current)
        });
    }

    /// A tree that moves on every attempt spends the read's whole attempt budget, and the
    /// read answers from the publication with a `stale_index` warning naming what the last
    /// capture found ahead, inside the detail's advertised bound.
    #[tokio::test]
    async fn a_read_whose_tree_keeps_moving_answers_stale() -> TestResult {
        let (directory, server) = fixture().await?;
        capture_a_tree_that_keeps_moving(&server, directory.path());

        let answer = tokio::time::timeout(RECONCILED_READ_MAX, get_symbol(&server, "beacon"))
            .await
            .map_err(|_| "a read whose tree keeps moving must answer within the bound")??;

        let (index, captured, detail) = stale_index_of(&answer.warnings)?;
        assert_ne!(
            captured, index,
            "the captured tree is ahead of the snapshot"
        );
        assert!(
            detail.contains("bounded reconciliation attempts"),
            "{detail}"
        );
        assert!(detail.contains("lib.rs moved"), "{detail}");
        assert!(
            detail.contains("the rebuild those changes asked for publishes next"),
            "{detail}"
        );
        assert!(detail.len() <= super::WARNING_DETAIL_BYTES_MAX, "{detail}");
        Ok(())
    }

    /// The same condition on a node listing: the listing answers from the publication and
    /// carries the same warning.
    #[tokio::test]
    async fn a_node_listing_whose_tree_keeps_moving_answers_stale() -> TestResult {
        let (directory, server) = fixture().await?;
        capture_a_tree_that_keeps_moving(&server, directory.path());
        let params = serde_json::from_value(json!({"path": "lib.rs", "position": 8}))?;

        let answer = tokio::time::timeout(RECONCILED_READ_MAX, server.nodes(Parameters(params)))
            .await
            .map_err(|_| "a node listing whose tree keeps moving must answer within the bound")??
            .0;

        let (_index, _captured, detail) = stale_index_of(&answer.warnings)?;
        assert!(
            detail.contains("bounded reconciliation attempts"),
            "{detail}"
        );
        Ok(())
    }

    /// The same condition on `search`: the answer carries the publication's own rows and
    /// the warning, rather than the refusal the spent bound used to raise.
    #[tokio::test]
    async fn a_search_whose_tree_keeps_moving_answers_stale() -> TestResult {
        let (directory, server) = fixture().await?;
        capture_a_tree_that_keeps_moving(&server, directory.path());

        let answer = tokio::time::timeout(RECONCILED_READ_MAX, run_search(&server, "beacon"))
            .await
            .map_err(|_| "a search whose tree keeps moving must answer within the bound")??;

        let (_index, _captured, detail) = stale_index_of(&answer.warnings)?;
        assert!(
            detail.contains("bounded reconciliation attempts"),
            "{detail}"
        );
        Ok(())
    }

    /// An epoch that moves while every digest stays equal is an observation that changed
    /// no byte: the capture folds to the publication's fingerprint, so the read answers
    /// from it with nothing to warn about.
    #[tokio::test]
    async fn a_read_whose_epoch_moves_with_every_digest_equal_answers() -> TestResult {
        let (_directory, server) = fixture().await?;
        let validation = Arc::clone(&server.validation);
        let unit = CoreProjectPath::new("lib.rs")?;
        server.force_capture(move |current| {
            // What a write that lands the bytes a file already held leaves behind.
            validation.observe_paths([unit.clone()])?;
            Ok((
                current.reads.workspace_digests(),
                current.configuration.fingerprint,
            ))
        });

        let answer = tokio::time::timeout(RECONCILED_READ_MAX, get_symbol(&server, "beacon"))
            .await
            .map_err(|_| "an observation that changed no byte must not make a read wait")??;

        assert_eq!(answer.hits.len(), 1, "the publication answers: {answer:?}");
        assert!(
            stale_index_of(&answer.warnings).is_err(),
            "a publication that answers for the tree warns about nothing: {:?}",
            answer.warnings
        );
        Ok(())
    }

    /// A capture that fails says nothing about the tree, so the read refuses with the
    /// capture's own failure rather than answering from a publication nothing checked.
    #[tokio::test]
    async fn a_read_whose_capture_fails_refuses() -> TestResult {
        let (_directory, server) = fixture().await?;
        server.force_capture(|_current| {
            Err(ReadFault::unavailable(
                "test capture",
                "injected capture failure",
            ))
        });

        let refusal = tokio::time::timeout(RECONCILED_READ_MAX, get_symbol(&server, "beacon"))
            .await
            .map_err(|_| "a failed capture must refuse within the bound")?
            .expect_err("a failed capture must refuse");

        assert!(
            refusal.message.contains("injected capture failure"),
            "{refusal:?}"
        );
        Ok(())
    }

    /// Captures a candidate successfully, then edits its source before publication.
    async fn supersede_rebuild(assembled: &UnsupervisedServer, next_source: String) -> TestResult {
        let server = &assembled.server;
        let validation = Arc::clone(&server.validation);
        let dependencies = Arc::clone(&server.dependencies);
        let path = CoreProjectPath::new("lib.rs")?;
        let outcome = rebuild_workspace(
            &assembled.context,
            server.validation.take_pending(),
            move |root: &std::path::Path, limits, request: &RebuildRequest| {
                let candidate = build_workspace_candidate(root, limits, request, &dependencies)?;
                assert!(matches!(candidate, WorkspaceCandidate::Stable { .. }));
                fs::write(root.join("lib.rs"), &next_source)
                    .expect("the later source edit must land");
                validation.observe_paths([path.clone()])?;
                Ok(candidate)
            },
        )
        .await?;
        assert_eq!(outcome, RebuildOutcome::Superseded);
        Ok(())
    }

    /// Successful captures can all be superseded before publication. The recorded
    /// completed work lets a later read answer from the existing snapshot with its warning.
    #[tokio::test]
    async fn a_read_after_successful_superseded_rebuilds_answers_without_publication() -> TestResult
    {
        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        let path = CoreProjectPath::new("lib.rs")?;
        fs::write(directory.path().join("lib.rs"), "pub fn lantern0() {}\n")?;
        server.validation.observe_paths([path.clone()])?;
        for round in 1..=super::INDEX_CAPTURE_ATTEMPTS_MAX {
            supersede_rebuild(&assembled, format!("pub fn lantern{round}() {{}}\n")).await?;
            assert_eq!(server.published.read().await.current.epoch, 0);
        }
        let (published, failure) = server.published.read().await.snapshot();
        assert_eq!(published.epoch, 0);
        assert!(
            failure.is_none(),
            "successful captures record no rebuild failure"
        );
        let (expected_capture, _) = captured_tree(directory.path(), &published)?;
        let expected_revision = super::wire_digest(
            expected_capture
                .tree_revision()
                .ok_or("captured tree revision")?,
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rounds = Arc::clone(&calls);
        let root = directory.path().to_path_buf();
        server.force_capture(move |current| {
            let round = rounds.fetch_add(1, Ordering::Relaxed);
            if round != 0 {
                return Err(ReadFault::unavailable(
                    "test capture",
                    "a second capture repeats proven movement",
                ));
            }
            captured_tree(&root, current)
        });

        let answer = tokio::time::timeout(UNWAITED_READ_MAX, run_search(server, "beacon"))
            .await
            .map_err(
                |_| "a read must not wait for a publication superseded builds cannot produce",
            )??;
        assert!(
            !answer.results.is_empty(),
            "the original publication answers"
        );
        let (index, captured, detail) = stale_index_of(&answer.warnings)?;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(index, published.reads.tree_revision());
        assert_eq!(captured, expected_revision.0);
        assert_ne!(index, captured);
        assert!(detail.contains("lib.rs moved"), "{detail}");
        assert!(detail.contains("superseded before publication"), "{detail}");
        assert!(
            !detail.contains("bounded reconciliation attempts"),
            "{detail}"
        );
        Ok(())
    }

    /// A successful superseded capture wakes an existing read. Its exception ends when
    /// the next publication lands, so a later ordinary edit still waits for fresh content.
    #[tokio::test]
    async fn a_superseded_capture_wakes_a_read_and_a_publication_restores_its_wait() -> TestResult {
        use std::future::{Future as _, poll_fn};
        use std::task::Poll;

        let (directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        let path = CoreProjectPath::new("lib.rs")?;
        fs::write(directory.path().join("lib.rs"), "pub fn lantern0() {}\n")?;
        server.validation.observe_paths([path.clone()])?;
        let root = directory.path().to_path_buf();
        let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rounds = Arc::clone(&captures);
        server.force_capture(move |current| {
            rounds.fetch_add(1, Ordering::Relaxed);
            captured_tree(&root, current)
        });

        let mut waiting = Box::pin(get_symbol(server, "beacon"));
        poll_fn(|context| {
            assert!(waiting.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            captures.load(Ordering::Relaxed),
            1,
            "the read reached its publication wait"
        );
        supersede_rebuild(&assembled, "pub fn lantern1() {}\n".to_owned()).await?;
        let stale = tokio::time::timeout(UNWAITED_READ_MAX, waiting)
            .await
            .map_err(|_| "a superseded capture must wake the waiting read")??;
        assert_eq!(stale.hits.len(), 1);
        stale_index_of(&stale.warnings)?;

        let outcome = rebuild_workspace(
            &assembled.context,
            server.validation.take_pending(),
            workspace_capture(&server.dependencies),
        )
        .await?;
        assert_eq!(outcome, RebuildOutcome::Published);
        let published_epoch = server.published.read().await.current.epoch;
        assert!(
            server
                .validation
                .superseded_after(published_epoch)
                .is_none()
        );

        fs::write(directory.path().join("lib.rs"), "pub fn lantern2() {}\n")?;
        server.validation.observe_paths([path])?;
        captures.store(0, Ordering::Relaxed);
        let mut waiting = Box::pin(get_symbol(server, "lantern2"));
        poll_fn(|context| {
            assert!(waiting.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            captures.load(Ordering::Relaxed),
            1,
            "the later edit still waits for publication"
        );
        let outcome = rebuild_workspace(
            &assembled.context,
            server.validation.take_pending(),
            workspace_capture(&server.dependencies),
        )
        .await?;
        assert_eq!(outcome, RebuildOutcome::Published);
        let fresh = tokio::time::timeout(UNWAITED_READ_MAX, waiting)
            .await
            .map_err(|_| "the later publication must wake the read")??;
        assert_eq!(fresh.hits.len(), 1, "the new declaration must answer");
        assert!(
            stale_index_of(&fresh.warnings).is_err(),
            "the published edit answers fresh"
        );
        Ok(())
    }

    /// Changed content waits while no successful superseded capture proves movement.
    #[tokio::test]
    async fn a_read_waits_for_a_pending_edit_without_a_superseded_capture() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "[server]\nreadiness_timeout = \"1s\"\n")?;
        let assembled = unsupervised_server(directory.path()).await?;
        let server = &assembled.server;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn lantern() {}\n",
        )?;
        let epoch = observe_without_recording_a_failure(server, "lib.rs", 3)?;
        assert_eq!(epoch, 3);
        assert_eq!(server.published.read().await.current.epoch, 0);
        let error = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(server, "beacon"))
            .await
            .map_err(|_| "a pending edit must resolve inside the readiness bound")?
            .expect_err("a read waits for changed content before any superseded capture");
        assert!(
            error.message.contains("filesystem events behind the tree"),
            "{error:?}"
        );
        Ok(())
    }

    /// A captured configuration change still needs acceptance before a read can use its
    /// policy. Without a publisher, the existing readiness deadline refuses the read.
    #[tokio::test]
    async fn a_read_waits_for_configuration_acceptance_before_answering() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "[server]\nreadiness_timeout = \"1s\"\n")?;
        let assembled = unsupervised_server(directory.path()).await?;
        let server = &assembled.server;
        fs::write(directory.path().join("lib.rs"), "pub fn lantern0() {}\n")?;
        server
            .validation
            .observe_paths([CoreProjectPath::new("lib.rs")?])?;
        supersede_rebuild(&assembled, "pub fn lantern1() {}\n".to_owned()).await?;
        super::hermetic_workspace(directory.path(), "[server]\nreadiness_timeout = \"0ms\"\n")?;

        let error = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(server, "beacon"))
            .await
            .map_err(|_| "configuration acceptance must stay inside the readiness bound")?
            .expect_err("an unpublished configuration must not answer from the old policy");

        assert!(
            error.message.contains("filesystem events behind the tree"),
            "{error:?}"
        );
        let wire = error.data.ok_or("typed refusal")?;
        assert_eq!(wire["code"], json!("temporarily_unavailable"));
        assert_eq!(wire["phase"], json!("read"));
        Ok(())
    }

    /// Configuration movement on the last capture must not use the exhausted read's
    /// stale fallback under the previous policy.
    #[tokio::test]
    async fn a_read_refuses_configuration_movement_on_its_last_capture() -> TestResult {
        let (directory, server) = fixture().await?;
        let root = directory.path().to_path_buf();
        let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rounds = Arc::clone(&captures);
        server.force_capture(move |current| {
            let round = rounds.fetch_add(1, Ordering::Relaxed) + 1;
            fs::write(
                root.join("lib.rs"),
                format!("pub fn lantern{round}() {{}}\n"),
            )
            .expect("the source edit must land");
            if round == super::INDEX_CAPTURE_ATTEMPTS_MAX {
                super::hermetic_workspace(&root, "[server]\nreadiness_timeout = \"0ms\"\n")
                    .expect("the configuration edit must land");
            }
            captured_tree(&root, current)
        });

        let error = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(&server, "beacon"))
            .await
            .map_err(|_| "the last capture must refuse within the read bound")?
            .expect_err("configuration movement must not answer from the old policy");

        assert_eq!(
            captures.load(Ordering::Relaxed),
            super::INDEX_CAPTURE_ATTEMPTS_MAX
        );
        assert!(
            error.message.contains("bounded reconciliation attempts"),
            "{error:?}"
        );
        assert_eq!(error.data.ok_or("typed refusal")?["phase"], json!("read"));
        Ok(())
    }

    /// Observes `path` `events` times and records no rebuild failure, so the observed
    /// epoch runs ahead of the published one with nothing to resolve it.
    fn observe_without_recording_a_failure(
        server: &RiftMcp,
        path: &str,
        events: u64,
    ) -> TestResult<u64> {
        let mut epoch = 0;
        for _event in 0..events {
            epoch = server
                .validation
                .observe_paths([CoreProjectPath::new(path)?])
                .map_err(|error| format!("observation must land: {error:?}"))?;
        }
        Ok(epoch)
    }

    #[test]
    fn a_stale_index_detail_is_cut_at_its_advertised_bound() {
        let long = "x".repeat(super::WARNING_DETAIL_BYTES_MAX + 1);
        assert_eq!(
            super::bounded_detail(long, super::WARNING_DETAIL_BYTES_MAX).len(),
            super::WARNING_DETAIL_BYTES_MAX
        );
        let exact = "y".repeat(super::WARNING_DETAIL_BYTES_MAX);
        assert_eq!(
            super::bounded_detail(exact.clone(), super::WARNING_DETAIL_BYTES_MAX),
            exact
        );
        let multibyte = "\u{e9}".repeat(3);
        assert_eq!(super::bounded_detail(multibyte, 3), "\u{e9}");
    }

    /// How long after a search starts waiting the gated commit is released.
    const COMMIT_LANDING_DELAY: Duration = Duration::from_millis(50);
    /// A commit wait budget a held commit never lands inside.
    const COMMIT_WAIT_BUDGET: Duration = Duration::from_millis(100);

    /// A server over a one-declaration workspace whose lexical lane runs over a
    /// [`StoreDouble`] holding every write at its gate, with the double the test releases
    /// them through. Returns once the first write is held.
    async fn server_over_gated_store(
        root: &std::path::Path,
    ) -> TestResult<(RiftMcp, Arc<StoreDouble>)> {
        fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(root, "")?;
        let double = StoreDouble::new();
        let gate = Arc::clone(&double);
        let assembled = RiftMcp::assemble(
            super::absolute_root(root)?,
            WorkspaceIndexLimits::default(),
            None,
            move |index, blocking, cancellation| {
                gate.attach(index);
                LexicalLane::spawn_over(gate, usize::MAX, blocking, cancellation)
            },
        )
        .await?;
        let server = assembled.supervised().await;
        double.calls_within_bound(1).await?;
        Ok((server, double))
    }

    /// The publication `server` currently answers from.
    async fn current_publication(server: &RiftMcp) -> Arc<PublishedWorkspace> {
        Arc::clone(&server.published.read().await.current)
    }

    #[tokio::test(start_paused = true)]
    async fn a_search_before_the_first_lexical_commit_lands_names_the_revision() -> TestResult {
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);
        let (server, double) = server_over_gated_store(directory.path()).await?;
        let revision = current_publication(&server)
            .await
            .reads
            .tree_revision()
            .to_owned();

        let answer = run_search(&server, "beacon").await?;
        assert!(
            !answer.results.is_empty(),
            "identifier matching answers while the commit is held"
        );
        let detail = answer
            .warnings
            .iter()
            .find_map(|warning| match warning {
                ReadWarning::LexicalRankingUnavailable { detail } => Some(detail.clone()),
                _ => None,
            })
            .ok_or("the search warns once its budget ran out with the transaction still held")?;
        assert!(
            detail.contains(&format!("still committing tree revision {revision}")),
            "{detail}"
        );

        tokio::time::sleep(LEXICAL_COMMIT_TIMEOUT + Duration::from_millis(1)).await;
        let mut records = Vec::new();
        while let Ok(record) = drain.try_recv_record() {
            records.push(record);
        }
        let delayed = records
            .iter()
            .find(|record| record.message().contains("ran past its deadline"))
            .ok_or("a transaction past its deadline is recorded")?;
        assert_eq!(delayed.level(), "error");
        let again = run_search(&server, "beacon").await?;
        assert!(
            !again.results.is_empty() && !store_ranked(&again),
            "the answer keeps coming from identifier matching past the deadline: {:?}",
            again.warnings
        );

        double.release_one();
        let ranked = search_after_population(&server, "beacon").await?;
        assert!(
            ranked.warnings.is_empty(),
            "a landed commit clears the warning: {:?}",
            ranked.warnings
        );
        Ok(())
    }

    /// A commit that lands inside the budget is waited out: the answer is store-ranked,
    /// and the operator-action warning is never spent.
    #[tokio::test(start_paused = true)]
    async fn a_search_waits_out_a_commit_that_lands_inside_its_budget() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (server, double) = server_over_gated_store(directory.path()).await?;
        let release = tokio::spawn({
            let double = Arc::clone(&double);
            async move {
                tokio::time::sleep(COMMIT_LANDING_DELAY).await;
                double.release_one();
            }
        });
        let started = tokio::time::Instant::now();
        let answer = run_search(&server, "beacon").await?;
        let waited = started.elapsed();
        release.await?;
        assert!(
            store_ranked(&answer) && !answer.results.is_empty(),
            "the landed commit ranks the answer: {:?}",
            answer.warnings
        );
        assert!(
            waited >= COMMIT_LANDING_DELAY && waited < Duration::from_secs(1),
            "the search waited for the landing and no longer: {waited:?}"
        );
        Ok(())
    }

    /// A commit that never lands inside the budget leaves the answer ranked by identifier
    /// matching, warning that the tree is still committing, once the budget ran out.
    #[tokio::test(start_paused = true)]
    async fn a_search_whose_commit_never_lands_inside_its_budget_warns() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (server, _double) = server_over_gated_store(directory.path()).await?;
        let published = current_publication(&server).await;
        let params: SearchParams = serde_json::from_value(json!({"query": "beacon"}))?;
        let started = tokio::time::Instant::now();
        let ranking = server
            .ranking(&params, &published, COMMIT_WAIT_BUDGET)
            .await?
            .ok_or("a tree still committing ranks by identifier matching alone")?;
        let waited = started.elapsed();
        let detail =
            unavailable_detail(&ranking).ok_or("the tier warns once the budget ran out")?;
        assert!(
            detail.contains(&format!(
                "still committing tree revision {}",
                published.reads.tree_revision()
            )),
            "{detail}"
        );
        assert!(
            waited >= COMMIT_WAIT_BUDGET && waited < COMMIT_WAIT_BUDGET * 2,
            "the wait spent its budget and no more: {waited:?}"
        );
        Ok(())
    }

    /// A settled store never waits: the answer comes back at once, far under the budget.
    #[tokio::test(start_paused = true)]
    async fn a_search_over_a_settled_store_never_waits() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (server, double) = server_over_gated_store(directory.path()).await?;
        double.release_one();
        search_after_population(&server, "beacon").await?;
        let published = current_publication(&server).await;
        let params: SearchParams = serde_json::from_value(json!({"query": "beacon"}))?;
        let budget = server.readiness_timeout().await;
        let started = tokio::time::Instant::now();
        let ranking = server
            .ranking(&params, &published, budget)
            .await?
            .ok_or("a settled store ranks")?;
        let waited = started.elapsed();
        assert!(
            ranking.warnings.is_empty(),
            "a settled store warns nothing: {:?}",
            ranking.warnings
        );
        assert!(
            waited < budget / 100,
            "a settled store answers without waiting: {waited:?} under {budget:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_read_answers_matching_content_while_publication_is_stalled() -> TestResult {
        let (_directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        // An observation without changed bytes needs no new publication for a read.
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answer = tokio::time::timeout(UNWAITED_READ_MAX, get_symbol(server, "beacon"))
            .await
            .map_err(|_| "matching content must answer without a publication")??;
        assert_eq!(answer.hits.len(), 1, "the publication answers");
        assert!(
            stale_index_of(&answer.warnings).is_err(),
            "matching content must not warn stale: {:?}",
            answer.warnings
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_stall_after_the_epoch_settled_names_unfinished_validation() -> TestResult {
        let (_directory, server) = fixture().await?;

        let detail = server.readiness_stall(Duration::from_millis(25)).await;

        assert!(detail.contains("the index settled at epoch 0"), "{detail}");
        assert!(
            detail.contains("workspace validation did not finish"),
            "{detail}"
        );
        assert!(detail.contains("capture and rebuild records"), "{detail}");
        assert!(detail.contains("25ms"), "{detail}");
        Ok(())
    }

    #[tokio::test]
    async fn a_read_refuses_at_once_when_the_supervisor_has_stopped() -> TestResult {
        let (_directory, server) = fixture().await?;
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        server
            .validation
            .supervisor_running
            .store(false, std::sync::atomic::Ordering::Release);

        // The readiness budget is thirty seconds; a real-clock test that reached it would
        // take that long, so refusing without waiting is what this asserts.
        let refused_at = std::time::Instant::now();
        let error = get_symbol(&server, "beacon")
            .await
            .expect_err("a stopped supervisor must refuse the read");

        assert!(
            refused_at.elapsed() < Duration::from_secs(5),
            "the refusal must not wait out the readiness budget: {:?}",
            refused_at.elapsed()
        );
        assert!(
            error.message.contains("the index supervisor stopped"),
            "{error:?}"
        );
        assert!(
            error.message.contains("restart the workspace server"),
            "{error:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn workspace_resource_reads_effective_languages_and_source() -> TestResult {
        let (_directory, server) = fixture().await?;
        let answer = server.read_workspace("rift://workspace").await?;
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = answer
            .contents
            .first()
            .expect("a workspace read answers with one content")
        else {
            unreachable!("a workspace read answers with text");
        };
        let body: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(
            body["configuration_revision"].as_str().map(str::len),
            Some(8),
            "{text}"
        );
        assert!(
            body["languages"].as_array().is_some_and(|languages| {
                languages.iter().any(|language| {
                    language["language"] == serde_json::json!("rust")
                        && language["enabled"] == serde_json::json!(true)
                        && language["syntax"] == serde_json::json!(true)
                })
            }),
            "{text}"
        );
        assert!(
            body["source"].as_array().is_some_and(|source| {
                source.iter().any(|unit| {
                    unit["path"] == serde_json::json!("lib.rs")
                        && unit["language"] == serde_json::json!("rust")
                })
            }),
            "{text}"
        );
        assert_eq!(
            body["pagination"],
            serde_json::json!({ "page_index": 0, "total_pages": 1 })
        );
        Ok(())
    }

    #[tokio::test]
    async fn workspace_resource_includes_an_unclassified_visible_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("Cargo.lock"), "version = 4\n")?;
        super::hermetic_workspace(directory.path(), "[languages.rust]\nenabled = false\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let answer = server.read_workspace("rift://workspace").await?;
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = answer
            .contents
            .first()
            .expect("a workspace read answers with one content")
        else {
            unreachable!("a workspace read answers with text");
        };
        let body: serde_json::Value = serde_json::from_str(text)?;
        let lock = body["source"]
            .as_array()
            .and_then(|source| {
                source
                    .iter()
                    .find(|unit| unit["path"] == serde_json::json!("Cargo.lock"))
            })
            .expect("visible Cargo.lock must join the source catalog");
        assert!(
            lock.get("language").is_none(),
            "an unclassified file must omit language: {lock}"
        );
        let rust = body["source"]
            .as_array()
            .and_then(|source| {
                source
                    .iter()
                    .find(|unit| unit["path"] == serde_json::json!("lib.rs"))
            })
            .expect("disabled Rust source must remain in the source catalog");
        assert_eq!(rust["language"], serde_json::json!("rust"));
        Ok(())
    }

    #[tokio::test]
    async fn the_log_resource_answers_while_workspace_reads_refuse() -> TestResult {
        let (_directory, server) = fixture().await?;
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        server
            .validation
            .supervisor_running
            .store(false, std::sync::atomic::Ordering::Release);
        get_symbol(&server, "beacon")
            .await
            .expect_err("the stalled workspace must refuse a read");

        let answer = server.read_logs("rift://logs").await?;

        // The read is the whole point of the resource: the request that just refused is
        // the one whose reason lives in these records.
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = answer
            .contents
            .first()
            .expect("a log read answers with one content")
        else {
            unreachable!("a log read answers with text");
        };
        let body: serde_json::Value = serde_json::from_str(text)?;
        assert!(body["records"].is_array(), "{text}");
        Ok(())
    }

    /// Parenthesis nesting past the shipped syntax depth bound of 512.
    const DEEP_NESTING: usize = 600;

    /// A Rust source whose syntax tree runs deeper than the provider accepts.
    fn deep_source() -> String {
        format!(
            "pub fn deep() -> i32 {{ {open}1{close} }}\n",
            open = "(".repeat(DEEP_NESTING),
            close = ")".repeat(DEEP_NESTING),
        )
    }

    /// The project paths the hits on one search page name.
    fn hit_paths(answer: &serde_json::Value) -> Vec<&str> {
        answer["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hit| hit["path"].as_str())
            .collect()
    }

    /// The build runs on the blocking pool, so only a process-wide subscriber sees its
    /// records; the sink captures under the workspace's own `[logs] capture` default, the
    /// filter a served workspace records under.
    #[tokio::test]
    async fn a_file_past_a_syntax_bound_is_named_in_the_logs_and_the_rest_serves() -> TestResult {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("src/deep.rs"), deep_source())?;
        super::hermetic_workspace(directory.path(), "")?;

        let (sink, drain) = crate::logs::log_capture();
        let capture = crate::logs::logs_configuration(directory.path()).capture;
        let filter = tracing_subscriber::EnvFilter::try_new(&capture)?;
        let subscriber = tracing_subscriber::registry().with(sink.with_filter(filter));
        tracing::subscriber::set_global_default(subscriber)?;

        let storage = crate::storage::WorkspaceStorage::open(directory.path()).await;
        let store = storage.logs().ok_or("the log store must open")?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let drain_task = tokio::spawn(drain.run(store, 10_000, cancellation.clone()));
        let server =
            RiftMcp::build_with_storage(directory.path(), WorkspaceIndexLimits::default(), storage)
                .await?;

        let kept = serde_json::to_value(run_search(&server, "beacon").await?)?;
        assert!(hit_paths(&kept).contains(&"src/lib.rs"), "{kept:#}");
        let absent = serde_json::to_value(run_search(&server, "deep").await?)?;
        assert!(
            !hit_paths(&absent).contains(&"src/deep.rs"),
            "the deep file answers no search: {absent:#}"
        );

        cancellation.cancel();
        drain_task.await?;
        let logs = server.read_logs("rift://logs").await?;
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = logs
            .contents
            .first()
            .ok_or("a log read answers with one content")?
        else {
            return Err("a log read answers with text".into());
        };
        assert!(
            text.contains("file left out of the index")
                && text.contains("src/deep.rs")
                && text.contains("too_deep"),
            "the logs must name the left-out file and its bound: {text}"
        );
        Ok(())
    }

    /// A record emitted immediately before a `rift://logs` read appears in that read, with the
    /// drain still running. The drain writes on its own timer and nothing made the read wait
    /// for it, so a caller reading back the diagnostic behind its own refusal was answered
    /// without it: cold first use met exactly this on `rift://logs/component/engine`.
    ///
    /// The sink captures under the workspace's own `[logs] capture` default, the filter a
    /// served workspace records under. Without it the lane also takes the storage driver's own
    /// trace records, and the read then waits out its bound behind thousands of them.
    #[tokio::test]
    async fn a_record_emitted_before_a_read_appears_in_that_read() -> TestResult {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;

        let (sink, drain) = crate::logs::log_capture();
        let capture = crate::logs::logs_configuration(directory.path()).capture;
        let filter = tracing_subscriber::EnvFilter::try_new(&capture)?;
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(sink.with_filter(filter)),
        );
        let storage = crate::storage::WorkspaceStorage::open(directory.path()).await;
        let store = storage.logs().ok_or("the log store must open")?;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let drain_task = tokio::spawn(drain.run(store, 10_000, cancellation.clone()));
        let server =
            RiftMcp::build_with_storage(directory.path(), WorkspaceIndexLimits::default(), storage)
                .await?;

        tracing::warn!(component = "engine", "the beacon engine did not start");
        let logs = server.read_logs("rift://logs/component/engine").await?;
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } = logs
            .contents
            .first()
            .ok_or("a log read answers with one content")?
        else {
            return Err("a log read answers with text".into());
        };
        let answered = text.clone();

        cancellation.cancel();
        drain_task.await?;
        assert!(
            answered.contains("the beacon engine did not start"),
            "the read answers with the record the same task emitted: {answered}"
        );
        Ok(())
    }

    /// One declaration's content past the lexical unit bound leaves the lexical index
    /// alone: the publication lands, the sibling answers `search`, the declaration still
    /// answers `get_symbol`, and the record names the file. The lexical commit runs on
    /// the building task, so the thread-local subscriber sees it.
    #[tokio::test]
    async fn an_oversized_declaration_is_left_out_of_search_and_the_rest_serves() -> TestResult {
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        let unit_bytes_max =
            usize::try_from(rift_index::LexicalIndexLimits::default().unit_bytes_max())?;
        let blob = format!(
            "pub const BLOB: &str = \"{}\";\n",
            "b".repeat(unit_bytes_max)
        );
        fs::write(directory.path().join("src/blob.rs"), blob)?;
        super::hermetic_workspace(directory.path(), "")?;
        let (sink, mut drain) = crate::logs::log_capture();
        let _guard = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));

        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let kept = serde_json::to_value(run_search(&server, "beacon").await?)?;
        assert!(hit_paths(&kept).contains(&"src/lib.rs"), "{kept:#}");
        let symbol = get_symbol(&server, "BLOB").await?;
        assert_eq!(
            symbol.hits.len(),
            1,
            "the declaration stays in the syntax index: {symbol:?}"
        );
        let recorded = loop {
            match drain.try_recv_record() {
                Ok(record) if record.message().contains("lexical unit left out") => break record,
                Ok(_) => {}
                Err(_) => return Err("the left-out unit must be recorded".into()),
            }
        };
        assert_eq!(recorded.level(), "warn");
        assert_eq!(recorded.component(), "index");
        assert!(
            recorded.fields().contains("src/blob.rs"),
            "the record names the path: {}",
            recorded.fields()
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_cancelled_supervisor_reports_that_it_stopped() -> TestResult {
        let (_directory, server) = fixture().await?;
        assert!(
            server
                .validation
                .supervisor_running
                .load(std::sync::atomic::Ordering::Acquire),
            "a built server runs its supervisor"
        );

        let stopped = server.validation.changed.notified();
        tokio::pin!(stopped);
        stopped.as_mut().enable();
        server.validation.cancellation.cancel();
        let handle = server.validation.task.lock().await.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        stopped.as_mut().await;

        assert!(
            !server
                .validation
                .supervisor_running
                .load(std::sync::atomic::Ordering::Acquire),
            "a supervisor that ended must say so"
        );
        Ok(())
    }

    #[tokio::test]
    async fn traced_read_reconciles_under_an_active_subscriber() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let (_directory, server) = fixture().await?;
        let result = get_symbol(&server, "beacon")
            .await
            .map_err(|error| format!("traced read must serve: {error:?}"))?;
        assert_eq!(result.hits.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn build_disables_search_index_when_rift_state_path_is_a_file() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // A regular file already occupies `.rift`, so `create_dir_all` cannot make the
        // state directory the lexical database needs.
        super::hermetic_workspace(directory.path(), "")?;
        fs::write(directory.path().join(".rift"), b"not a directory")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(
            server.search_index.is_none(),
            "a blocked state directory must degrade to no search index, not fail startup"
        );
        Ok(())
    }

    #[tokio::test]
    async fn build_disables_search_index_when_database_path_is_a_directory() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // A directory at the database path makes SQLite reject the open without changing
        // the unexpected filesystem entry.
        super::hermetic_workspace(directory.path(), "")?;
        fs::create_dir_all(directory.path().join(".rift/db"))?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(
            server.search_index.is_none(),
            "a database path occupied by a directory must leave the server running without \
             the search index"
        );

        // With no search index, identifier search still serves results rather than failing.
        let result = run_search(&server, "beacon").await?;
        assert!(
            !result.results.is_empty(),
            "identifier search must still serve results without the search index"
        );
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::LexicalRankingUnavailable { .. })),
            "an absent index must say so on the answer: {result:#?}"
        );
        Ok(())
    }

    fn shipped_search_configuration() -> SearchConfiguration {
        SearchConfiguration::default()
    }

    #[test]
    fn search_index_limits_carry_every_accepted_search_key() -> TestResult {
        let search = shipped_search_configuration();
        let root = std::path::Path::new("/workspace");
        let acquisition = super::model_acquisition(&search.semantic, root)
            .ok_or("the shipped table must resolve an acquisition")?;
        let limits = super::search_index_limits(&search, Some(&acquisition));
        assert!(!limits.is_semantic_disabled());
        assert_eq!(limits.lexical(), super::lexical_index_limits(&search));
        assert_eq!(limits.fusion_k(), search.fusion_k);
        assert_eq!(limits.candidates(), search.semantic.candidates);
        assert_eq!(limits.max_vectors(), search.semantic.max_vectors);
        assert_eq!(
            limits.batch_declarations(),
            search.semantic.batch_declarations
        );
        assert_eq!(limits.max_tokens(), search.semantic.max_tokens);
        assert_eq!(
            limits.per_file_max(),
            3,
            "no key sets the per-file candidate bound yet"
        );
        assert!((limits.lexical_weight() - search.lexical.weight).abs() < f64::EPSILON);
        assert!((limits.semantic_weight() - search.semantic.weight).abs() < f64::EPSILON);
        assert_eq!(acquisition.limits.attempts(), 3);
        assert_eq!(
            acquisition.limits.timeout(),
            Duration::from_millis(search.semantic.download_timeout.milliseconds())
        );
        Ok(())
    }

    #[test]
    fn a_disabled_semantic_tier_resolves_no_acquisition_and_disables_the_index() {
        let mut search = shipped_search_configuration();
        search.semantic.disabled = true;
        let root = std::path::Path::new("/workspace");
        assert!(super::model_acquisition(&search.semantic, root).is_none());
        let limits = super::search_index_limits(&search, None);
        assert!(
            limits.is_semantic_disabled(),
            "an unresolved acquisition must disable the tier rather than leave it preparing"
        );
    }

    #[test]
    fn each_semantic_source_reads_its_own_model_form() -> TestResult {
        let root = std::path::Path::new("/workspace");
        let hub = SemanticSearchConfiguration {
            model: "BAAI/bge-small-en-v1.5@dd0a482".to_owned(),
            ..SemanticSearchConfiguration::default()
        };
        let acquired =
            super::model_acquisition(&hub, root).ok_or("a hub repository must resolve")?;
        assert_eq!(
            acquired.source,
            ModelSource::Repository {
                repository: "BAAI/bge-small-en-v1.5".to_owned(),
                revision: "dd0a482".to_owned(),
            }
        );
        let held = SemanticSearchConfiguration {
            source: SemanticSource::Directory,
            model: "models/bge".to_owned(),
            ..SemanticSearchConfiguration::default()
        };
        let acquired =
            super::model_acquisition(&held, root).ok_or("a held directory must resolve")?;
        assert_eq!(
            acquired.source,
            ModelSource::Directory(root.join("models/bge")),
            "a directory model resolves against the workspace root"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_model_the_source_refuses_leaves_the_tier_off_and_full_text_serving() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        // Acceptance's path rule allows an empty segment; `ModelSource` refuses one, so this
        // value passes the first gate and fails the second.
        let refused = SemanticSearchConfiguration {
            source: SemanticSource::Directory,
            model: "models//bge".to_owned(),
            ..SemanticSearchConfiguration::default()
        };
        assert!(
            super::semantic_model_source(&refused, std::path::Path::new("/workspace")).is_err(),
            "the model value must be one ModelSource refuses"
        );
        assert!(super::model_acquisition(&refused, std::path::Path::new("/workspace")).is_none());

        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let rift_toml = directory.path().join("rift.toml");
        let table = "[search.semantic]\nsource = \"directory\"\nmodel = \"models//bge\"\n";
        fs::write(rift_toml, table)?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let index = server
            .search_index
            .clone()
            .ok_or("a refused model must not stop the search index from opening")?;
        assert_eq!(
            index.readiness(),
            SemanticReadiness::Disabled,
            "a refused model disables the tier rather than leaving it preparing"
        );
        // The run's first pass lands after `build` returns, and until it does the answer
        // carries the revision guard's own warning. Wait for the pass, then prove the
        // disabled tier adds nothing of its own on top.
        let result = search_after_population(&server, "beacon").await?;
        assert!(
            !result.results.is_empty(),
            "the full-text tier must still serve: {result:#?}"
        );
        assert_eq!(
            result.warnings,
            Vec::new(),
            "a disabled tier adds no warning: {result:#?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_invalid_configuration_holds_the_acquisition_back() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // The table naming the model is the very part acceptance could not read.
        let rift_toml = directory.path().join("rift.toml");
        fs::write(rift_toml, "[search.semantic]\nnot_a_key = 1\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let index = server
            .search_index
            .clone()
            .ok_or("an invalid configuration must not stop the search index from opening")?;
        assert_eq!(
            index.readiness(),
            SemanticReadiness::Disabled,
            "a server that answers nothing must not spend a download first"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_model_directory_without_weights_ends_preparation_and_the_answer_says_so()
    -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // An empty directory holds none of the three files an encoder loads, so acquisition
        // refuses without reaching a network.
        fs::create_dir_all(directory.path().join("weights"))?;
        let rift_toml = directory.path().join("rift.toml");
        let table = "[search.semantic]\nsource = \"directory\"\nmodel = \"weights\"\n";
        fs::write(rift_toml, table)?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        // Preparation runs behind startup, so poll for its verdict under a bound rather
        // than racing the task that carries it.
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            let result = run_search(&server, "beacon").await?;
            let refused = result
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::SemanticRankingUnavailable { .. }));
            if refused {
                assert!(
                    !result.results.is_empty(),
                    "the full-text tier must keep serving: {result:#?}"
                );
                return Ok(());
            }
            tokio::time::sleep(SEARCH_TIER_POLL).await;
        }
        Err("a model directory without weights never ended preparation".into())
    }

    /// The one `lexical_ranking_unavailable` detail a ranking carries, or nothing when it
    /// carries none.
    fn unavailable_detail(ranking: &super::SearchRanking) -> Option<&str> {
        ranking.warnings.iter().find_map(|warning| match warning {
            ReadWarning::LexicalRankingUnavailable { detail } => Some(detail.as_str()),
            _ => None,
        })
    }

    #[test]
    fn a_settled_store_holding_another_tree_asks_the_request_to_recapture() {
        assert!(
            super::ranking_of(
                RevisionScoped::OtherRevision("aaaaaaaa".to_owned()),
                SemanticReadiness::Ready,
                10,
                "bbbbbbbb",
                LexicalCommitState::Settled,
            )
            .is_none(),
            "rows from another tree are never merged into this answer, and no warning \
             stands in for the recapture"
        );
    }

    #[test]
    fn a_settled_store_holding_no_tree_warns_that_it_will_not_answer() -> TestResult {
        let ranking = super::ranking_of(
            RevisionScoped::NoRevision,
            SemanticReadiness::Ready,
            10,
            "bbbbbbbb",
            LexicalCommitState::Settled,
        )
        .ok_or("a store holding no tree still ranks, by identifier matching alone")?;
        assert!(ranking.units.is_empty());
        let detail = unavailable_detail(&ranking).ok_or("the tier warns")?;
        assert!(detail.contains("holds no indexed tree"), "{detail}");
        assert!(detail.contains("bbbbbbbb"), "{detail}");
        assert_eq!(ranking.warnings.len(), 1, "exactly one warning is raised");
        Ok(())
    }

    #[test]
    fn a_store_the_lane_is_still_committing_into_warns_naming_the_revision() -> TestResult {
        for searched in [
            RevisionScoped::NoRevision,
            RevisionScoped::OtherRevision("aaaaaaaa".to_owned()),
        ] {
            let ranking = super::ranking_of(
                searched,
                SemanticReadiness::Ready,
                10,
                "bbbbbbbb",
                LexicalCommitState::Committing,
            )
            .ok_or("a tree still committing ranks by identifier matching alone")?;
            assert!(ranking.units.is_empty());
            let detail = unavailable_detail(&ranking).ok_or("the tier warns")?;
            assert!(
                detail.contains("still committing tree revision bbbbbbbb"),
                "{detail}"
            );
            assert_eq!(ranking.warnings.len(), 1);
        }
        Ok(())
    }

    #[test]
    fn a_store_owed_a_whole_replace_warns_that_it_missed_a_commit() -> TestResult {
        let ranking = super::ranking_of(
            RevisionScoped::OtherRevision("aaaaaaaa".to_owned()),
            SemanticReadiness::Ready,
            10,
            "bbbbbbbb",
            LexicalCommitState::Owed {
                cause: "field units_max, observed 1101, maximum 1000".to_owned(),
            },
        )
        .ok_or("a store that missed a commit ranks by identifier matching alone")?;
        let detail = unavailable_detail(&ranking).ok_or("the tier warns")?;
        assert!(detail.contains("missed a commit"), "{detail}");
        assert!(detail.contains("tree revision bbbbbbbb"), "{detail}");
        assert!(
            detail.ends_with("field units_max, observed 1101, maximum 1000"),
            "the warning carries the refusal the store rendered: {detail}"
        );
        Ok(())
    }

    #[test]
    fn a_matched_store_ranks_its_units_and_carries_the_readiness_warning() -> TestResult {
        let ranking = super::ranking_of(
            RevisionScoped::Matched(FusedRanking::new(Vec::new(), None)),
            SemanticReadiness::Preparing {
                prepared: 1,
                total: 4,
            },
            10,
            "bbbbbbbb",
            LexicalCommitState::Committing,
        )
        .ok_or("a matched store ranks")?;
        assert!(ranking.units.is_empty());
        let warnings = serde_json::to_value(&ranking.warnings)?;
        assert_eq!(warnings[0]["code"], json!("semantic_index_preparing"));
        assert_eq!(
            ranking.warnings.len(),
            1,
            "a matched store's rows answer whatever the lane still holds"
        );
        Ok(())
    }

    #[test]
    fn a_matched_store_cut_at_its_bound_carries_the_truncation_warning() -> TestResult {
        let ranking = super::ranking_of(
            RevisionScoped::Matched(FusedRanking::new(Vec::new(), Some(1_000))),
            SemanticReadiness::Ready,
            10,
            "bbbbbbbb",
            LexicalCommitState::Settled,
        )
        .ok_or("a matched store ranks")?;
        let warnings = serde_json::to_value(&ranking.warnings)?;
        assert_eq!(warnings[0]["code"], json!("lexical_ranking_truncated"));
        assert_eq!(warnings[0]["matches_max"], json!(1_000));
        assert_eq!(warnings[1], json!(null), "exactly one warning is raised");
        Ok(())
    }

    #[tokio::test]
    async fn a_search_over_a_tree_the_store_never_held_asks_for_a_recapture() -> TestResult {
        let (_directory, server) = fixture().await?;
        // The store holds the served workspace's tree once the first pass has landed.
        search_after_population(&server, "beacon").await?;
        // A snapshot of an unrelated workspace stands in for a superseded publication: its
        // tree revision is one the server's store was never populated for. Nothing writes
        // into the served workspace, so no rebuild can close the window mid-test.
        let other = tempfile::tempdir()?;
        fs::write(other.path().join("lib.rs"), "pub fn lantern() {}\n")?;
        let moved = stable_candidate(other.path(), 0)?;
        let params: SearchParams = serde_json::from_value(json!({"query": "lantern"}))?;
        let budget = server.readiness_timeout().await;
        assert!(
            server.ranking(&params, &moved, budget).await?.is_none(),
            "a store that never held this tree ends the attempt rather than ranking"
        );
        Ok(())
    }

    #[test]
    fn an_absent_store_warns_lexical_ranking_unavailable() -> TestResult {
        let ranking = super::SearchRanking::unavailable("the database could not be opened");
        let warnings = serde_json::to_value(&ranking.warnings)?;
        assert_eq!(warnings[0]["code"], json!("lexical_ranking_unavailable"));
        assert_eq!(
            warnings[0]["detail"],
            json!("the database could not be opened")
        );
        Ok(())
    }

    #[test]
    fn each_readiness_state_produces_its_own_warning() {
        assert_eq!(
            super::readiness_warnings(SemanticReadiness::Ready, 10),
            Vec::new()
        );
        assert_eq!(
            super::readiness_warnings(SemanticReadiness::Disabled, 10),
            Vec::new()
        );
        let unavailable = super::readiness_warnings(SemanticReadiness::Unavailable, 10);
        assert!(matches!(
            unavailable.as_slice(),
            [ReadWarning::SemanticRankingUnavailable { .. }]
        ));
        let readiness = SemanticReadiness::Preparing {
            prepared: 1,
            total: 4,
        };
        let expected = ReadWarning::SemanticIndexPreparing {
            prepared: 1,
            total: 4,
            // Three of four declarations left is three quarters of a small workspace's span.
            ready_in: WireDuration::from_millis(2_250),
            detail: "1 of 4 declarations carry a vector, so the answer was ranked lexically \
                     alone; resend the request once the semantic tier has caught up"
                .to_owned(),
        };
        assert_eq!(super::readiness_warnings(readiness, 10), vec![expected]);
    }

    #[test]
    fn the_preparation_span_steps_exactly_at_each_declared_file_count() {
        assert_eq!(super::preparation_span(0), WireDuration::from_millis(3_000));
        assert_eq!(
            super::preparation_span(1_000),
            WireDuration::from_millis(3_000)
        );
        assert_eq!(
            super::preparation_span(1_001),
            WireDuration::from_millis(10_000)
        );
        assert_eq!(
            super::preparation_span(5_000),
            WireDuration::from_millis(10_000)
        );
        assert_eq!(
            super::preparation_span(5_001),
            WireDuration::from_millis(60_000)
        );
        assert_eq!(
            super::preparation_span(10_000),
            WireDuration::from_millis(60_000)
        );
        assert_eq!(
            super::preparation_span(10_001),
            WireDuration::from_millis(120_000)
        );
    }

    #[test]
    fn ready_in_scales_the_declared_span_by_the_work_left() {
        let files = 20_000;
        assert_eq!(
            super::ready_in(files, 0, 100),
            WireDuration::from_millis(120_000),
            "nothing prepared waits the whole span"
        );
        assert_eq!(
            super::ready_in(files, 50, 100),
            WireDuration::from_millis(60_000),
            "half prepared waits half the span"
        );
        assert_eq!(
            super::ready_in(files, 99, 100),
            WireDuration::from_millis(1_200),
            "one declaration left waits a hundredth of the span"
        );
        assert_eq!(
            super::ready_in(files, 0, 0),
            WireDuration::from_millis(120_000),
            "a set of no declarations divides by nothing and answers the whole span"
        );
        assert_eq!(
            super::ready_in(files, 5, 1),
            WireDuration::from_millis(0),
            "more prepared than the set holds is no work left, never a negative wait"
        );
        assert_eq!(
            super::ready_in(files, 0, u64::MAX),
            WireDuration::from_millis(120_000),
            "the widest set still divides exactly, because the product runs in u128"
        );
    }

    #[tokio::test]
    async fn a_revision_search_never_consults_the_search_index() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce beacon");
        super::hermetic_workspace(directory.path(), "")?;
        // A directory at the database path exhausts the open retry, so the handle is absent
        // and a current-tree search says so. A revision search must stay silent about it.
        fs::create_dir_all(directory.path().join(".rift/db"))?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(server.search_index.is_none());
        let current = run_search(&server, "beacon").await?;
        assert!(
            current
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::LexicalRankingUnavailable { .. })),
            "a current-tree search must report the absent index: {current:#?}"
        );
        let params: SearchParams =
            serde_json::from_value(json!({"query": "beacon", "rev": "main"}))?;
        let answer = server.search(Parameters(params)).await?.0;
        assert!(
            !answer.results.is_empty(),
            "a revision search must still answer: {answer:#?}"
        );
        assert_eq!(
            answer.warnings,
            Vec::new(),
            "a revision search passes no ranked units and consults no index: {answer:#?}"
        );
        Ok(())
    }

    /// Polls one search-tier pass a test waits on before it gives up: three seconds, at
    /// [`SEARCH_TIER_POLL`] each.
    const SEARCH_TIER_ATTEMPTS_MAX: usize = 60;
    /// Wait between two reads of a search tier still catching up.
    const SEARCH_TIER_POLL: Duration = Duration::from_millis(50);

    /// Whether the search index itself ranked this answer, rather than the revision guard
    /// degrading it to identifier matching while a pass is still pending.
    fn store_ranked(answer: &SearchResult) -> bool {
        !answer.warnings.iter().any(|warning| {
            matches!(
                warning,
                ReadWarning::LexicalRankingUnavailable { .. } | ReadWarning::StaleIndex { .. }
            )
        })
    }

    /// Polls one in-process search until the population lane's pass has landed, and answers
    /// with the first answer the store itself ranked.
    ///
    /// The lane runs every pass off the request path, so neither [`RiftMcp::build`] nor a
    /// landed change hands back an already-populated store. Until the pass lands the
    /// revision guard ranks by identifier matching alone and says so, which is exactly the
    /// condition polled here.
    ///
    /// # Errors
    ///
    /// Returns the warnings the last answer still carried once the bound runs out.
    async fn search_after_population(server: &RiftMcp, query: &str) -> TestResult<SearchResult> {
        let mut answer = run_search(server, query).await?;
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            if store_ranked(&answer) {
                return Ok(answer);
            }
            tokio::time::sleep(SEARCH_TIER_POLL).await;
            answer = run_search(server, query).await?;
        }
        Err(format!(
            "the population lane never stamped the served tree for query {query}; the last \
             answer still carried {:?}",
            answer.warnings
        )
        .into())
    }

    /// Polls `search` at `limit` until the population lane's pass has landed, the same
    /// condition [`search_after_population`] polls for a default-limit request.
    async fn search_at_limit_after_population(
        server: &RiftMcp,
        query: &str,
        limit: u64,
    ) -> TestResult<SearchResult> {
        let params: SearchParams = serde_json::from_value(json!({"query": query, "limit": limit}))
            .expect("test search parameters must deserialize");
        let mut answer = server.search(Parameters(params.clone())).await?.0;
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            if store_ranked(&answer) {
                return Ok(answer);
            }
            tokio::time::sleep(SEARCH_TIER_POLL).await;
            answer = server.search(Parameters(params.clone())).await?.0;
        }
        Err(format!(
            "the population lane never stamped the served tree for query {query} at limit \
             {limit}; the last answer still carried {:?}",
            answer.warnings
        )
        .into())
    }

    /// The lexical store ranks at most `matches_max` units for one request. A workspace
    /// whose matches exceed that bound carries the cut as a warning naming the bound; the
    /// same workspace answers a query under the bound with no such warning.
    #[tokio::test]
    async fn search_past_the_lexical_bound_carries_the_truncation_warning() -> TestResult {
        let directory = tempfile::tempdir()?;
        let matches_max = rift_index::LexicalIndexLimits::default().matches_max();
        for index in 0..=matches_max {
            let beacon = directory.path().join(format!("beacon_{index}.rs"));
            fs::write(beacon, "pub fn beacon() {}\n")?;
        }
        fs::write(directory.path().join("lantern.rs"), "pub fn lantern() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let cut = search_after_population(&server, "beacon").await?;
        assert!(
            cut.warnings
                .contains(&ReadWarning::LexicalRankingTruncated {
                    matches_max: u64::from(matches_max),
                }),
            "one match past the bound must carry the cut: {:?}",
            cut.warnings
        );

        let whole = search_after_population(&server, "lantern").await?;
        assert!(
            !whole
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::LexicalRankingTruncated { .. })),
            "a ranking under the bound is not cut: {:?}",
            whole.warnings
        );
        Ok(())
    }

    /// `fetch_limit` no longer scales with the requested `limit`, so `total_pages` reflects
    /// the same candidate pool whatever page size the caller asks for: a `limit: 1` request
    /// reports as many pages as the pool a `limit` wide enough to fit it all serves on one
    /// page.
    #[tokio::test]
    async fn search_total_pages_reflects_the_pool_whatever_the_requested_limit() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn helper_one() {}\npub fn helper_two() {}\npub fn helper_three() {}\n\
             pub fn helper_four() {}\npub fn helper_five() {}\n",
        )?;
        super::hermetic_workspace(directory.path(), "")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        let wide = search_at_limit_after_population(&server, "helper", 50).await?;
        let pool_size = wide.results.len();
        assert!(
            pool_size >= 5,
            "the fixture must rank every declared helper: {wide:#?}"
        );

        let narrow = search_at_limit_after_population(&server, "helper", 1).await?;
        assert_eq!(
            usize::try_from(narrow.pagination.total_pages).unwrap_or(usize::MAX),
            pool_size,
            "a limit of 1 must report as many pages as the pool a limit of 50 served on one \
             page: narrow={narrow:#?} wide={wide:#?}"
        );
        Ok(())
    }

    /// Calls `search` through the wire until one hit carries `path`.
    ///
    /// The population lane runs the pass off the request path, so a text file is searchable
    /// only once the pass its publication asked for has landed; identifier matching alone
    /// never reaches a non-source file.
    ///
    /// # Errors
    ///
    /// Returns the last answer that still missed `path` once the bound runs out.
    async fn search_until_hit(
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
        query: &str,
        path: &str,
    ) -> TestResult<serde_json::Value> {
        let mut last_answer = json!(null);
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            let answer = peer
                .call_tool(
                    CallToolRequestParams::new("search")
                        .with_arguments(arguments(&json!({"query": query}))?),
                )
                .await?;
            let structured = answer
                .structured_content
                .ok_or("search must return structured content")?;
            let results = structured["results"]
                .as_array()
                .ok_or("results must be an array")?;
            if results.iter().any(|hit| hit["path"] == json!(path)) {
                return Ok(structured);
            }
            last_answer = structured;
            tokio::time::sleep(SEARCH_TIER_POLL).await;
        }
        Err(format!(
            "the population lane never ranked {path} for query {query}; the last answer was \
             {last_answer:#}"
        )
        .into())
    }

    #[tokio::test]
    async fn startup_populates_the_index_at_the_published_revision() -> TestResult {
        let (_directory, server) = fixture().await?;
        let index = server
            .search_index
            .clone()
            .ok_or("the fixture must open a search index")?;
        let published = server
            .published
            .read()
            .await
            .current
            .reads
            .tree_revision()
            .to_owned();
        // `build` hands the initial publication to the population lane and returns, so the
        // stamp arrives after it rather than with it. Poll under a bound.
        let mut stamped = index.tree_revision().await?;
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            if stamped.as_deref() == Some(published.as_str()) {
                break;
            }
            tokio::time::sleep(SEARCH_TIER_POLL).await;
            stamped = index.tree_revision().await?;
        }
        assert_eq!(
            stamped.as_deref(),
            Some(published.as_str()),
            "the run's first pass must stamp the published tree revision"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_answers_matching_content_while_publication_is_stalled() -> TestResult {
        let (_directory, assembled) = unsupervised_fixture().await?;
        let server = &assembled.server;
        // Search validates its publication by content, as every read does.
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let answer = tokio::time::timeout(UNWAITED_READ_MAX, run_search(server, "beacon"))
            .await
            .map_err(|_| "matching content must answer without a publication")??;
        assert!(!answer.results.is_empty(), "the publication answers");
        assert!(
            stale_index_of(&answer.warnings).is_err(),
            "matching content must not warn stale: {:?}",
            answer.warnings
        );
        Ok(())
    }
}
