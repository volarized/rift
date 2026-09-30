//! The history task: fills the repository's history store in the background.
//!
//! The server opens its store once at startup, attaches the store to every
//! snapshot it serves, and spawns one task that fills it under the
//! `[providers.history]` strategy. No request waits on the task: a timeline
//! answers from what the store holds, and a read that finds the store behind
//! its served revision wakes the task. The task plans newest first, parses on
//! the runtime's blocking threads - outside the server's worker pool, so it
//! never takes a worker a request needs - and commits bounded batches. Before
//! each batch it waits a bounded time for the server to run no request, then
//! parses at the operator's CPU share, pausing after each commit.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_core::{ErrorCode, ErrorName};
use rift_history_store::{HistoryStore, StoreFiller, StoreLocation};
use rift_protocol::configuration::HistoryConfiguration;
use rift_server::{
    AnalyzedCommit, FillPlan, FillProgress, HistoryAnalysis, PendingCommit, ReadError,
    StoredHistory,
};
use sha2::{Digest as _, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::http::IdleTracker;
use crate::validation::{ConfigurationState, derivation_revision};

/// How long the task waits for the server to run no request before a batch.
/// Past it the batch runs anyway at the CPU share, so calls that overlap
/// without a pause never starve the fill.
const IDLE_WAIT: Duration = Duration::from_secs(2);

/// How long the task rests between plans when no read asks for a fill: a
/// worktree's `HEAD` moves without any event the server watches.
const REPLAN_INTERVAL: Duration = Duration::from_secs(60);

/// A test hook run on the blocking thread before each commit's analysis.
pub(crate) type AnalysisGate = Arc<dyn Fn() + Send + Sync>;

/// The batch bounds and the CPU share one fill runs under, from
/// `[providers.history]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FillBounds {
    commits_max: usize,
    bytes_max: u64,
    cpu_share: f64,
    idle_wait: Duration,
}

impl FillBounds {
    /// The bounds `history` states, with the fixed wait before a batch.
    pub(crate) fn from_configuration(history: &HistoryConfiguration) -> Self {
        Self {
            commits_max: usize::try_from(history.batch_commits).unwrap_or(usize::MAX),
            bytes_max: history.batch_size.bytes(),
            cpu_share: history.cpu_share,
            idle_wait: IDLE_WAIT,
        }
    }

    /// Whether a batch holding `commits` commits and `bytes` parsed bytes
    /// takes one more commit that parsed `next` bytes. A batch always takes
    /// its first commit, whatever it parsed, and takes another only while it
    /// stays within both bounds.
    pub(crate) const fn admits(&self, commits: usize, bytes: u64, next: u64) -> bool {
        commits == 0 || (commits < self.commits_max && bytes.saturating_add(next) <= self.bytes_max)
    }

    /// The pause after `spent` parsing that holds the CPU share: parsing for
    /// `spent` then resting `spent * (1 - share) / share` keeps the share of
    /// one core the operator set.
    pub(crate) fn pause_after(&self, spent: Duration) -> Duration {
        let rest = (1.0 - self.cpu_share) / self.cpu_share;
        Duration::try_from_secs_f64(spent.as_secs_f64() * rest).unwrap_or(Duration::MAX)
    }
}

/// The derivation revision one store file is keyed on: what decides the
/// lexical rows - the build, by its product version, the corpus shape, the
/// index-owned tables - beside the strategy and the releases it selects, since
/// a commit analyzed under one strategy is compared with another commit than
/// under the other.
pub(crate) fn store_revision(product_version: &str, configuration: &ConfigurationState) -> String {
    let history = configuration.history_configuration();
    let mut hasher = Sha256::new();
    hasher.update(derivation_revision(product_version, configuration).as_bytes());
    hasher.update([0]);
    hasher.update(format!("{:?}", history.strategy).as_bytes());
    for release in &history.releases {
        hasher.update([0]);
        hasher.update(release.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// One server's history lane: the store handle every snapshot attaches.
#[derive(Clone, Debug)]
pub(crate) struct HistoryLane {
    stored: StoredHistory,
}

impl HistoryLane {
    /// The store handle a snapshot attaches.
    pub(crate) const fn stored(&self) -> &StoredHistory {
        &self.stored
    }

    /// Opens the history store once and spawns the task that fills it, with
    /// `gate` run before each commit's analysis when a test holds the task.
    ///
    /// `None` when `[providers.history]` is off, the workspace has no
    /// repository, or no folder takes the store; timelines then walk git per
    /// request, and each case other than an unversioned workspace is logged.
    /// When the common git directory refuses the store and the worktree's
    /// state directory takes it, the refusal is logged once, here.
    pub(crate) async fn start(
        root: &Path,
        configuration: &ConfigurationState,
        product_version: &str,
        (activity, cancellation, gate): (Arc<IdleTracker>, CancellationToken, Option<AnalysisGate>),
    ) -> Option<Self> {
        let history = configuration.history_configuration();
        if !history.enabled || !configuration.is_accepted() {
            return None;
        }
        let opening = OpenedStore::open(root, configuration, &history, product_version);
        let opened = tokio::task::spawn_blocking(opening).await.ok()??;
        let wake = Arc::new(Notify::new());
        let signal = Arc::clone(&wake);
        let stored = StoredHistory::new(
            opened.store.reader(),
            history.strategy,
            Arc::new(move || signal.notify_one()),
        );
        let progress = stored.progress();
        if let Some(plan) = &opened.plan {
            progress.record_plan(plan.keep().len(), plan.pending().len());
        }
        let task = HistoryTask {
            store: Arc::new(opened.store),
            analysis: Arc::new(opened.analysis),
            progress,
            bounds: FillBounds::from_configuration(&history),
            activity,
            warned_tags: HashSet::new(),
            warned_past_bound: 0,
            gate,
        };
        tokio::spawn(task.run(wake, cancellation));
        Some(Self { stored })
    }
}

/// The store and the analysis one lane opened, with the fill plan it met the
/// store with.
struct OpenedStore {
    store: HistoryStore,
    analysis: HistoryAnalysis,
    plan: Option<FillPlan>,
}

impl OpenedStore {
    /// The blocking open: the repository, the policy, the store, a sweep of
    /// the store files no live server holds, and a first plan against what the
    /// store holds, so the first read already knows how far the fill has got.
    fn open(
        root: &Path,
        configuration: &ConfigurationState,
        history: &HistoryConfiguration,
        product_version: &str,
    ) -> impl FnOnce() -> Option<Self> + Send + 'static {
        let root = root.to_path_buf();
        let history = history.clone();
        let revision = store_revision(product_version, configuration);
        let policy = (
            configuration.source_visibility(),
            configuration.text_inclusion(),
            configuration.language_file_selections(),
        );
        let syntax = configuration
            .index_limits(rift_index::WorkspaceIndexLimits::default())
            .map(rift_index::WorkspaceIndexLimits::syntax);
        move || {
            let syntax = syntax
                .map_err(|error| open_failed("history.open", &error))
                .ok()?;
            let analysis =
                HistoryAnalysis::open(&root, &history, (&policy.0, &policy.1, &policy.2), syntax)
                    .map_err(|error| open_failed("history.open", &error))
                    .ok()?;
            let location = StoreLocation::new(&analysis.common_directory(), &revision)
                .or_worktree(&root.join(RIFT_STATE_DIRECTORY));
            let store = HistoryStore::open(&location)
                .map_err(|error| {
                    tracing::warn!(
                        component = "history",
                        operation = "history.open",
                        error = %error,
                        "the history store could not open; symbol history walks git per request"
                    );
                })
                .ok()?;
            record_fallback(&store);
            sweep(&store);
            let plan = observed_plan(&store, &analysis)
                .map_err(|error| fill_failed(&error))
                .ok();
            Some(Self {
                store,
                analysis,
                plan,
            })
        }
    }
}

/// Plans against what the store holds through a read connection, which
/// needs no fill lock: the view of a server whose task another server's
/// task has kept from filling.
fn observed_plan(store: &HistoryStore, analysis: &HistoryAnalysis) -> Result<FillPlan, String> {
    let held = store
        .reader()
        .connect()
        .and_then(|reads| reads.held())
        .map_err(|error| error.to_string())?;
    analysis.plan(&held).map_err(|error| error.to_string())
}

/// Logs why the history lane did not start. A capability the workspace lacks -
/// no git repository versions it - is the ordinary case for a folder outside
/// git, and logs at debug level.
fn open_failed(operation: &'static str, error: &ReadError) {
    let unversioned = error.name() == ErrorName::Wire(ErrorCode::CapabilityUnavailable);
    if unversioned {
        tracing::debug!(
            component = "history",
            operation,
            "the workspace has no git repository, so no history store opens"
        );
    } else {
        tracing::warn!(
            component = "history",
            operation,
            error = %error,
            "the history store could not open; symbol history walks git per request"
        );
    }
}

/// Records, once per server, that the store sits in the worktree's state
/// directory because the common git directory refused it.
fn record_fallback(store: &HistoryStore) {
    let Some(fallback) = store.worktree_fallback() else {
        return;
    };
    let refused = fallback.refused().display().to_string();
    let folder = store.location().folder().display().to_string();
    let cause = fallback.cause();
    tracing::warn!(
        component = "history",
        operation = "history.open",
        refused = refused.as_str(),
        folder = folder.as_str(),
        error = %cause,
        "the common git directory refused the history store, so it sits in the worktree's \
         state directory and other worktrees of this repository analyze their commits again"
    );
}

/// Deletes the store files no live server holds, logging each released
/// revision the sweep could not take: a live lock it could not open or try, or
/// a deletion the filesystem refused.
fn sweep(store: &HistoryStore) {
    match store.sweep() {
        Ok(swept) => {
            for failure in swept.failures() {
                tracing::warn!(
                    component = "history",
                    operation = "history.sweep",
                    error = %failure,
                    "a released history store revision could not be swept"
                );
            }
        }
        Err(error) => tracing::warn!(
            component = "history",
            operation = "history.sweep",
            error = %error,
            "the history store folder could not be swept"
        ),
    }
}

/// The task's state across fills. The fill lock's write connection is not
/// `Sync`, so the task owns it apart from this state, in [`Self::run`].
struct HistoryTask {
    store: Arc<HistoryStore>,
    analysis: Arc<HistoryAnalysis>,
    progress: Arc<FillProgress>,
    bounds: FillBounds,
    activity: Arc<IdleTracker>,
    warned_tags: HashSet<String>,
    warned_past_bound: usize,
    gate: Option<AnalysisGate>,
}

impl HistoryTask {
    /// Fills, then rests until a read wakes the task, the replan interval
    /// passes, or the server stops.
    ///
    /// # Cancel safety
    ///
    /// The task ends at its next await once `cancellation` fires; a commit
    /// already parsing on a blocking thread stops at its next changed path.
    async fn run(mut self, wake: Arc<Notify>, cancellation: CancellationToken) {
        let mut filler: Option<StoreFiller> = None;
        loop {
            let fill = self.fill(filler.take(), &cancellation);
            tokio::select! {
                () = cancellation.cancelled() => return,
                held = fill => filler = held,
            }
            tokio::select! {
                () = cancellation.cancelled() => return,
                () = wake.notified() => {}
                () = tokio::time::sleep(REPLAN_INTERVAL) => {}
            }
        }
    }

    /// One fill: takes the fill lock unless another server holds it, plans,
    /// analyzes and writes the owed commits in batches, then trims what no
    /// window keeps. A failure ends the fill and is logged; the next fill
    /// plans again from what the store holds. Answers the fill lock's
    /// connection, which later fills keep using.
    async fn fill(
        &mut self,
        held: Option<StoreFiller>,
        cancellation: &CancellationToken,
    ) -> Option<StoreFiller> {
        let filler = if let Some(filler) = held {
            filler
        } else if let Some(filler) = self.take_filler().await {
            filler
        } else {
            self.observe().await;
            return None;
        };
        let analysis = Arc::clone(&self.analysis);
        let planned = blocking(move || {
            let plan = rift_core::traced!(component = "history", operation = "history.plan", {
                filler
                    .held()
                    .map_err(|error| error.to_string())
                    .and_then(|held| analysis.plan(&held).map_err(|error| error.to_string()))
            });
            (filler, plan)
        })
        .await;
        let (filler, plan) = planned?;
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                fill_failed(&error);
                return Some(filler);
            }
        };
        self.record_releases(&plan);
        self.progress
            .record_plan(plan.keep().len(), plan.pending().len());
        self.fill_planned(filler, &plan, cancellation).await
    }

    /// Records how far another server's fill has got, planning through a read
    /// connection while that server's task holds the fill lock.
    async fn observe(&self) {
        let store = Arc::clone(&self.store);
        let analysis = Arc::clone(&self.analysis);
        match blocking(move || observed_plan(&store, &analysis)).await {
            Some(Ok(plan)) => self
                .progress
                .record_plan(plan.keep().len(), plan.pending().len()),
            Some(Err(error)) => fill_failed(&error),
            None => {}
        }
    }

    /// The fill lock, taken on the first fill that finds it free and held
    /// from then on; `None` while another server's task fills the store.
    async fn take_filler(&self) -> Option<StoreFiller> {
        let store = Arc::clone(&self.store);
        match blocking(move || store.filler()).await? {
            Ok(filler) => filler,
            Err(error) => {
                fill_failed(&error.to_string());
                None
            }
        }
    }

    /// Analyzes and writes every pending commit of `plan` in bounded batches,
    /// then trims. Answers the filler back, or `None` when a blocking step
    /// panicked with it.
    ///
    /// Each batch records its start with the pending commits the plan has not
    /// analyzed yet. A span reaches the log only when it closes, so that start
    /// record is what shows a batch in flight.
    async fn fill_planned(
        &self,
        mut filler: StoreFiller,
        plan: &FillPlan,
        cancellation: &CancellationToken,
    ) -> Option<StoreFiller> {
        let mut pending = plan.pending().iter();
        let mut carried: Option<AnalyzedCommit> = None;
        loop {
            if cancellation.is_cancelled() {
                return Some(filler);
            }
            let settled = self.activity.settled(self.bounds.idle_wait);
            let stopped =
                rift_core::traced_async!(component = "history", operation = "history.idle_wait", {
                    tokio::select! {
                        () = cancellation.cancelled() => true,
                        _ = settled => false,
                    }
                })
                .await;
            if stopped {
                return Some(filler);
            }
            let first = carried.take();
            let remaining = &mut pending;
            let batch =
                rift_core::traced_async!(component = "history", operation = "history.batch", {
                    tracing::debug!(
                        component = "history",
                        operation = "history.batch",
                        phase = "start",
                        pending = remaining.len(),
                        "history batch started"
                    );
                    self.fill_batch(filler, first, remaining, cancellation)
                        .await
                })
                .await?;
            filler = batch.filler;
            carried = batch.carried;
            match batch.outcome {
                BatchOutcome::Written => {}
                BatchOutcome::Drained => break,
                BatchOutcome::Failed => return Some(filler),
            }
        }
        let keep: BTreeSet<String> = plan.keep().clone();
        let (filler, trimmed) = blocking(move || {
            let trimmed = filler.trim(&keep);
            (filler, trimmed)
        })
        .await?;
        if let Err(error) = trimmed {
            fill_failed(&error.to_string());
        }
        Some(filler)
    }

    /// Analyzes and writes one batch, starting at the commit `carried` over from the last
    /// one. Answers the filler back with the commit that overflowed the batch, or `None`
    /// when a blocking step panicked with the filler.
    async fn fill_batch(
        &self,
        filler: StoreFiller,
        carried: Option<AnalyzedCommit>,
        pending: &mut std::slice::Iter<'_, PendingCommit>,
        cancellation: &CancellationToken,
    ) -> Option<FilledBatch> {
        let (batch, carried) = self.next_batch(carried, pending, cancellation).await;
        if batch.is_empty() {
            return Some(FilledBatch {
                filler,
                carried,
                outcome: BatchOutcome::Drained,
            });
        }
        let records: Vec<_> = batch.into_iter().map(AnalyzedCommit::into_record).collect();
        let written_commits = records.len();
        let parent = tracing::Span::current();
        let (filler, written) = blocking(move || {
            let mut filler = filler;
            let written = rift_core::traced!(
                parent: &parent,
                component = "history",
                operation = "history.write",
                { filler.write_batch(&records) }
            );
            (filler, written)
        })
        .await?;
        let outcome = match written {
            Ok(()) => {
                self.progress.record_written(written_commits);
                BatchOutcome::Written
            }
            Err(error) => {
                fill_failed(&error.to_string());
                BatchOutcome::Failed
            }
        };
        Some(FilledBatch {
            filler,
            carried,
            outcome,
        })
    }

    /// Analyzes pending commits into one batch: the commit `carried` over
    /// from the last batch first, then more while the batch bounds admit
    /// them. The commit that overflows the bounds is answered back, to open
    /// the next batch. After each commit the task pauses to hold the CPU
    /// share.
    async fn next_batch(
        &self,
        carried: Option<AnalyzedCommit>,
        pending: &mut std::slice::Iter<'_, PendingCommit>,
        cancellation: &CancellationToken,
    ) -> (Vec<AnalyzedCommit>, Option<AnalyzedCommit>) {
        let mut batch: Vec<AnalyzedCommit> = carried.into_iter().collect();
        let mut bytes: u64 = batch.iter().map(AnalyzedCommit::parsed_bytes).sum();
        for next in pending.by_ref() {
            let started = Instant::now();
            let Some(analyzed) = self.analyze(next, cancellation).await else {
                break;
            };
            let pause = self.bounds.pause_after(started.elapsed());
            if !self
                .bounds
                .admits(batch.len(), bytes, analyzed.parsed_bytes())
            {
                self.rest(pause, cancellation).await;
                return (batch, Some(analyzed));
            }
            bytes = bytes.saturating_add(analyzed.parsed_bytes());
            batch.push(analyzed);
            self.rest(pause, cancellation).await;
        }
        (batch, None)
    }

    /// Analyzes one commit on a blocking thread; `None` when the server
    /// stopped, the analysis failed, or its thread panicked.
    async fn analyze(
        &self,
        pending: &PendingCommit,
        cancellation: &CancellationToken,
    ) -> Option<AnalyzedCommit> {
        let analysis = Arc::clone(&self.analysis);
        let pending = pending.clone();
        let stop = cancellation.clone();
        let gate = self.gate.clone();
        let parent = tracing::Span::current();
        let analyzed = blocking(move || {
            if let Some(gate) = gate {
                gate();
            }
            rift_core::traced!(
                parent: &parent,
                component = "history",
                operation = "history.analyze",
                { analysis.analyze(&pending, &|| stop.is_cancelled()) }
            )
        })
        .await?;
        match analyzed {
            Ok(analyzed) => analyzed,
            Err(error) => {
                fill_failed(&error.to_string());
                None
            }
        }
    }

    /// Rests `pause`, or until the server stops.
    async fn rest(&self, pause: Duration, cancellation: &CancellationToken) {
        tokio::select! {
            () = cancellation.cancelled() => {}
            () = tokio::time::sleep(pause) => {}
        }
    }

    /// Logs each tag a release pattern matched whose name holds no version,
    /// once per tag, and the count of releases the bound left out each time
    /// that count changes.
    fn record_releases(&mut self, plan: &FillPlan) {
        for tag in plan.unversioned() {
            if !self.warned_tags.insert(tag.name.clone()) {
                continue;
            }
            tracing::warn!(
                component = "history",
                operation = "history.plan",
                tag = tag.name.as_str(),
                remainder = tag.remainder.as_str(),
                "a release tag holds no version once the pattern's literal text is stripped, \
                 so the history store leaves it out"
            );
        }
        let past_bound = plan.releases_past_bound();
        if past_bound > 0 && past_bound != self.warned_past_bound {
            self.warned_past_bound = past_bound;
            tracing::warn!(
                component = "history",
                operation = "history.plan",
                releases = past_bound,
                "the release patterns select more releases than max_revisions, so the history \
                 store keeps the newest"
            );
        }
    }
}

/// One written batch: the filler back, the commit that overflowed the batch bounds, and
/// what the batch did.
struct FilledBatch {
    filler: StoreFiller,
    carried: Option<AnalyzedCommit>,
    outcome: BatchOutcome,
}

/// What one batch did.
enum BatchOutcome {
    /// The batch committed its commits.
    Written,
    /// No pending commit was left to analyze, so nothing was written.
    Drained,
    /// The store refused the batch, which was logged.
    Failed,
}

/// Logs one failed fill step.
fn fill_failed(error: &str) {
    tracing::warn!(
        component = "history",
        operation = "history.fill",
        error,
        "a history store fill stopped; the next fill plans again from what the store holds"
    );
}

/// Runs `work` on the runtime's blocking threads; `None` when it panicked,
/// which is logged.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => Some(value),
        Err(error) => {
            fill_failed(&error.to_string());
            None
        }
    }
}

#[cfg(test)]
mod tests;
