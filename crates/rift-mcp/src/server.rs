use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{
    LexicalIndexLimits, LexicalMatch, LexicalSearchIndex, WorkspaceFingerprint,
    WorkspaceIndexLimits, WorkspaceSourcePolicy,
};
use rift_protocol::change::{
    ChangeResult, ChangeSummary, GuaranteeEvidence, InsertSymbolParams, PatchParams,
    ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::configuration::{
    CommandHook, SEARCH_BUSY_TIMEOUT_MS_MAX, SEARCH_POOL_SLOTS_MAX, SERVER_NUM_WORKERS_MAX,
    SearchConfiguration, ServerConfiguration, WorkspaceConfiguration,
};
use rift_protocol::error as wire;
use rift_protocol::read::{
    GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, SearchParams, SearchResult,
};
use rift_server::{ChangeService, HookStatus, ReadError, ReadFault, ReadService, run_hooks};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::{Mutex as AsyncMutex, RwLock, Semaphore};
use tracing::Instrument as _;

use crate::failure::{WireFailure, hook_failure_diagnostic, stale_snapshot_diagnostic};
use crate::validation::{
    ConfigurationState, INDEX_CAPTURE_ATTEMPTS_MAX, INDEX_FRESHNESS_TIMEOUT, IndexState,
    IndexSupervisor, IndexSupervisorContext, IndexValidation, PublishedWorkspace, RebuildOutcome,
    configuration_fingerprint, initial_workspace, populate_lexical, publish_rebuild,
    run_index_supervisor, workspace_watcher,
};

/// Overfetches lexical matches beyond the caller's requested `limit` before the identifier
/// and lexical hit lists merge: the merge can collapse a lexical hit into an
/// identifier-matched one it duplicates, so asking for exactly `limit` lexical matches would
/// under-fill the final page whenever duplicates exist.
const LEXICAL_OVERFETCH_FACTOR: u32 = 4;

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

/// Opens the workspace's lexical search database at `.rift/db`, creating `.rift` first.
///
/// The database is a derived index, rebuildable from the workspace tree at any time: an
/// open failure deletes the file and retries exactly once before this run gives up on the
/// lexical tier, rather than refusing to start the server over a file Rift itself can
/// always regenerate. The server serves identifier search alone when both attempts fail.
async fn open_lexical_index(
    root: &Path,
    limits: LexicalIndexLimits,
) -> Option<Arc<LexicalSearchIndex>> {
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    if let Err(error) = tokio::fs::create_dir_all(&state_directory).await {
        tracing::warn!(
            component = "search",
            operation = "lexical.open",
            path = %state_directory.display(),
            error = %error,
            "could not create the workspace state directory; the server starts without the \
             lexical search tier"
        );
        return None;
    }
    let database_path = state_directory.join(WORKSPACE_DATABASE_FILE_NAME);
    match LexicalSearchIndex::open(&database_path, limits).await {
        Ok(index) => return Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(
                component = "search",
                operation = "lexical.open",
                path = %database_path.display(),
                error = %error,
                "lexical search database failed to open; deleting and recreating it once"
            );
        }
    }
    let _ = tokio::fs::remove_file(&database_path).await;
    match LexicalSearchIndex::open(&database_path, limits).await {
        Ok(index) => Some(Arc::new(index)),
        Err(error) => {
            tracing::warn!(
                component = "search",
                operation = "lexical.open",
                path = %database_path.display(),
                error = %error,
                "lexical search database failed to open after recreation; the server starts \
                 without the lexical search tier"
            );
            None
        }
    }
}

/// Rust workspace MCP server: reads serve an immutable snapshot, changes
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
    /// The lexical search database, absent when it could not be opened at
    /// startup; `search` then serves identifier matching alone.
    lexical: Option<Arc<LexicalSearchIndex>>,
    tool_router: ToolRouter<Self>,
}

/// One already-serialized change's outcome, threaded out of the blocking executor so the
/// async `change` method can await lexical population against exactly the workspace this
/// change published, without a second, possibly-superseded read of shared state.
struct SerializedChange {
    result: Result<Json<ChangeResult>, ErrorData>,
    published: Option<Arc<PublishedWorkspace>>,
}

impl SerializedChange {
    /// A refusal or a diagnostic-only outcome: no fresh snapshot to populate.
    const fn wire(result: Result<Json<ChangeResult>, ErrorData>) -> Self {
        Self {
            result,
            published: None,
        }
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
        let root = root.to_path_buf();
        let configuration_root = root.clone();
        let startup_configuration =
            tokio::task::spawn_blocking(move || ConfigurationState::accept(&configuration_root))
                .await
                .map_err(|error| ReadFault::task("configuration acceptance", error.to_string()))?;
        let blocking =
            BlockingExecutor::for_configuration(&startup_configuration.server_configuration());
        let (validation, invalidations) = IndexValidation::new();
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
        let published = initial_workspace(&root, limits, &validation, &blocking).await?;
        // The lexical database lives under the workspace's own `.rift` directory, so it
        // opens only once the workspace root itself is proven real by a successful initial
        // scan - never before, or a missing root would be silently fabricated by creating
        // `.rift` under it.
        let lexical_limits = lexical_index_limits(&startup_configuration.search_configuration());
        let lexical = open_lexical_index(&root, lexical_limits).await;
        if let Some(lexical) = lexical.as_ref() {
            populate_lexical(lexical, &published).await;
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
                lexical: lexical.clone(),
            },
        ));
        let mut task = validation.task.lock().await;
        *task = Some(supervisor_task);
        drop(task);
        let supervisor_cancellation = Arc::new(validation.cancellation.clone().drop_guard());
        Ok(Self {
            root: root.clone(),
            limits,
            published,
            validation,
            supervisor_cancellation,
            changes: Arc::new(ChangeService::new(&root)),
            change_lane,
            blocking,
            lexical,
            tool_router: Self::tool_router(),
        })
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

    /// Searches indexed Rust declarations and source lines by lexical `query`, merged with
    /// full-text matches from included `[search.text]` files and declaration bodies. `rev`
    /// searches a version-control revision instead of the current tree. Use `get_symbol`
    /// when the declaration name is known.
    ///
    /// For a current-tree search, the published workspace is resolved exactly once and
    /// threaded through both the lexical tier's revision check and the executed
    /// `ReadService::search` call: a concurrent rebuild between two separate resolutions
    /// could otherwise validate lexical matches against one snapshot and merge them into
    /// results computed from another.
    #[tool]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let Some(rev) = params.rev.clone() else {
            let published = self.published_workspace(wire::ErrorPhase::Read).await?;
            let lexical_matches = self.lexical_search_matches(&params, &published).await?;
            return self
                .current_tree_read(&published, move |reads| {
                    reads.search(&params, &lexical_matches)
                })
                .await;
        };
        // The lexical tier only ever holds the current tree, so a revision-addressed
        // search never consults it.
        self.read_at(Some(rev), move |reads| reads.search(&params, &[]))
            .await
    }

    /// Runs the lexical search-index tier for one search request against `published` -
    /// the exact snapshot the caller also runs `ReadService::search` against, never a
    /// separately resolved one - when the tier is available and its stamped tree revision
    /// still matches `published`'s. A revision mismatch or an absent handle answers with no
    /// lexical matches, so identifier search proceeds alone rather than serving a possibly
    /// stale tier. A query-term limit the adapter refuses surfaces as this request's own
    /// `limit_exceeded` error, never a silent degrade.
    async fn lexical_search_matches(
        &self,
        params: &SearchParams,
        published: &PublishedWorkspace,
    ) -> Result<Vec<LexicalMatch>, ErrorData> {
        let Some(lexical) = self.lexical.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(query) = params.query.as_deref().filter(|query| !query.is_empty()) else {
            return Ok(Vec::new());
        };
        let Ok(current_revision) = lexical.tree_revision().await else {
            return Ok(Vec::new());
        };
        if current_revision.as_deref() != Some(published.reads.tree_revision()) {
            return Ok(Vec::new());
        }
        // The enforced ceiling identifier search itself would refuse past (`results_max`),
        // so the lexical tier never overfetches beyond what a merge could ever keep; this
        // also keeps the later `u32` conversion within range without needing its
        // saturating fallback in practice.
        let results_max = u64::try_from(self.limits.results_max()).unwrap_or(u64::MAX);
        let requested_limit = params
            .limit
            .unwrap_or(rift_core::constants::SEARCH_RESULTS_DEFAULT as u64)
            .min(results_max);
        let fetch_limit =
            u32::try_from(requested_limit.saturating_mul(u64::from(LEXICAL_OVERFETCH_FACTOR)))
                .unwrap_or(u32::MAX);
        lexical
            .search(query, fetch_limit)
            .await
            .map_err(|error| error.tool_error(wire::ErrorPhase::Read))
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
        let root = self.root.clone();
        let limits = self.limits;
        self.blocking
            .run("revision workspace read", move || {
                let reads = ReadService::at_revision(&root, &rev, limits, &visibility)?;
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
            let text_inclusion = current.configuration.text_inclusion();
            let capture = self
                .blocking
                .run("workspace fingerprint", move || {
                    let fingerprint =
                        WorkspaceFingerprint::capture(&root, limits, &visibility, &text_inclusion)
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
                    let _ = self.validation.observe();
                    return Err(error.tool_error(phase));
                }
            };
            let configuration_matches =
                current.configuration.fingerprint == configuration_fingerprint;
            let epoch_matches = current.epoch == self.validation.observed_epoch();
            if fingerprint == current.fingerprint && configuration_matches && epoch_matches {
                current.configuration.accepted(phase)?;
                return Ok(current);
            }
            self.validation
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
        if let (Some(lexical), Some(next)) = (self.lexical.as_ref(), outcome.published.as_ref()) {
            populate_lexical(lexical, next).await;
        }
        outcome.result
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
        let published_next = if let ChangeResult::Applied { summary } = &mut result {
            Self::rebuild_after_applied_change(
                root,
                limits,
                published,
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
            published: published_next,
        })
    }

    /// Rebuilds and publishes the snapshot after one landed change, running its hooks
    /// first. Returns the freshly published workspace only when publication actually
    /// happened; every failure rides `summary` as a diagnostic instead of failing the call,
    /// since the write already landed.
    fn rebuild_after_applied_change(
        root: &Path,
        limits: WorkspaceIndexLimits,
        published: &RwLock<IndexState>,
        validation: &IndexValidation,
        configuration: &WorkspaceConfiguration,
        current: &PublishedWorkspace,
        summary: &mut ChangeSummary,
    ) -> Option<Arc<PublishedWorkspace>> {
        let epoch = match validation.observe() {
            Ok(epoch) => epoch,
            Err(error) => {
                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                return None;
            }
        };
        Self::attach_hook_verdicts(root, &configuration.hooks, summary);
        let visibility = SourceVisibility::from(&configuration.source);
        let text_inclusion = TextFileInclusion::from(&configuration.search);
        let rebuilt = match ReadService::build(root, limits, &visibility, &text_inclusion) {
            Ok(rebuilt) => rebuilt,
            Err(error) => {
                summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                return None;
            }
        };
        let fingerprint = rebuilt.workspace_fingerprint().clone();
        let source_policy =
            match WorkspaceSourcePolicy::build(root, limits, &visibility, &text_inclusion) {
                Ok(policy) => Arc::new(policy),
                Err(error) => {
                    let error = ReadError::from(ReadFault::Index(error));
                    summary.diagnostics.push(stale_snapshot_diagnostic(&error));
                    return None;
                }
            };
        if current.configuration.fingerprint != configuration_fingerprint(root) {
            let error = ReadFault::unavailable(
                "workspace change",
                "configuration changed during snapshot rebuild",
            );
            let _ = validation.observe();
            summary.diagnostics.push(stale_snapshot_diagnostic(&error));
            return None;
        }
        let next = Arc::new(PublishedWorkspace {
            reads: Arc::new(rebuilt),
            configuration: current.configuration.clone(),
            fingerprint,
            source_policy,
            epoch,
        });
        if publish_rebuild(published, validation, Arc::clone(&next)) == RebuildOutcome::Published {
            tracing::info!(
                component = "index",
                operation = "index.publish",
                trigger = "rift_change",
                epoch,
                "index snapshot published"
            );
            validation.changed.notify_waiters();
            return Some(next);
        }
        None
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
                 and replace_symbol, insert_symbol, replace_node, and patch change \
                 code atomically behind verified preconditions.",
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

    use rift_protocol::read::{GetSymbolResult, SearchParams, SearchResult};
    use rift_server::{ChangeService, ConfigurationFault, ReadError, ReadFault};

    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;

    use super::{BlockingExecutor, ChangeLane, Parameters, RiftMcp};
    use crate::validation::{
        ConfigurationState, IndexState, IndexValidation, PublishedWorkspace, WorkspaceCandidate,
        build_workspace_candidate, configuration_fingerprint, record_rebuild_failure,
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
        match build_workspace_candidate(root, WorkspaceIndexLimits::default(), epoch)? {
            WorkspaceCandidate::Stable(candidate) => Ok(candidate),
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
        let (validation, _invalidations) = IndexValidation::new();
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
        assert!(outcome.published.is_none());
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
    /// comment supplies just the word "units" and `guide.md` supplies "replace" and "all",
    /// so only the lexical search-index tier's per-term matching can produce either hit.
    #[tokio::test]
    async fn client_search_merges_lexical_symbol_and_text_file_hits() -> TestResult {
        let directory = tempfile::tempdir()?;
        let lib_rs = "/// Converts a raw measurement into base units.\npub fn scale_value(value: f64) -> f64 {\n    value * 2.0\n}\n";
        fs::write(directory.path().join("lib.rs"), lib_rs)?;
        let guide_md = "# Guide\n\nThis document explains how to replace all safely.\n";
        fs::write(directory.path().join("guide.md"), guide_md)?;
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

        let search = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": "replace all units"}))?),
            )
            .await?;
        let structured = search
            .structured_content
            .ok_or("search must return structured content")?;
        let results = structured["results"]
            .as_array()
            .ok_or("results must be an array")?;

        let file_hit = results
            .iter()
            .find(|hit| hit["hit"]["target"] == "file" && hit["path"] == json!("guide.md"))
            .ok_or_else(|| format!("guide.md text-file hit missing: {structured:#}"))?;
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
        let guide_md = "notes about the beacon subsystem\n";
        fs::write(directory.path().join("guide.md"), guide_md)?;
        let state_directory = directory.path().join(".rift");
        fs::create_dir_all(&state_directory)?;
        fs::write(state_directory.join("db"), b"not a sqlite database")?;

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

        let search = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": "beacon subsystem"}))?),
            )
            .await?;
        let structured = search
            .structured_content
            .ok_or("search must return structured content")?;
        let results = structured["results"]
            .as_array()
            .ok_or("results must be an array")?;
        assert!(
            results.iter().any(|hit| hit["path"] == json!("guide.md")),
            "the recreated lexical database must be populated and serve results: {structured:#}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// A `.md` file created after startup exists only because the change-applied rebuild
    /// path repopulates the lexical tier; the initial population at `build` never saw it.
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

        let diff = "--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1 @@\n+the migration guide covers replacing every legacy unit\n";
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

        // The change's own writes wake the watcher, whose rebuild republishes a newer
        // revision; until the supervisor repopulates, the revision guard serves
        // identifier-only results. Retry within a bound instead of asserting the first
        // answer, because that degraded window is advertised behavior.
        let repopulation_attempts_max = 50;
        let mut lexical_hit_observed = false;
        let mut last_answer = json!(null);
        for _ in 0..repopulation_attempts_max {
            let search = client
                .call_tool(
                    CallToolRequestParams::new("search")
                        .with_arguments(arguments(&json!({"query": "replacing legacy unit"}))?),
                )
                .await?;
            let structured = search
                .structured_content
                .ok_or("search must return structured content")?;
            let results = structured["results"]
                .as_array()
                .ok_or("results must be an array")?;
            if results.iter().any(|hit| hit["path"] == json!("notes.md")) {
                lexical_hit_observed = true;
                break;
            }
            last_answer = structured;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            lexical_hit_observed,
            "the change-applied rebuild must repopulate the lexical tier with the new file: \
             {last_answer:#}"
        );

        client.cancel().await?;
        server_task.await?;
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
        let (validation, _receiver) = IndexValidation::new();
        validation
            .observe()
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
        assert!(outcome.published.is_none());
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
        let (validation, receiver) = IndexValidation::new();
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
                        id: ChangeId("chg_abcdefghijklmnopqrstuvwxyz".to_owned()),
                        paths: Vec::new(),
                        edits: Vec::new(),
                        diagnostics: Vec::new(),
                        guarantees: Vec::new(),
                    },
                })
            },
        )?;
        assert!(outcome.published.is_none());
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
        let (validation, _receiver) = IndexValidation::new();
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
                        id: ChangeId("chg_abcdefghijklmnopqrstuvwxyz".to_owned()),
                        paths: Vec::new(),
                        edits: Vec::new(),
                        diagnostics: Vec::new(),
                        guarantees: Vec::new(),
                    },
                })
            },
        )?;
        assert!(outcome.published.is_none());
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
        let (validation, _receiver) = IndexValidation::new();
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
                        id: ChangeId("chg_abcdefghijklmnopqrstuvwxyz".to_owned()),
                        paths: Vec::new(),
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
            .observe()
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
    async fn build_disables_lexical_tier_when_rift_state_path_is_a_file() -> TestResult {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(std::io::sink)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        // A regular file already occupies `.rift`, so `create_dir_all` cannot make the
        // state directory the lexical database needs.
        fs::write(directory.path().join(".rift"), b"not a directory")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(
            server.lexical.is_none(),
            "a blocked state directory must degrade to no lexical tier, not fail startup"
        );
        Ok(())
    }

    #[tokio::test]
    async fn build_disables_lexical_tier_when_database_path_is_a_directory() -> TestResult {
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
        fs::create_dir_all(directory.path().join(".rift/db"))?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
        assert!(
            server.lexical.is_none(),
            "a database path occupied by a directory must exhaust the recreate-once retry \
             and still leave the server running without the lexical tier"
        );

        // With no lexical tier, identifier search still serves results rather than failing.
        let result = run_search(&server, "beacon").await?;
        assert!(
            !result.results.is_empty(),
            "identifier search must still serve results without the lexical tier"
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
            error.message.contains("index freshness deadline elapsed"),
            "unexpected refusal: {error:?}"
        );
        Ok(())
    }
}
