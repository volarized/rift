use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rift_core::{ProjectPath as CoreProjectPath, SourceVisibility};
use rift_index::{
    LexicalIndexLimits, LogStore, PathChanges, WorkspaceIndexLimits, capture_digests_with_languages,
};
use rift_protocol::change::{
    ChangeResult, ChangeSummary, GuaranteeEvidence, InsertNodeParams, InsertSymbolParams,
    MoveFileParams, PatchParams, RemoveNodeParams, RemoveSymbolParams, RenameSymbolParams,
    ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::configuration::{
    Duration as WireDuration, LspConfiguration, SEARCH_BUSY_TIMEOUT_MS_MAX, SEARCH_POOL_SLOTS_MAX,
    SERVER_NUM_WORKERS_MAX, SearchConfiguration, SemanticSearchConfiguration, SemanticSource,
    ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_protocol::lock::ProductIdentity;
use rift_protocol::read::{
    DiagnosticCode, Digest, GetSymbolParams, GetSymbolResult, Language, NodesParams, NodesResult,
    Pagination, ProjectPath, ReadWarning, SearchParams, SearchResult,
};
use rift_protocol::workspace::{
    WORKSPACE_SOURCE_UNITS_MAX, WorkspaceHookSummary, WorkspaceLanguageSummary,
    WorkspaceLspSummary, WorkspaceResourcePage, WorkspaceSourceUnit,
};
use rift_search::{
    AcquisitionLimits, ModelSource, RankedUnit, RevisionScoped, SearchError, SearchIndex,
    SearchIndexLimits, SemanticReadiness,
};
use rift_server::{
    ChangeService, EnginePool, HookSnapshot, HookStatus, LspProcessKey, MoveResolution, ReadError,
    ReadFault, ReadService, RemoveResolution, RenameResolution, plan_move, plan_remove_node,
    plan_remove_symbol, plan_rename,
};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
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

use crate::failure::{WireFailure, hook_failure_diagnostic, stale_snapshot_diagnostic};
use crate::resource;
use crate::storage::WorkspaceStorage;
use crate::validation::{
    ConfigurationState, INDEX_CAPTURE_ATTEMPTS_MAX, IndexState, IndexSupervisor,
    IndexSupervisorContext, IndexValidation, LexicalLane, LexicalWrite, PendingWork,
    PopulationLane, PublishedWorkspace, RebuildOutcome, WorkspaceCandidate,
    build_workspace_candidate, configuration_fingerprint, finish_rebuild, initial_workspace,
    lexical_write, run_index_supervisor, workspace_watcher,
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

/// Serializes workspace mutations and snapshot publication.
#[derive(Debug, Default)]
pub(crate) struct ChangeLane {
    entry: AsyncMutex<()>,
}

impl ChangeLane {
    /// Runs one operation after FIFO entry to the workspace lane.
    pub(crate) fn run<Output>(&self, operation: impl FnOnce() -> Output) -> Output {
        let guard = self.entry.blocking_lock();
        let output = operation();
        // Filesystem mutation and snapshot publication finish before release.
        drop(guard);
        output
    }

    /// Verifies occupied entry, reports it, then uses the production lane.
    #[cfg(test)]
    fn run_after_contention<Output>(
        &self,
        on_contention: impl FnOnce(),
        operation: impl FnOnce() -> Output,
    ) -> Output {
        assert!(
            self.entry.try_lock().is_err(),
            "contention witness requires an occupied change lane"
        );
        on_contention();
        self.run(operation)
    }
}

/// Resolves the workspace root the server serves into an absolute path.
///
/// The CLI serves the process working directory, which it names `.`, and every
/// filesystem operation below the root resolves against that same directory, so
/// a relative root reads and writes correctly. A language engine does not: it is
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

/// Sizes the lexical search index's connection pool and busy-wait budget from one accepted
/// `[search]` table, keeping this release's fixed unit, query-term, and match-count bounds.
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
        defaults.units_max(),
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

/// The ranking one search request merges, and what the search index's own state adds to
/// that answer's warnings.
///
/// The search store is read under the tree revision the request captured, so a store that
/// holds another tree contributes no ranking and no warning: the request recaptures
/// instead. What remains here is a store that will not answer at all until an operator
/// acts, which is a different thing to tell a caller.
#[derive(Debug, Default)]
struct SearchRanking {
    units: Vec<RankedUnit>,
    warnings: Vec<ReadWarning>,
}

impl SearchRanking {
    /// No ranking at all: the tier will not answer until an operator acts.
    fn unavailable(detail: &str) -> Self {
        Self {
            units: Vec::new(),
            warnings: vec![ReadWarning::LexicalRankingUnavailable {
                detail: detail.to_owned(),
            }],
        }
    }
}

/// The paths one applied change asks the next snapshot to reparse, or nothing when that
/// snapshot has to read every visible file.
///
/// A change names what it wrote, so an ordinary change reparses exactly those files. Three
/// cases read everything instead: a workspace that declares hooks, because a hook runs
/// after the change lands and may write anything; a written path that decides what the
/// workspace includes, because `rift.toml` and every `.gitignore` reshape the whole file
/// set; and a wire path the index cannot key, which is covered rather than narrowed away.
fn changed_paths_to_reparse(
    root: &Path,
    current: &PublishedWorkspace,
    configuration: &WorkspaceConfiguration,
    summary: &ChangeSummary,
) -> Option<Vec<CoreProjectPath>> {
    if !configuration.hooks.is_empty() {
        return None;
    }
    let mut paths = Vec::with_capacity(summary.files.len());
    for file in &summary.files {
        let path = CoreProjectPath::new(&file.path.0).ok()?;
        if current
            .source_policy
            .decides_inclusion(&root.join(path.as_str()))
        {
            return None;
        }
        paths.push(path);
    }
    Some(paths)
}

/// What one revision-qualified store answer means for the request that asked for it.
///
/// Nothing means the store holds a tree other than the one this request captured, which
/// asks the request to capture the publication the store already answers for. A store
/// holding no tree at all ranks nothing and says so: no pass has ever landed in it, which
/// waits on an operator rather than on work already under way.
fn ranking_of(
    searched: RevisionScoped<Vec<RankedUnit>>,
    readiness: SemanticReadiness,
    files: u64,
) -> Option<SearchRanking> {
    match searched {
        RevisionScoped::Matched(units) => Some(SearchRanking {
            units,
            warnings: readiness_warnings(readiness, files),
        }),
        RevisionScoped::OtherRevision(_) => None,
        RevisionScoped::NoRevision => Some(SearchRanking::unavailable(
            "the workspace search database holds no indexed tree, or could not be read, so \
             the answer was ranked by identifier matching alone; the server log names the \
             failure, and a restart retries it",
        )),
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

/// Workspace MCP server: reads serve an immutable snapshot, changes
/// write the workspace and swap in a fresh snapshot.
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
    changes: Arc<ChangeService>,
    change_lane: Arc<ChangeLane>,
    blocking: BlockingExecutor,
    /// The workspace's search index, absent when it could not be opened at
    /// startup; `search` then serves identifier matching alone and says so in
    /// every answer's warnings.
    search_index: Option<Arc<SearchIndex>>,
    /// The population lane, absent exactly when [`Self::search_index`] is: with
    /// no index open there is no store to populate. A landed change hands its
    /// publication here and returns, rather than awaiting the pass itself.
    population: Option<PopulationLane>,
    /// The lexical lane, absent exactly when [`Self::search_index`] is. A rebuild commits
    /// through it before its snapshot becomes current.
    lexical: Option<LexicalLane>,
    /// The workspace's recorded diagnostics, absent when the store could not be
    /// opened at startup; `rift://logs` then answers with that reason rather
    /// than refusing. The handle is the read side alone: the drain task that
    /// writes records holds its own.
    logs: Option<Arc<LogStore>>,
    engines: Arc<EngineHold>,
    tool_router: ToolRouter<Self>,
}

/// One already-serialized change's outcome, threaded out of the blocking executor so the
/// async `change` method commits and publishes exactly the candidate this change built,
/// without a second, possibly-superseded read of shared state.
struct SerializedChange {
    result: Result<Json<ChangeResult>, ErrorData>,
    candidate: Option<AppliedCandidate>,
}

impl SerializedChange {
    /// A refusal or a diagnostic-only outcome: no candidate to commit or publish.
    const fn wire(result: Result<Json<ChangeResult>, ErrorData>) -> Self {
        Self {
            result,
            candidate: None,
        }
    }
}

/// Candidate one landed change built for publication and diagnostics.
struct AppliedCandidate {
    previous: Arc<PublishedWorkspace>,
    published: Arc<PublishedWorkspace>,
    change_set: rift_index::ChangeSet,
    write: LexicalWrite,
    work: PendingWork,
    epoch: u64,
}

struct AppliedPublication {
    previous: Arc<PublishedWorkspace>,
    published: Option<Arc<PublishedWorkspace>>,
    change_set: rift_index::ChangeSet,
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
        let identity = crate::identity::product_identity()
            .await
            .map_err(|error| ReadFault::task("product identity", error.to_string()))?;
        let startup_configuration = Self::startup_configuration(&root).await?;
        let blocking =
            BlockingExecutor::for_configuration(&startup_configuration.server_configuration());
        let (validation, invalidations) = IndexValidation::new(limits.files_max());
        let watch_root = root.clone();
        let watch_validation = Arc::clone(&validation);
        let watcher = blocking
            .run("workspace watch setup", move || {
                workspace_watcher(&watch_root, &watch_validation)
            })
            .instrument(tracing::info_span!(
                "index.watch",
                component = "index",
                operation = "watch.setup"
            ))
            .await?;
        let (published, lexical_write) =
            initial_workspace(&root, limits, &validation, &blocking).await?;
        // Direct construction delays the database open until the initial scan proves the
        // workspace root. A serving process supplies the owner it opened for foreground log
        // capture; that path creates `.rift` only below an already-existing root.
        let storage = Self::resolve_storage(&root, storage).await;
        let search_configuration = startup_configuration.search_configuration();
        // While `rift.toml` is invalid every request is refused until it is fixed, and the
        // table naming the model is the very part that could not be read. Acquiring the
        // shipped default would spend a download on a server that answers nothing, so the
        // tier waits for a workspace whose configuration was accepted.
        let acquisition = startup_configuration
            .is_accepted()
            .then(|| model_acquisition(&search_configuration.semantic, &root))
            .flatten();
        let search_limits = search_index_limits(&search_configuration, acquisition.as_ref());
        let search_index = open_search_index(&storage, search_limits);
        // The log store shares the database owner without depending on index readiness. Its
        // reads use committed WAL snapshots, so `rift://logs` can answer while a rebuild is
        // still preparing the next publication.
        let logs = storage.logs();
        // The lexical set commits before this snapshot becomes current, so the first
        // request to reach the server reads rows stamped with the tree revision it
        // captured. Embedding does not: the population lane runs the run's first pass
        // afterwards, which establishes the vector set, because a store found on disk was
        // written by an earlier process, possibly under another model. Awaiting that pass
        // here held the first answer for around fifteen seconds on a real workspace.
        let lexical = search_index
            .as_ref()
            .map(|index| LexicalLane::spawn(Arc::clone(index), validation.cancellation.clone()));
        if let Some(lane) = lexical.as_ref() {
            lane.commit(lexical_write, published.reads.tree_revision())
                .await?;
        }
        let population = search_index
            .as_ref()
            .map(|index| PopulationLane::spawn(Arc::clone(index), validation.cancellation.clone()));
        if let Some(lane) = population.as_ref() {
            lane.request(Arc::clone(&published));
        }
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
                validation: Arc::clone(&validation),
                blocking: blocking.clone(),
                population: population.clone(),
                lexical: lexical.clone(),
            },
        ));
        let mut task = validation.task.lock().await;
        *task = Some(supervisor_task);
        drop(task);
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
        Ok(Self {
            root: root.clone(),
            identity,
            limits,
            published,
            validation,
            supervisor_cancellation,
            changes: Arc::new(ChangeService::new(&root)),
            change_lane,
            blocking,
            search_index,
            population,
            lexical,
            logs,
            engines,
            tool_router: Self::tool_router(),
        })
    }

    /// Exact identity advertised by this server.
    #[must_use]
    pub(crate) fn product_identity(&self) -> &ProductIdentity {
        &self.identity
    }

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
    /// version-control revision instead of the current tree. Use `search` when
    /// the name is not exactly known.
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
    /// full-text matches from included `[search.text]` files and declaration bodies. `rev`
    /// searches a version-control revision instead of the current tree. Use `get_symbol`
    /// when the declaration name is known.
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
        let Some(rev) = params.rev.clone() else {
            return self.current_tree_search(params).await;
        };
        // The search index only ever holds the current tree, so a revision-addressed
        // search never consults it.
        self.read_at(Some(rev), move |reads| reads.search(&params, &[]))
            .await
    }

    /// Ranks and reads one current-tree search against one publication.
    ///
    /// The store answers only for the tree it was stamped with, so a store that has moved
    /// past the captured publication ends this attempt rather than ranking rows the answer
    /// cannot place. The next attempt captures the publication the store already holds, and
    /// the bound is the same one `reconcile_workspace` applies to a tree that keeps moving.
    async fn current_tree_search(
        &self,
        params: SearchParams,
    ) -> Result<Json<SearchResult>, ErrorData> {
        for _attempt in 0..INDEX_CAPTURE_ATTEMPTS_MAX {
            let published = self.published_workspace(wire::ErrorPhase::Read).await?;
            let Some(SearchRanking { units, warnings }) = self.ranking(&params, &published).await?
            else {
                continue;
            };
            let executed = params.clone();
            let mut answer = self
                .current_tree_read(&published, move |reads| reads.search(&executed, &units))
                .await?;
            answer.0.warnings.extend(warnings);
            return Ok(answer);
        }
        Err(ReadFault::unavailable(
            "current workspace search",
            "the search store moved past the captured tree across bounded attempts",
        )
        .tool_error(wire::ErrorPhase::Read))
    }

    /// Runs the search index for one request against `published` - the exact snapshot the
    /// caller also runs `ReadService::search` against, never a separately resolved one.
    ///
    /// Returns nothing when the store holds a tree other than `published`'s, which asks the
    /// caller to capture the publication the store already answers for. Every other outcome
    /// ranks: an index that could not be opened, and one holding no tree at all, warn
    /// `lexical_ranking_unavailable` and leave identifier search to answer alone, because
    /// both wait on an operator rather than on a pass already under way. A query-term limit
    /// the index refuses surfaces as this request's own `limit_exceeded` error, never a
    /// silent degrade.
    async fn ranking(
        &self,
        params: &SearchParams,
        published: &PublishedWorkspace,
    ) -> Result<Option<SearchRanking>, ErrorData> {
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
        let searched = index
            .search(published.reads.tree_revision(), query, self.fetch_limit())
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        Ok(ranking_of(
            searched,
            index.readiness(),
            published.reads.file_count(),
        ))
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

    /// Replaces one declaration addressed by symbol. The whole declaration
    /// includes its attached outer attributes and doc comments. The parser
    /// derives the span, so the caller supplies no offsets; a refusal
    /// names the failed precondition and leaves the workspace untouched.
    /// The body is spliced in verbatim at the declaration's own start byte:
    /// its first line inherits the declaration's column, and every later
    /// line carries whatever indentation it is written with.
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
    /// failed precondition and leaves the workspace untouched. A body
    /// inserted `before` its anchor is spliced in at the anchor's start byte
    /// and its first line inherits the anchor's column; a body inserted
    /// `after` its anchor, or at a file target either side, always starts a
    /// fresh line at column zero.
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

    /// Inserts new content beside a syntax node addressed through a witnessed address
    /// from `nodes`. The server recomputes the witness before writing and refuses when
    /// the bytes drifted, the same check `replace_node` runs. Unlike `insert_symbol`,
    /// which separates a new declaration from its anchor with a blank line and preserves
    /// the anchor's indentation, `body` lands verbatim at the node's own boundary with no
    /// separator of its own: a node is not a declaration, so the caller supplies whatever
    /// spacing and indentation the inserted bytes need.
    #[tool]
    async fn insert_node(
        &self,
        Parameters(params): Parameters<InsertNodeParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.insert_node(reads, &params))
            .await
    }

    /// Renames one declaration addressed by symbol through the configured
    /// language engine. The engine proposes the edits; the server verifies
    /// each one against the tree and writes them atomically, then reports
    /// surviving occurrences of the old name as warning findings. Refused
    /// as `unsupported` when no engine serves the declaration's language;
    /// a refusal leaves the workspace untouched.
    #[tool]
    async fn rename_symbol(
        &self,
        Parameters(params): Parameters<RenameSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        let published = self.published_workspace(wire::ErrorPhase::Change).await?;
        published.configuration.accepted(wire::ErrorPhase::Change)?;
        let pool = self.engine_pool_for(&published).await;
        let resolution = plan_rename(&published.reads, &pool, &self.root, &params)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?;
        match resolution {
            RenameResolution::Refused(result) => Ok(Json(result)),
            RenameResolution::Planned(plan) => {
                self.change(move |reads, changes| changes.apply_rename(reads, &plan))
                    .await
            }
        }
    }

    /// Moves one visible file to a new project path. When the configured
    /// language engine advertises will-rename requests for the file, its
    /// reference updates land in the same atomic change; without an engine
    /// or the capability the move still lands and the result carries a
    /// warning that references were not updated. A refusal names the
    /// failed precondition and leaves the workspace untouched.
    #[tool]
    async fn move_file(
        &self,
        Parameters(params): Parameters<MoveFileParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        let published = self.published_workspace(wire::ErrorPhase::Change).await?;
        published.configuration.accepted(wire::ErrorPhase::Change)?;
        let pool = self.engine_pool_for(&published).await;
        let resolution = plan_move(&published.reads, &pool, &self.root, &params)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?;
        match resolution {
            MoveResolution::Refused(result) => Ok(Json(result)),
            MoveResolution::Planned(plan) => {
                self.change(move |reads, changes| changes.apply_move(reads, &plan))
                    .await
            }
        }
    }

    /// Removes one declaration addressed by symbol. The whole declaration,
    /// its attached outer attributes and doc comments included, is removed
    /// together with the separator that followed it, so no blank-line run
    /// stands where it stood. When the configured language engine
    /// advertises `textDocument/references`, a standing reference refuses
    /// `unmet_precondition` naming `no_references`, unless `force` applies
    /// the removal anyway and carries the references as a warning. Without
    /// such an engine, the removal applies and carries a warning naming why
    /// it was not checked.
    #[tool]
    async fn remove_symbol(
        &self,
        Parameters(params): Parameters<RemoveSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        let published = self.published_workspace(wire::ErrorPhase::Change).await?;
        published.configuration.accepted(wire::ErrorPhase::Change)?;
        let pool = self.engine_pool_for(&published).await;
        let resolution = plan_remove_symbol(&published.reads, &pool, &self.root, &params)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?;
        match resolution {
            RemoveResolution::Refused(result) => Ok(Json(result)),
            RemoveResolution::Planned(plan) => {
                self.change(move |reads, changes| changes.apply_remove(reads, &plan))
                    .await
            }
        }
    }

    /// Removes one syntax node through a witnessed address from `nodes`.
    /// The server recomputes the witness before writing and refuses when
    /// the bytes drifted, so a stale address never removes moved code. When
    /// the node names a declaration, the removal is checked against the
    /// configured language engine's references the same way `remove_symbol`
    /// checks them; a node naming no declaration applies unchecked, with a
    /// warning saying so.
    #[tool]
    async fn remove_node(
        &self,
        Parameters(params): Parameters<RemoveNodeParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        let published = self.published_workspace(wire::ErrorPhase::Change).await?;
        published.configuration.accepted(wire::ErrorPhase::Change)?;
        let pool = self.engine_pool_for(&published).await;
        let resolution = plan_remove_node(&published.reads, &pool, &self.root, &params)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?;
        match resolution {
            RemoveResolution::Refused(result) => Ok(Json(result)),
            RemoveResolution::Planned(plan) => {
                self.change(move |reads, changes| changes.apply_remove(reads, &plan))
                    .await
            }
        }
    }

    /// Applies unified-diff hunks to workspace files atomically. The target is any
    /// file the workspace's `[source]` policy makes visible, parsed or not. Hunk
    /// context guards the change: a header's line numbers are hints and
    /// its line counts are read from the hunk's own body, as with
    /// `git apply`. A `/dev/null` header creates or deletes the file. A body
    /// that is not a unified diff, such as an `*** Begin Patch` envelope, is
    /// refused naming the form to send. The result names each file the change
    /// wrote with its size and line counts.
    #[tool]
    async fn patch(
        &self,
        Parameters(params): Parameters<PatchParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.patch(reads, &params))
            .await
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
        Answer: Send + 'static,
    {
        let published = self.published_workspace(wire::ErrorPhase::Read).await?;
        let Some(rev) = rev else {
            return self.current_tree_read(&published, operation).await;
        };
        let configuration = published.configuration.accepted(wire::ErrorPhase::Read)?;
        if !configuration.providers.history.enabled {
            return Err(ReadError::from(ReadFault::Unsupported {
                capability: "revision reads (providers.history disabled)".to_owned(),
            })
            .tool_error(wire::ErrorPhase::Read));
        }
        let visibility = SourceVisibility::from(&configuration.source);
        let text_inclusion = rift_core::TextFileInclusion::from(&configuration.search);
        let languages = rift_core::LanguageFileSelections::from(&configuration);
        let history = configuration.providers.history.clone();
        let root = self.root.clone();
        let limits = self.limits;
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

    /// Runs one read against `published`'s current-tree snapshot, behind the acceptance
    /// gate every request passes. Shared by `read_at`'s current-tree path and `search`,
    /// which resolves `published` itself first so the lexical tier's revision check and
    /// the identifier read it merges into can never straddle two different snapshots.
    async fn current_tree_read<Answer>(
        &self,
        published: &Arc<PublishedWorkspace>,
        operation: impl FnOnce(&ReadService) -> Result<Answer, ReadError> + Send + 'static,
    ) -> Result<Json<Answer>, ErrorData>
    where
        Answer: Send + 'static,
    {
        published.configuration.accepted(wire::ErrorPhase::Read)?;
        let reads = Arc::clone(&published.reads);
        self.blocking
            .run("current workspace read", move || operation(&reads))
            .await
            .map(Json)
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))
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
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
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
                "the index settled at epoch {published} but the tree kept moving under the \
                 request for {waited_ms}ms; read rift://logs for the rebuilds it made"
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
    /// whole workspace.
    async fn reconcile_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        for _attempt in 0..INDEX_CAPTURE_ATTEMPTS_MAX {
            let current = self.await_current_workspace(phase).await?;
            let root = self.root.clone();
            let limits = self.limits;
            let visibility = current.configuration.source_visibility();
            let text_inclusion = current.configuration.text_inclusion();
            let languages = current.configuration.language_file_selections();
            let capture = self
                .blocking
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
                .await;
            let (digests, configuration_fingerprint) = match capture {
                Ok(capture) => capture,
                Err(error) => {
                    let _ = self.validation.observe_whole_workspace();
                    return Err(error.tool_error(phase));
                }
            };
            let configuration_matches =
                current.configuration.fingerprint == configuration_fingerprint;
            let epoch_matches = current.epoch == self.validation.observed_epoch();
            if digests.fingerprint() == current.fingerprint
                && configuration_matches
                && epoch_matches
            {
                current.configuration.accepted(phase)?;
                return Ok(current);
            }
            let observed = if configuration_matches {
                let changes = PathChanges::between(&current.reads.workspace_digests(), &digests);
                self.validation
                    .observe_paths(changes.iter().map(|(path, _)| path.clone()))
            } else {
                self.validation.observe_whole_workspace()
            };
            observed.map_err(|error| error.tool_error(phase))?;
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
                return Ok(current);
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
                && failed_epoch == observed_epoch
            {
                return Err(error.tool_error(phase));
            }
            changed.as_mut().await;
        }
    }

    /// Runs one change, publishes its snapshot, then pulls engine diagnostics.
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
        let validation = Arc::clone(&self.validation);
        let changes = Arc::clone(&self.changes);
        let change_lane = Arc::clone(&self.change_lane);
        let outcome = self
            .blocking
            .run("workspace change", move || {
                change_lane.run(|| {
                    Self::change_serialized(
                        &root,
                        limits,
                        &published,
                        &validation,
                        &changes,
                        operation,
                    )
                })
            })
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?;
        let mut result = outcome.result?;
        let publication = match (&mut result, outcome.candidate) {
            (Json(ChangeResult::Applied { summary }), Some(candidate)) => {
                self.publish_applied_change(candidate, summary).await
            }
            _ => None,
        };
        let published_next = publication
            .as_ref()
            .and_then(|publication| publication.published.as_ref());
        if let (Some(lane), Some(next)) = (self.population.as_ref(), published_next) {
            lane.request(Arc::clone(next));
        }
        if let Json(ChangeResult::Applied { summary }) = &mut result
            && let Some(publication) = publication.as_ref()
            && let Some(snapshot) = self.diagnostics_snapshot(published_next, summary).await
        {
            if publication.previous.configuration.has_validation_hooks() {
                self.engines.shutdown().await;
            }
            let engines = self.engine_pool_for(&snapshot).await;
            summary.diagnostics.extend(
                rift_server::engine_change_set_diagnostics(
                    &engines,
                    &publication.previous.reads,
                    &snapshot.reads,
                    &publication.change_set,
                )
                .await,
            );
        }
        Ok(result)
    }

    /// Commits one landed change and returns its diagnostic snapshots.
    async fn publish_applied_change(
        &self,
        candidate: AppliedCandidate,
        summary: &mut ChangeSummary,
    ) -> Option<AppliedPublication> {
        let AppliedCandidate {
            previous,
            published,
            change_set,
            write,
            work,
            epoch,
        } = candidate;
        if let Some(lane) = self.lexical.as_ref()
            && let Err(error) = lane.commit(write, published.reads.tree_revision()).await
        {
            self.validation.restore_pending(work);
            summary.diagnostics.push(stale_snapshot_diagnostic(&error));
            return None;
        }
        let state = Arc::clone(&self.published);
        let validation = Arc::clone(&self.validation);
        let publishing = Arc::clone(&published);
        let outcome = self
            .blocking
            .run("workspace change publication", move || {
                Ok(finish_rebuild(&state, &validation, publishing, work, epoch))
            })
            .await;
        match outcome {
            Ok(RebuildOutcome::Published) => {
                tracing::info!(
                    component = "index",
                    operation = "index.publish",
                    trigger = "rift_change",
                    epoch,
                    "index snapshot published"
                );
                Some(AppliedPublication {
                    previous,
                    published: Some(published),
                    change_set,
                })
            }
            Ok(RebuildOutcome::Superseded) => Some(AppliedPublication {
                previous,
                published: None,
                change_set,
            }),
            Err(error) => {
                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                None
            }
        }
    }

    /// The snapshot engine diagnostics pull against after one applied
    /// change: the change's own publication, or the currently published
    /// workspace when a concurrent rebuild superseded it - the change lane
    /// serialized the write, so that snapshot holds the changed tree, plus
    /// whatever external writes landed after it. A change whose rebuild
    /// failed pulls nothing: its summary already carries the stale-snapshot
    /// warning, and current-tree reads refuse until a fresh snapshot
    /// publishes.
    async fn diagnostics_snapshot(
        &self,
        published_next: Option<&Arc<PublishedWorkspace>>,
        summary: &ChangeSummary,
    ) -> Option<Arc<PublishedWorkspace>> {
        if let Some(next) = published_next {
            return Some(Arc::clone(next));
        }
        let stale = DiagnosticCode::SnapshotStale.code();
        let rebuild_failed = summary
            .diagnostics
            .iter()
            .any(|finding| finding.code.as_deref() == Some(stale.as_str()));
        if rebuild_failed {
            return None;
        }
        Some(Arc::clone(&self.published.read().await.current))
    }

    /// One serialized change result and its new workspace snapshot.
    fn change_serialized(
        root: &Path,
        limits: WorkspaceIndexLimits,
        published: &RwLock<IndexState>,
        validation: &IndexValidation,
        changes: &ChangeService,
        operation: impl FnOnce(&ReadService, &ChangeService) -> Result<ChangeResult, ReadError>,
    ) -> Result<SerializedChange, ReadError> {
        let state = published.blocking_read();
        let (current, _) = state.snapshot();
        drop(state);
        if current.epoch != validation.observed_epoch() {
            return Ok(SerializedChange::wire(Err(ReadFault::unavailable(
                "workspace change",
                "index changed before operation acceptance",
            )
            .tool_error(wire::ErrorPhase::Change))));
        }
        let configuration = match current.configuration.accepted(wire::ErrorPhase::Change) {
            Ok(configuration) => configuration,
            Err(error) => return Ok(SerializedChange::wire(Err(error))),
        };
        let original = if configuration.hooks.is_empty() {
            None
        } else {
            let snapshot = changes.capture_hook_snapshot(&current.reads)?;
            snapshot.require_source_text()?;
            Some(snapshot)
        };
        let result = operation(&current.reads, changes)?;
        let mut result = match original {
            Some(original) => Self::apply_hook_pipeline(
                root,
                &configuration,
                changes,
                &current,
                &original,
                result,
            )?,
            None => result,
        };
        let candidate = if let ChangeResult::Applied { summary } = &mut result {
            Self::rebuild_after_applied_change(
                root,
                limits,
                validation,
                &configuration,
                &current,
                summary,
            )
        } else {
            None
        };
        Ok(SerializedChange {
            result: Ok(Json(result)),
            candidate,
        })
    }

    /// Rebuilds snapshot after one landed change and returns publication candidate.
    fn rebuild_after_applied_change(
        root: &Path,
        limits: WorkspaceIndexLimits,
        validation: &IndexValidation,
        configuration: &WorkspaceConfiguration,
        current: &Arc<PublishedWorkspace>,
        summary: &mut ChangeSummary,
    ) -> Option<AppliedCandidate> {
        let observed = match changed_paths_to_reparse(root, current, configuration, summary) {
            Some(paths) => validation.observe_paths(paths),
            None => validation.observe_whole_workspace(),
        };
        if let Err(error) = observed {
            summary.diagnostics.push(stale_snapshot_diagnostic(&error));
            return None;
        }
        if current.configuration.fingerprint != configuration_fingerprint(root) {
            let error = ReadFault::unavailable(
                "workspace change",
                "configuration changed during snapshot rebuild",
            );
            let _ = validation.observe_whole_workspace();
            summary.diagnostics.push(stale_snapshot_diagnostic(&error));
            return None;
        }
        let mut request = validation.take_pending();
        request.previous = Some(Arc::clone(current));
        let epoch = request.epoch;
        let candidate = match build_workspace_candidate(root, limits, &request) {
            Ok(WorkspaceCandidate::Stable {
                published,
                change_set,
            }) => {
                let write = lexical_write(&published, &change_set);
                AppliedCandidate {
                    previous: Arc::clone(current),
                    published,
                    change_set,
                    write,
                    work: request.work,
                    epoch,
                }
            }
            Ok(WorkspaceCandidate::ConfigurationChanged) => {
                let error = ReadFault::unavailable(
                    "workspace change",
                    "configuration changed during snapshot rebuild",
                );
                let _ = validation.observe_whole_workspace();
                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                return None;
            }
            Err(error) => {
                validation.restore_pending(request.work);
                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                return None;
            }
        };
        Some(candidate)
    }

    /// Runs transforms, restores rejected writes, then runs validations.
    fn apply_hook_pipeline(
        root: &Path,
        configuration: &WorkspaceConfiguration,
        changes: &ChangeService,
        current: &PublishedWorkspace,
        original: &HookSnapshot,
        result: ChangeResult,
    ) -> Result<ChangeResult, ReadError> {
        let ChangeResult::Applied { mut summary } = result else {
            return Ok(result);
        };
        let changed_paths = summary.paths();
        let selected_hooks =
            rift_server::selected_hooks(&configuration.hooks, root, &changed_paths)
                .map_err(ReadFault::index)?;
        if selected_hooks.is_empty() {
            return Ok(ChangeResult::Applied { summary });
        }
        let direct_paths: std::collections::BTreeSet<&str> =
            changed_paths.iter().map(|path| path.0.as_str()).collect();

        for hook in selected_hooks
            .iter()
            .copied()
            .filter(|hook| hook.writes.is_transform())
        {
            let before = changes.capture_hook_snapshot(&current.reads)?;
            before.require_source_text()?;
            let run = rift_server::run_hook(hook, root, &changed_paths);
            let after = changes.capture_hook_snapshot(&current.reads)?;
            let hook_paths = before.changed_paths(&after);
            let in_scope = hook.writes == rift_protocol::configuration::HookWrites::Workspace
                || hook_paths
                    .iter()
                    .all(|path| direct_paths.contains(path.0.as_str()));
            let permissions_changed = before.permissions_changed(&after);
            let unavailable_path = after.unavailable_path();
            if run.status == HookStatus::Passed
                && in_scope
                && !permissions_changed
                && unavailable_path.is_none()
            {
                continue;
            }
            changes.restore_hook_snapshot(&current.reads, &before, &after)?;
            let mut reported = run;
            if reported.status == HookStatus::Passed {
                let detail = if let Some(path) = unavailable_path {
                    format!("hook source unavailable: {path}")
                } else if permissions_changed {
                    "hook changed source permissions".to_owned()
                } else {
                    "hook changed source outside declared write scope".to_owned()
                };
                reported.status = HookStatus::Error(detail);
            }
            summary
                .diagnostics
                .push(hook_failure_diagnostic(hook, &reported));
        }

        let final_snapshot = changes.capture_hook_snapshot(&current.reads)?;
        final_snapshot.require_source_text()?;
        let ChangeResult::Applied { mut summary } =
            changes.finalize_hook_result(original, &final_snapshot, summary)?
        else {
            return Ok(ChangeResult::Unchanged);
        };
        let validated_paths = summary.paths();

        for hook in selected_hooks
            .iter()
            .copied()
            .filter(|hook| hook.writes.is_validation())
        {
            let before = changes.capture_hook_snapshot(&current.reads)?;
            before.require_source_text()?;
            let run = rift_server::run_hook(hook, root, &validated_paths);
            let after = changes.capture_hook_snapshot(&current.reads)?;
            if !before.is_unchanged(&after) {
                changes.restore_hook_snapshot(&current.reads, &before, &after)?;
                let mut reported = run;
                reported.status =
                    HookStatus::Error("validation hook changed source files".to_owned());
                summary
                    .diagnostics
                    .push(hook_failure_diagnostic(hook, &reported));
            } else if run.status == HookStatus::Passed {
                summary
                    .guarantees
                    .extend(hook.guarantees.iter().map(|guarantee| GuaranteeEvidence {
                        kind: guarantee.kind,
                        scope: guarantee.scope.clone(),
                        hook: hook.id.clone(),
                        detail: guarantee.detail.clone(),
                    }));
            } else {
                summary
                    .diagnostics
                    .push(hook_failure_diagnostic(hook, &run));
            }
        }

        Ok(ChangeResult::Applied { summary })
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
        let pool = self.engine_pool_for(&current).await;
        let configuration = current
            .configuration
            .accepted
            .as_ref()
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?
            .clone();
        let languages = workspace_languages(&current, &configuration, &pool)?;
        let hooks = configuration
            .hooks
            .iter()
            .map(|hook| WorkspaceHookSummary {
                id: hook.id.clone(),
                kind: hook.kind,
                include: hook.include.clone(),
                exclude: hook.exclude.clone(),
            })
            .collect();
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
            hooks,
            source: page_source,
            pagination: Pagination {
                page_index,
                total_pages: u64::try_from(total_pages).unwrap_or(u64::MAX),
            },
        };
        Ok(resource::rendered_workspace(uri, &page))
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
            "Read and edit the current workspace: get_symbol and search find \
             declarations, nodes lists witnessed syntax nodes at a byte position, \
             and replace_symbol, insert_symbol, replace_node, rename_symbol, \
             move_file, and patch change code atomically behind verified \
             preconditions. The rift://workspace resource reads effective configuration \
             and source files. The rift://logs resource reads the server's own diagnostics, \
             including while a tool refuses.",
        );
        info.meta = Some(crate::identity::identity_meta(&self.identity));
        info
    }
}

#[cfg(test)]
mod tests {
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
    use rift_search::{ModelSource, RevisionScoped, SemanticReadiness};
    use rift_server::{ChangeService, ConfigurationFault, LspProcessKey, ReadError, ReadFault};

    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    use crate::validation::RebuildRequest;

    use super::{BlockingExecutor, ChangeLane, Parameters, RiftMcp};
    use crate::validation::{
        ConfigurationState, IndexState, IndexValidation, PublishedWorkspace, WorkspaceCandidate,
        build_workspace_candidate, configuration_fingerprint, record_rebuild_failure,
    };

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
        )? {
            WorkspaceCandidate::Stable { published, .. } => Ok(published),
            WorkspaceCandidate::ConfigurationChanged => {
                Err("fixture configuration must remain stable".into())
            }
        }
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
            ReadWarning::SourceUnavailable { unit, detail }
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
                ReadWarning::SourceUnavailable { unit, .. } if unit.0.contains("invalid.rs")
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
            ReadWarning::SourceUnavailable { unit, detail }
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
        supervisor.shutdown().await?;
        supervisor.shutdown().await?;
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
            .expect("second operation must reach occupied entry");
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
                    accepted: Err(Arc::new(configuration_error)),
                    fingerprint: configuration_fingerprint(directory.path()),
                },
                fingerprint: candidate.fingerprint.clone(),
                source_policy: Arc::clone(&candidate.source_policy),
                epoch: 0,
            }),
            failure: None,
        });
        let (validation, _invalidations) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let changes = ChangeService::new(directory.path());
        let operation_called = AtomicBool::new(false);

        let outcome = RiftMcp::change_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &published,
            &validation,
            &changes,
            |_, _| {
                operation_called.store(true, Ordering::SeqCst);
                panic!("invalid configuration must stop before operation")
            },
        )?;
        assert!(outcome.candidate.is_none());
        let Err(error) = outcome.result else {
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
                "insert_node",
                "insert_symbol",
                "move_file",
                "nodes",
                "patch",
                "remove_node",
                "remove_symbol",
                "rename_symbol",
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
        assert_eq!(
            structured["summary"]["files"][0]["path"],
            json!("lib.rs"),
            "the applied summary names the file it wrote"
        );
        assert_eq!(structured["summary"]["files"][0]["kind"], json!("modified"));
        assert_eq!(structured["summary"]["files"][0]["lines_added"], json!(3));

        let symbol = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon"}))?),
            )
            .await?;
        let structured = symbol
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        let excerpt = structured["hits"][0]["source"]
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

    /// A text file created after startup is searchable only because the change's own
    /// publication reached the population lane; the run's first pass never saw it.
    #[tokio::test]
    async fn client_change_creating_a_text_file_populates_lexical_search() -> TestResult {
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

        let diff = "--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+the migration guide covers replacing every legacy unit\n";
        let change = client
            .call_tool(
                CallToolRequestParams::new("patch")
                    .with_arguments(arguments(&json!({"patch": diff}))?),
            )
            .await?;
        let structured = change
            .structured_content
            .ok_or("patch must return structured content")?;
        assert_eq!(structured["status"], json!("applied"));

        // The change hands its publication to the population lane and returns, and the
        // change's own writes also wake the watcher, whose rebuild hands over a newer one.
        // Until one of those passes lands, the revision guard serves identifier-only
        // results. Poll within a bound instead of asserting the first answer, because that
        // degraded window is advertised behavior.
        search_until_hit(client.peer(), "replacing legacy unit", "notes.txt").await?;

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// `build` returns before the population lane runs the run's first pass.
    ///
    /// Awaiting that pass inside `build` is what held the first answer for around fifteen
    /// seconds on a real workspace. The caller sees the fix directly: the first search a
    /// freshly built server answers is ranked without the store, and names the degraded
    /// ranking rather than carrying the store's own hits. A `build` that awaited its pass
    /// could not answer that way at all, whatever the machine.
    ///
    /// `max_chunk` at its enforced minimum against a megabyte of text is what puts a
    /// thousand lexical units in that pass, so the pass is real work next to the one
    /// in-process call that follows `build`.
    #[tokio::test]
    async fn build_answers_from_a_store_that_already_holds_its_tree() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("guide.txt"),
            "the earlier guide covers every legacy sensor ".repeat(24_000),
        )?;
        super::hermetic_workspace(directory.path(), "[search.text]\nmax_chunk = \"1kb\"\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;

        // The lexical set commits before the snapshot becomes current, so the very first
        // request reads rows stamped with the tree it captured - no lag window, and no
        // identifier-only answer while a pass catches up.
        let first = run_search(&server, "legacy sensor").await?;
        assert!(
            store_ranked(&first),
            "the first answer must be ranked by a store that already holds this tree: \
             {first:#?}"
        );
        assert!(
            !first.results.is_empty(),
            "the committed unit set must be searchable: {first:#?}"
        );
        Ok(())
    }

    /// Calls one tool, retrying the refusals the server advertises as
    /// `retry: same_request`: a concurrent write may move the index between
    /// snapshot and acceptance, and the wire contract answers with a bounded
    /// retry rather than a failure.
    async fn call_until_accepted(
        peer: &rmcp::service::Peer<rmcp::service::RoleClient>,
        params: CallToolRequestParams,
    ) -> TestResult<rmcp::model::CallToolResult> {
        const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;
        for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
            match peer.call_tool(params.clone()).await {
                Ok(result) => return Ok(result),
                Err(ServiceError::McpError(error))
                    if error
                        .data
                        .as_ref()
                        .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("the server kept refusing a retryable change".into())
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
        let first = call_until_accepted(
            &first_client,
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon",
                "position": "after",
                "body": "pub fn first_insert() {}"
            }))?),
        );
        let second = call_until_accepted(
            &second_client,
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
    async fn applied_change_skips_oversized_content_without_false_rebuild_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
        let tight = rift_index::WorkspaceIndexLimits::new(4, 60, 60, 4, 100)
            .map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;
        let reads = server.clone();
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
        assert!(
            structured["summary"].get("diagnostics").is_none(),
            "an applied change with no findings must omit diagnostics"
        );

        let skipped = get_symbol(&reads, "beacon").await?;
        assert!(skipped.hits.is_empty());
        assert!(skipped.warnings.iter().any(|warning| matches!(
            warning,
            ReadWarning::SourceUnavailable { unit, detail }
                if unit.0.ends_with("/lib.rs") && detail.contains("file byte limit")
        )));

        client.cancel().await?;
        server_task.await?;
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

    #[test]
    fn serialized_change_refuses_when_index_already_moved() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let candidate = stable_candidate(directory.path(), 0)?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        validation
            .observe_whole_workspace()
            .map_err(|error| format!("observation must land: {error:?}"))?;
        let published = tokio::sync::RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let changes = ChangeService::new(directory.path());
        let outcome = RiftMcp::change_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &published,
            &validation,
            &changes,
            |_, _| panic!("a moved index must refuse before the operation runs"),
        )?;
        assert!(outcome.candidate.is_none());
        let Err(error) = outcome.result else {
            panic!("a moved index must refuse the change");
        };
        assert!(
            error
                .message
                .contains("index changed before operation acceptance"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }

    #[test]
    fn applied_change_reports_lost_observation_as_stale_snapshot() -> TestResult {
        use rift_protocol::change::{ChangeId, ChangeResult, ChangeSummary};
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let candidate = stable_candidate(directory.path(), 0)?;
        let (validation, receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        drop(receiver);
        let published = tokio::sync::RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let changes = ChangeService::new(directory.path());
        let outcome = RiftMcp::change_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &published,
            &validation,
            &changes,
            |_, _| {
                Ok(ChangeResult::Applied {
                    summary: ChangeSummary {
                        id: ChangeId("0123abcd".to_owned()),
                        files: Vec::new(),
                        diagnostics: Vec::new(),
                        guarantees: Vec::new(),
                    },
                })
            },
        )?;
        assert!(outcome.candidate.is_none());
        let Ok(rmcp::Json(ChangeResult::Applied { summary })) = outcome.result else {
            panic!("the applied change must survive a lost observation");
        };
        assert_eq!(summary.diagnostics.len(), 1);
        assert!(
            summary.diagnostics[0].message.contains("could not refresh"),
            "diagnostic must explain the stale snapshot: {:?}",
            summary.diagnostics[0]
        );
        Ok(())
    }

    #[test]
    fn applied_change_that_moves_configuration_reports_stale_snapshot() -> TestResult {
        use rift_protocol::change::{ChangeId, ChangeResult, ChangeSummary};
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let candidate = stable_candidate(directory.path(), 0)?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let published = tokio::sync::RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let changes = ChangeService::new(directory.path());
        let root = directory.path().to_path_buf();
        let outcome = RiftMcp::change_serialized(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &published,
            &validation,
            &changes,
            move |_, _| {
                let moved = "[providers.history]\nenabled = false\n";
                fs::write(root.join("rift.toml"), moved).map_err(|error| {
                    ReadFault::task("test configuration write", error.to_string())
                })?;
                Ok(ChangeResult::Applied {
                    summary: ChangeSummary {
                        id: ChangeId("0123abcd".to_owned()),
                        files: Vec::new(),
                        diagnostics: Vec::new(),
                        guarantees: Vec::new(),
                    },
                })
            },
        )?;
        assert!(outcome.candidate.is_none());
        let Ok(rmcp::Json(ChangeResult::Applied { summary })) = outcome.result else {
            panic!("the applied change must survive a moved configuration");
        };
        assert_eq!(summary.diagnostics.len(), 1);
        assert!(
            summary.diagnostics[0]
                .message
                .contains("configuration changed during snapshot rebuild"),
            "diagnostic must name the moved configuration: {:?}",
            summary.diagnostics[0]
        );
        Ok(())
    }

    #[test]
    fn applied_change_that_breaks_the_source_policy_rebuild_reports_stale_snapshot() -> TestResult {
        use rift_protocol::change::{
            ChangeId, ChangeResult, ChangeSummary, FileChange, FileChangeKind,
        };
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let candidate = stable_candidate(directory.path(), 0)?;
        let (validation, _receiver) =
            IndexValidation::new(WorkspaceIndexLimits::default().files_max());
        let published = tokio::sync::RwLock::new(IndexState {
            current: candidate,
            failure: None,
        });
        let changes = ChangeService::new(directory.path());
        let root = directory.path().to_path_buf();
        // `files_max=1` accepts the workspace's single Rust source file: the source scan
        // never counts `.gitignore` files. `ReadService::build` also compiles the `[source]`
        // policy right after that scan, and its `GitignoreChain` walk counts each `.gitignore`
        // file against that same bound, so two `.gitignore` files written by the change trip
        // `TooManyFiles` there even though the source scan alone already succeeded.
        let tight_limits = WorkspaceIndexLimits::new(1, 1_048_576, 10_485_760, 16, 5)
            .expect("tight limits accept exactly one file");
        let outcome = RiftMcp::change_serialized(
            directory.path(),
            tight_limits,
            &published,
            &validation,
            &changes,
            move |_, _| {
                let nested = root.join("nested");
                let root_gitignore = root.join(".gitignore");
                let nested_gitignore = root.join("nested/.gitignore");
                fs::create_dir_all(&nested).expect("nested directory scaffold must write");
                fs::write(&root_gitignore, "").expect("root gitignore scaffold must write");
                fs::write(&nested_gitignore, "").expect("nested gitignore scaffold must write");
                Ok(ChangeResult::Applied {
                    summary: ChangeSummary {
                        id: ChangeId("0123abcd".to_owned()),
                        // A written `.gitignore` decides what the workspace includes, so
                        // this change asks for the whole workspace and its `[source]`
                        // policy is compiled again.
                        files: vec![
                            FileChange {
                                path: rift_protocol::read::ProjectPath(".gitignore".to_owned()),
                                kind: FileChangeKind::Created,
                                size_bytes: 0,
                                line_count: 0,
                                lines_added: 0,
                                lines_removed: 0,
                            },
                            FileChange {
                                path: rift_protocol::read::ProjectPath(
                                    "nested/.gitignore".to_owned(),
                                ),
                                kind: FileChangeKind::Created,
                                size_bytes: 0,
                                line_count: 0,
                                lines_added: 0,
                                lines_removed: 0,
                            },
                        ],
                        diagnostics: Vec::new(),
                        guarantees: Vec::new(),
                    },
                })
            },
        )?;
        let Ok(rmcp::Json(ChangeResult::Applied { summary })) = outcome.result else {
            panic!("the applied change must survive a failed source-policy rebuild");
        };
        assert_eq!(summary.diagnostics.len(), 1);
        let diagnostic = &summary.diagnostics[0];
        assert!(
            diagnostic.message.contains("too_many_files"),
            "diagnostic must name the source-policy rebuild failure: {diagnostic:?}"
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

    #[tokio::test]
    async fn recorded_rebuild_failure_serves_reads_the_typed_error() -> TestResult {
        let (_directory, server) = fixture().await?;
        let lane_guard = server.change_lane.entry.lock().await;
        let epoch = server
            .validation
            .observe_whole_workspace()
            .map_err(|error| format!("observation must land: {error:?}"))?;
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
            "the failure must be recorded at the current epoch"
        );
        let error = get_symbol(&server, "beacon")
            .await
            .expect_err("a recorded rebuild failure must refuse current reads");
        assert!(
            error.message.contains("injected failure"),
            "unexpected refusal: {error:?}"
        );
        drop(lane_guard);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn reads_time_out_while_publication_is_stalled() -> TestResult {
        let (_directory, server) = fixture().await?;
        // Advance the observed epoch without an invalidation signal, so no
        // rebuild ever publishes a matching snapshot and the read must wait.
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let error = get_symbol(&server, "beacon")
            .await
            .expect_err("a stalled publication must miss the freshness deadline");
        assert!(
            error.message.contains("filesystem events behind the tree"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_read_names_the_epochs_it_waited_on() -> TestResult {
        let (_directory, server) = fixture().await?;
        server
            .validation
            .observed_epoch
            .fetch_add(3, std::sync::atomic::Ordering::SeqCst);

        let error = get_symbol(&server, "beacon")
            .await
            .expect_err("a stalled publication must refuse");

        assert!(error.message.contains("published epoch 0"), "{error:?}");
        assert!(error.message.contains("observed epoch 3"), "{error:?}");
        assert!(error.message.contains("rift://logs"), "{error:?}");
        Ok(())
    }

    #[tokio::test]
    async fn a_stall_after_the_epoch_settled_names_tree_movement() -> TestResult {
        let (_directory, server) = fixture().await?;

        let detail = server.readiness_stall(Duration::from_millis(25)).await;

        assert!(detail.contains("the index settled at epoch 0"), "{detail}");
        assert!(detail.contains("tree kept moving"), "{detail}");
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
    async fn workspace_resource_reports_named_inline_and_disabled_languages() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("script.py"), "print('beacon')\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[lsp.shared]\ncommand = \"language-server\"\n\
             [languages.python]\ninclude = [\"**/*.py\"]\nexecution = true\nlsp = \"shared\"\n\
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

        let python = language("python");
        assert_eq!(
            python["execution"],
            serde_json::json!(true),
            "an enabled entry carries its own execution permission"
        );
        assert_eq!(python["syntax"], serde_json::json!(false));
        assert_eq!(python["lsp"]["process"], serde_json::json!("shared"));
        assert_eq!(python["lsp"]["state"], serde_json::json!("stopped"));

        let toml = language("toml");
        assert_eq!(toml["enabled"], serde_json::json!(false));
        assert_eq!(toml["execution"], serde_json::json!(false));
        assert!(
            toml.get("lsp").is_none(),
            "disabled language omits LSP state"
        );
        Ok(())
    }

    /// Each configured hook is reported with its own path selection, beside the
    /// language entries rather than inside one.
    #[tokio::test]
    async fn workspace_resource_reports_each_hook_with_its_path_selection() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[[hooks]]\nid = \"tests\"\nkind = \"test\"\ncommand = [\"true\"]\n\
             determinism = \"deterministic\"\ninclude = [\"crates/**\"]\n\
             exclude = [\"crates/generated/**\"]\n",
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
        let hooks = body["hooks"].as_array().expect("hooks are an array");
        assert_eq!(hooks.len(), 1, "{text}");
        assert_eq!(hooks[0]["id"], serde_json::json!("tests"));
        assert_eq!(hooks[0]["kind"], serde_json::json!("test"));
        assert_eq!(hooks[0]["include"], serde_json::json!(["crates/**"]));
        assert_eq!(
            hooks[0]["exclude"],
            serde_json::json!(["crates/generated/**"])
        );
        assert_eq!(
            body["pagination"]["total_pages"],
            serde_json::json!(1),
            "one page holds the whole catalog: {text}"
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

    #[test]
    fn a_store_holding_another_tree_asks_the_request_to_recapture() {
        assert!(
            super::ranking_of(
                RevisionScoped::OtherRevision("aaaaaaaa".to_owned()),
                SemanticReadiness::Ready,
                10,
            )
            .is_none(),
            "rows from another tree are never merged into this answer, and no warning \
             stands in for the recapture"
        );
    }

    #[test]
    fn a_store_holding_no_tree_warns_that_it_will_not_answer() -> TestResult {
        let ranking = super::ranking_of(RevisionScoped::NoRevision, SemanticReadiness::Ready, 10)
            .ok_or("a store holding no tree still ranks, by identifier matching alone")?;
        assert!(ranking.units.is_empty());
        let warnings = serde_json::to_value(&ranking.warnings)?;
        assert_eq!(warnings[0]["code"], json!("lexical_ranking_unavailable"));
        assert_eq!(warnings[1], json!(null), "exactly one warning is raised");
        Ok(())
    }

    #[test]
    fn a_matched_store_ranks_its_units_and_carries_the_readiness_warning() -> TestResult {
        let ranking = super::ranking_of(
            RevisionScoped::Matched(Vec::new()),
            SemanticReadiness::Preparing {
                prepared: 1,
                total: 4,
            },
            10,
        )
        .ok_or("a matched store ranks")?;
        assert!(ranking.units.is_empty());
        let warnings = serde_json::to_value(&ranking.warnings)?;
        assert_eq!(warnings[0]["code"], json!("semantic_index_preparing"));
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
        assert!(
            server.ranking(&params, &moved).await?.is_none(),
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
    /// The units the store ranks for whatever tree it is stamped with right now.
    ///
    /// A store is read under one revision, so a poll that watches for a pass to land reads
    /// the stamp first and asks that same tree for its rows.
    async fn ranked_now(
        index: &rift_search::SearchIndex,
        query: &str,
    ) -> TestResult<Vec<rift_search::RankedUnit>> {
        let Some(stamped) = index.tree_revision().await? else {
            return Ok(Vec::new());
        };
        match index.search(&stamped, query, 8).await? {
            RevisionScoped::Matched(ranked) => Ok(ranked),
            other => Err(format!("the store moved while it was being read: {other:?}").into()),
        }
    }

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
    async fn startup_populates_the_index_and_a_landed_change_repopulates_it() -> TestResult {
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
        assert!(
            !ranked_now(&index, "beacon").await?.is_empty(),
            "the run's first pass must leave the published unit set searchable"
        );

        let insert = json!({
            "anchor": "rift://symbol/rust/lib.rs/beacon",
            "position": "after",
            "body": "pub fn lantern() {}"
        });
        let params = serde_json::from_value(insert)?;
        let applied = server.insert_symbol(Parameters(params)).await?.0;
        assert!(
            matches!(applied, rift_protocol::change::ChangeResult::Applied { .. }),
            "the fixture insert must land: {applied:#?}"
        );
        // The change hands its publication to the population lane; a filesystem rebuild that
        // supersedes it hands the supervisor's instead. Poll under a bound rather than
        // racing which of the two passes ran.
        for _attempt in 0..SEARCH_TIER_ATTEMPTS_MAX {
            if !ranked_now(&index, "lantern").await?.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(SEARCH_TIER_POLL).await;
        }
        Err("the pass after a landed change never ranked the inserted declaration".into())
    }

    /// `insert_symbol` `after`, then `remove_symbol` on exactly what landed, returns the
    /// file's bytes to the original exactly - the same round trip
    /// `insert_symbol_after_then_remove_symbol_round_trips_to_the_original_bytes` proves
    /// against `ChangeService` directly, driven here through the tool methods the MCP
    /// surface advertises.
    #[tokio::test]
    async fn insert_symbol_after_then_remove_symbol_round_trips_through_the_mcp_surface()
    -> TestResult {
        let (directory, server) = fixture().await?;
        let original = fs::read_to_string(directory.path().join("lib.rs"))?;

        let insert = serde_json::from_value(json!({
            "anchor": "rift://symbol/rust/lib.rs/beacon",
            "position": "after",
            "body": "pub fn tail() {}"
        }))?;
        let inserted = server.insert_symbol(Parameters(insert)).await?.0;
        assert!(
            matches!(
                inserted,
                rift_protocol::change::ChangeResult::Applied { .. }
            ),
            "the insertion must land: {inserted:#?}"
        );

        let remove = serde_json::from_value(json!({
            "symbol": "rift://symbol/rust/lib.rs/tail",
            "force": false
        }))?;
        let removed = server.remove_symbol(Parameters(remove)).await?.0;
        assert!(
            matches!(removed, rift_protocol::change::ChangeResult::Applied { .. }),
            "the removal must land: {removed:#?}"
        );

        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written, original,
            "insert then remove through the MCP surface must return the original bytes"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn search_times_out_while_publication_is_stalled_like_every_other_read() -> TestResult {
        let (_directory, server) = fixture().await?;
        // Advance the observed epoch without an invalidation signal, so no rebuild ever
        // publishes a matching snapshot and the read must wait. `search` resolves the
        // published workspace exactly once (the TOCTOU fix in this review round), so a
        // stalled publication fails the whole request the same way every other current-tree
        // tool does, rather than merely degrading the lexical tier: `lexical_search_matches`
        // is never even reached with a snapshot to validate against.
        server
            .validation
            .observed_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let error = run_search(&server, "beacon")
            .await
            .expect_err("a stalled publication must miss the freshness deadline");
        assert!(
            error.message.contains("filesystem events behind the tree"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }
}
