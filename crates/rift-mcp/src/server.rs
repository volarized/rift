use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_core::{ProjectPath as CoreProjectPath, SourceVisibility};
use rift_index::{LexicalIndexLimits, PathChanges, WorkspaceIndexLimits, capture_digests};
use rift_protocol::change::{
    ChangeResult, ChangeSummary, GuaranteeEvidence, InsertSymbolParams, MoveFileParams,
    PatchParams, RemoveNodeParams, RemoveSymbolParams, RenameSymbolParams, ReplaceNodeParams,
    ReplaceSymbolParams,
};
use rift_protocol::configuration::{
    CommandHook, Duration as WireDuration, EngineConfiguration, SEARCH_BUSY_TIMEOUT_MS_MAX,
    SEARCH_POOL_SLOTS_MAX, SERVER_NUM_WORKERS_MAX, SearchConfiguration,
    SemanticSearchConfiguration, SemanticSource, ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_protocol::read::{
    DiagnosticCode, GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, ReadWarning,
    SearchParams, SearchResult,
};
use rift_search::{
    AcquisitionLimits, ModelSource, RankedUnit, RevisionScoped, SearchError, SearchIndex,
    SearchIndexLimits, SemanticReadiness,
};
use rift_server::{
    ChangeService, EnginePool, HookStatus, MoveResolution, ReadError, ReadFault, ReadService,
    RemoveResolution, RenameResolution, engine_change_diagnostics, plan_move, plan_remove_node,
    plan_remove_symbol, plan_rename, run_hooks,
};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::failure::{WireFailure, hook_failure_diagnostic, stale_snapshot_diagnostic};
use crate::validation::{
    ConfigurationState, INDEX_CAPTURE_ATTEMPTS_MAX, INDEX_FRESHNESS_TIMEOUT, IndexState,
    IndexSupervisor, IndexSupervisorContext, IndexValidation, LexicalLane, LexicalWrite,
    PendingWork, PopulationLane, PublishedWorkspace, RebuildOutcome, WorkspaceCandidate,
    build_workspace_candidate, configuration_fingerprint, finish_rebuild, initial_workspace,
    lexical_write, run_index_supervisor, workspace_watcher,
};

/// Overfetches ranked units beyond the caller's requested `limit` before the identifier and
/// ranked hit lists merge: the merge can collapse a ranked hit into an identifier-matched
/// one it duplicates, so asking for exactly `limit` ranked units would under-fill the final
/// page whenever duplicates exist.
const SEARCH_OVERFETCH_FACTOR: u32 = 4;

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

/// The engine pool held across requests, replaced when the accepted
/// `[engines]` tables change.
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
    /// Builds the hold with a pool for the startup `[engines]` tables.
    pub(crate) fn new(root: PathBuf, engines: BTreeMap<String, EngineConfiguration>) -> Self {
        let pool = Arc::new(EnginePool::new(&root, engines));
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
        engines: BTreeMap<String, EngineConfiguration>,
    ) -> Arc<EnginePool> {
        let mut held = self.pool.lock().await;
        if held.built_from(&engines) {
            return Arc::clone(&held);
        }
        let rebuilt = Arc::new(EnginePool::new(&self.root, engines));
        let replaced = std::mem::replace(&mut *held, Arc::clone(&rebuilt));
        drop(held);
        replaced.shutdown().await;
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

/// Opens the workspace's search database at `.rift/db`, creating `.rift` first.
///
/// The database is a derived index, rebuildable from the workspace tree at any time: an
/// open failure deletes the file and retries exactly once before this run gives up on the
/// search tier, rather than refusing to start the server over a file Rift itself can
/// always regenerate. The server serves identifier search alone when both attempts fail.
async fn open_search_index(root: &Path, limits: SearchIndexLimits) -> Option<Arc<SearchIndex>> {
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    if let Err(error) = tokio::fs::create_dir_all(&state_directory).await {
        tracing::warn!(
            component = "search",
            operation = "search.open",
            path = %state_directory.display(),
            error = %error,
            "could not create the workspace state directory; the server starts without the \
             search index"
        );
        return None;
    }
    let database_path = state_directory.join(WORKSPACE_DATABASE_FILE_NAME);
    match SearchIndex::open(&database_path, limits).await {
        Ok(index) => return Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(
                component = "search",
                operation = "search.open",
                path = %database_path.display(),
                error = %error,
                "search database failed to open; deleting and recreating it once"
            );
        }
    }
    let _ = tokio::fs::remove_file(&database_path).await;
    match SearchIndex::open(&database_path, limits).await {
        Ok(index) => Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(
                component = "search",
                operation = "search.open",
                path = %database_path.display(),
                error = %error,
                "search database failed to open after recreation; the server starts without \
                 the search index"
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
    let mut paths = Vec::with_capacity(summary.paths.len());
    for path in &summary.paths {
        let path = CoreProjectPath::new(&path.0).ok()?;
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

/// The candidate one landed change built, waiting for its lexical commit and its
/// publication.
///
/// The change lane is released before the commit awaits, so this carries everything the
/// publication check needs rather than reading shared state again.
struct AppliedCandidate {
    published: Arc<PublishedWorkspace>,
    write: LexicalWrite,
    work: PendingWork,
    epoch: u64,
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
        let root = absolute_root(root)?;
        let configuration_root = root.clone();
        let startup_configuration =
            tokio::task::spawn_blocking(move || ConfigurationState::accept(&configuration_root))
                .await
                .map_err(|error| ReadFault::task("configuration acceptance", error.to_string()))?;
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
        // The search database lives under the workspace's own `.rift` directory, so it
        // opens only once the workspace root itself is proven real by a successful initial
        // scan - never before, or a missing root would be silently fabricated by creating
        // `.rift` under it.
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
        let search_index = open_search_index(&root, search_limits).await;
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
        let engines = Arc::new(EngineHold::new(
            root.clone(),
            startup_configuration.engines_configuration(),
        ));
        Ok(Self {
            root: root.clone(),
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
            engines,
            tool_router: Self::tool_router(),
        })
    }

    /// The engine pool serving the currently published `[engines]` tables.
    ///
    /// The hold outlives rebuilds: a publication whose engine tables are
    /// unchanged reuses the running sessions, and one whose tables differ
    /// replaces the pool and shuts the old engines down.
    pub async fn engine_pool(&self) -> Arc<EnginePool> {
        let engines = self
            .published
            .read()
            .await
            .current
            .configuration
            .engines_configuration();
        self.engines.pool_for(engines).await
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
    /// carries the declaration and its source excerpt; `include_body: false` omits
    /// both. `include_history: true` adds each hit's version-control timeline,
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
            .search(
                published.reads.tree_revision(),
                query,
                self.fetch_limit(params),
            )
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))?;
        Ok(ranking_of(
            searched,
            index.readiness(),
            published.reads.file_count(),
        ))
    }

    /// How deep the search index is read for one request.
    ///
    /// The enforced ceiling identifier search itself would refuse past (`results_max`), so
    /// the index never overfetches beyond what a merge could ever keep; this also keeps the
    /// `u32` conversion within range without needing its saturating fallback in practice.
    fn fetch_limit(&self, params: &SearchParams) -> u32 {
        let results_max = u64::try_from(self.limits.results_max()).unwrap_or(u64::MAX);
        let requested_limit = params
            .limit
            .unwrap_or(rift_core::constants::SEARCH_RESULTS_DEFAULT as u64)
            .min(results_max);
        u32::try_from(requested_limit.saturating_mul(u64::from(SEARCH_OVERFETCH_FACTOR)))
            .unwrap_or(u32::MAX)
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
        let pool = self.engine_pool().await;
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
        let pool = self.engine_pool().await;
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
        let pool = self.engine_pool().await;
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
        let pool = self.engine_pool().await;
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

    /// Applies unified-diff hunks to workspace files atomically. Hunk
    /// context guards the change: a header's line numbers are hints and
    /// its line counts are read from the hunk's own body, as with
    /// `git apply`. A `/dev/null` header creates or deletes the file. The
    /// result carries one edit per hunk, spanning the bytes it replaced.
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
                capability: "revision reads (providers.history disabled)",
            })
            .tool_error(wire::ErrorPhase::Read));
        }
        let visibility = SourceVisibility::from(&configuration.source);
        let history = configuration.providers.history.clone();
        let root = self.root.clone();
        let limits = self.limits;
        self.blocking
            .run("revision workspace read", move || {
                let reads = ReadService::at_revision(&root, &rev, limits, &visibility, history)?;
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
            let capture = self
                .blocking
                .run("workspace fingerprint", move || {
                    let digests = capture_digests(&root, limits, &visibility, &text_inclusion)
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
    /// under the `[source]` policy this call already accepted. A rebuild
    /// failure after a landed change rides the result as a diagnostic
    /// rather than failing the call: the write happened, and the caller
    /// must not be told otherwise.
    ///
    /// Engine diagnostics attach here too, beside the hook verdicts' place
    /// in the post-apply flow: once the changed workspace published, each
    /// changed path whose engine advertises diagnostic pulls is pulled
    /// against the published bytes, and the findings ride the summary. An
    /// engine failure at this point is a warning on the summary, never a
    /// failed call - the change already applied.
    ///
    /// Search index population does not run here. The change hands its
    /// publication to the population lane and returns; awaiting that pass made
    /// every edit pay a whole lexical replacement plus the embedding of each
    /// new declaration before the caller heard that the write landed. A search
    /// issued before the pass lands is ranked by identifier matching alone and
    /// names the two tree revisions that disagree.
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
        let published_next = match (&mut result, outcome.candidate) {
            (Json(ChangeResult::Applied { summary }), Some(candidate)) => {
                self.publish_applied_change(candidate, summary).await
            }
            _ => None,
        };
        if let (Some(lane), Some(next)) = (self.population.as_ref(), published_next.as_ref()) {
            lane.request(Arc::clone(next));
        }
        if let Json(ChangeResult::Applied { summary }) = &mut result
            && let Some(snapshot) = self
                .diagnostics_snapshot(published_next.as_ref(), summary)
                .await
        {
            let engines = self.engine_pool().await;
            summary
                .diagnostics
                .extend(engine_change_diagnostics(&engines, &snapshot.reads, &summary.paths).await);
        }
        Ok(result)
    }

    /// Commits one landed change's lexical rows and publishes its snapshot, in that order.
    ///
    /// A request that captures this publication has to find the store already holding its
    /// tree, so the commit runs before the swap. The change lane is released by now, which
    /// is what lets the commit await; the publication check takes the observation lane
    /// again, so a candidate superseded while the transaction ran cannot become current.
    ///
    /// Every failure rides `summary` as a diagnostic rather than failing the call: the
    /// write already landed, and the caller must not be told otherwise. Current-tree reads
    /// then meet the recorded freshness failure until a later observation publishes.
    async fn publish_applied_change(
        &self,
        candidate: AppliedCandidate,
        summary: &mut ChangeSummary,
    ) -> Option<Arc<PublishedWorkspace>> {
        let AppliedCandidate {
            published,
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
                Some(published)
            }
            Ok(RebuildOutcome::Superseded) => None,
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

    /// One already-serialized change's outcome: the wire result, and the freshly published
    /// workspace exactly when this change's own rebuild published a new snapshot - absent
    /// for a refusal, and for a landed change whose rebuild only appended a diagnostic.
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
        let mut result = operation(&current.reads, changes)?;
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

    /// Rebuilds the snapshot after one landed change, running its hooks first, and returns
    /// the candidate for the caller to commit and publish. Every failure rides `summary` as
    /// a diagnostic instead of failing the call, since the write already landed.
    ///
    /// The change names the paths it wrote, so an ordinary change reparses exactly those
    /// files. A workspace that declares hooks reads every visible file instead: a hook runs
    /// after the change lands and may write anything, and the snapshot this call publishes
    /// has to carry whatever it wrote.
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
        Self::attach_hook_verdicts(root, &configuration.hooks, summary);
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
                    published,
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

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RiftMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read and edit the current workspace: get_symbol and search find \
                 declarations, nodes lists witnessed syntax nodes at a byte position, \
                 and replace_symbol, insert_symbol, replace_node, rename_symbol, \
                 move_file, and patch change code atomically behind verified \
                 preconditions.",
            )
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
        Duration as WireDuration, SearchConfiguration, SemanticSearchConfiguration, SemanticSource,
    };
    use rift_protocol::read::{GetSymbolResult, ReadWarning, SearchParams, SearchResult};
    use rift_search::{ModelSource, RevisionScoped, SemanticReadiness};
    use rift_server::{ChangeService, ConfigurationFault, ReadError, ReadFault};

    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;

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

    /// One `[engines]` table set naming `program`, built through serde so
    /// the optional keys carry their documented defaults.
    fn engine_tables(
        program: &str,
    ) -> std::collections::BTreeMap<String, rift_protocol::configuration::EngineConfiguration> {
        serde_json::from_value(json!({
            "ty": { "program": program, "languages": ["python"] }
        }))
        .expect("the engine table fixture deserializes")
    }

    #[tokio::test]
    async fn engine_hold_reuses_and_replaces_pools_by_table_equality() -> TestResult {
        let directory = tempfile::tempdir()?;
        let hold = super::EngineHold::new(directory.path().to_path_buf(), engine_tables("uvx"));
        let first = hold.pool_for(engine_tables("uvx")).await;
        let second = hold.pool_for(engine_tables("uvx")).await;
        assert!(
            Arc::ptr_eq(&first, &second),
            "unchanged tables reuse the held pool"
        );
        let replaced = hold.pool_for(engine_tables("pyright")).await;
        assert!(
            !Arc::ptr_eq(&first, &replaced),
            "changed tables replace the pool"
        );
        assert!(replaced.built_from(&engine_tables("pyright")));
        hold.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn engine_pool_serves_the_published_engine_tables() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(
            directory.path(),
            "[engines.ty]\nprogram = \"uvx\"\nlanguages = [\"python\"]\n",
        )?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        let pool = server.engine_pool().await;
        let language = |name: &str| rift_protocol::read::Language {
            name: name.to_owned(),
            dialect: None,
        };
        assert!(
            pool.engine_for(&language("python")).is_some(),
            "the accepted table serves its language"
        );
        assert!(
            pool.engine_for(&language("rust")).is_none(),
            "an unclaimed language answers no engine"
        );
        let again = server.engine_pool().await;
        assert!(
            Arc::ptr_eq(&pool, &again),
            "unchanged published tables reuse the held pool"
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
    async fn build_propagates_workspace_index_failure() {
        let directory = tempfile::tempdir().expect("fixture must exist");
        fs::write(directory.path().join("invalid.rs"), [0xff])
            .expect("invalid source fixture must write");
        let error = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())
            .await
            .expect_err("invalid source must fail");
        assert!(matches!(error.fault(), ReadFault::Index(_)));
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
        super::hermetic_workspace(directory.path(), "")?;
        let tight =
            WorkspaceIndexLimits::new(4, 60, 60, 4, 100).map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;

        let oversized = format!("pub fn oversized() {{}}\n{}", " ".repeat(80));
        fs::write(&path, oversized)?;
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

        let file_hit = results
            .iter()
            .find(|hit| hit["hit"]["target"] == "file" && hit["path"] == json!("guide.txt"))
            .ok_or_else(|| format!("guide.txt text-file hit missing: {structured:#}"))?;
        assert_eq!(file_hit["matched_by"], json!(["content"]));

        let symbol_hit = results
            .iter()
            .find(|hit| {
                hit["hit"]["target"] == "symbol" && hit["hit"]["symbol"]["name"] == "scale_value"
            })
            .ok_or_else(|| format!("scale_value doc-comment hit missing: {structured:#}"))?;
        assert!(
            symbol_hit["matched_by"]
                .as_array()
                .is_some_and(|fields| fields.contains(&json!("content"))),
            "the symbol hit must name content as a matched field: {symbol_hit:#}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Corrupt bytes fail `SQLite`'s file-format check deterministically, exercising the
    /// documented recreate-once path: the server still starts, and the recreated database
    /// serves lexical search once repopulated.
    #[tokio::test]
    async fn build_recovers_from_a_corrupt_lexical_database_by_recreating_it_once() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let guide_txt = "notes about the beacon subsystem\n";
        fs::write(directory.path().join("guide.txt"), guide_txt)?;
        let state_directory = directory.path().join(".rift");
        fs::create_dir_all(&state_directory)?;
        fs::write(state_directory.join("db"), b"not a sqlite database")?;
        super::hermetic_workspace(directory.path(), "")?;

        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())
            .await
            .map_err(|error| format!("corrupt database must not fail startup: {error:?}"))?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        // The recreated database is populated by the population lane, after `build`
        // returned. Poll for the hit under a bound; the helper's failure names the last
        // answer that still missed it.
        search_until_hit(client.peer(), "beacon subsystem", "guide.txt").await?;

        client.cancel().await?;
        server_task.await?;
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
    async fn applied_change_reports_failed_snapshot_rebuild_as_warning() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        super::hermetic_workspace(directory.path(), "")?;
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
                        paths: Vec::new(),
                        edits: Vec::new(),
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
                        paths: Vec::new(),
                        edits: Vec::new(),
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
        // `files_max=1` accepts the workspace's single Rust source file for `ReadService::build`,
        // which never counts `.gitignore` files. `WorkspaceSourcePolicy::build` re-walks for
        // `.gitignore` files specifically and counts each one against that same bound, so a
        // second `.gitignore` written by the change trips `TooManyFiles` there even though the
        // read-side rebuild already succeeded.
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
                        paths: vec![
                            rift_protocol::read::ProjectPath(".gitignore".to_owned()),
                            rift_protocol::read::ProjectPath("nested/.gitignore".to_owned()),
                        ],
                        edits: Vec::new(),
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
            error.message.contains("index freshness deadline elapsed"),
            "unexpected refusal: {error:?}"
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
        // A directory at the database path fails the initial open; `remove_file` cannot
        // remove a directory, so the recreate-once retry also fails against it unchanged -
        // this is also the deterministic way to drive the recreate-once arm itself.
        super::hermetic_workspace(directory.path(), "")?;
        fs::create_dir_all(directory.path().join(".rift/db"))?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(
            server.search_index.is_none(),
            "a database path occupied by a directory must exhaust the recreate-once retry \
             and still leave the server running without the search index"
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
            error.message.contains("index freshness deadline elapsed"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }
}
