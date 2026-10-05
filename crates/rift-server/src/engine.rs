//! The pool of language engine sessions the server holds across requests.
//!
//! One [`EngineSlot`] exists per accepted LSP process definition. A slot
//! spawns its engine on the first request for a language bound to it, reuses
//! the running session across requests, and replaces an engine that ended,
//! failed to start, or stopped answering within the budget its
//! `restart` table states. The pool never invents an engine: a language no
//! accepted binding names a process for answers no slot, and the caller turns
//! that absence into its own refusal.
//!
//! The slot is also where every transient condition between Rift and one
//! engine is absorbed. [`EngineSlot::request`] runs the caller's operation
//! again while the engine answers provisionally, with nothing, or with a refusal, under
//! that process's `retry` table, and starts a replacement engine under its
//! `restart` table when the one it has
//! dies. It also sends an operation again under configured retry policy
//! when an engine answers nothing. Progress does not bind announced work
//! to one semantic request, so an empty answer stays provisional.
//! Callers hold no retry loop of their own: an operation returns
//! either the engine's settled answer or the failure that outlasted the
//! whole budget.
//!
//! Locking: each slot owns one Tokio mutex over its own session and its own
//! restart budget, and the pool holds no lock spanning two slots. A request
//! holds that slot's lock for the whole conversation - across a spawn, a
//! restart, and any wait an operation takes between its own attempts - so
//! requests for one engine serialize while requests for other engines
//! proceed untouched. No std lock is held across an await anywhere in the
//! pool.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lsp_types::FileChangeType;
use rift_core::ProjectPath;
use rift_error::{RiftError, errors};
use rift_index::{PathChange, PathChanges};
use rift_lsp::session::{EngineLaunch, EngineSession};
use rift_protocol::configuration::{CommandInput, EmbeddedEngine, LspConfiguration};
use rift_protocol::read::Language;
use rift_protocol::retry::{RETRY_ATTEMPTS_MAX, RestartPolicy, RetryPolicy};
use rift_protocol::workspace::LspState;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

use rift_lsp::session::EngineReadiness;

/// One step of an engine exchange: a future borrowing the session it runs on.
pub(crate) type SessionFuture<'session, T> =
    std::pin::Pin<Box<dyn Future<Output = T> + Send + 'session>>;

/// How many distinct changed paths one slot holds for its live session before it
/// replaces the session instead: a replacement reads every file from disk at its start.
const OWED_CHANGES_MAX: usize = 16_384;

fn begin_immediately(_session: &mut EngineSession) -> SessionFuture<'_, Result<(), RiftError>> {
    Box::pin(async { Ok(()) })
}

fn finish_immediately(_session: &mut EngineSession) -> SessionFuture<'_, ()> {
    Box::pin(async {})
}

/// The language engines one workspace serves, spawned lazily and reused.
///
/// Dropping the pool without [`EnginePool::shutdown`] still kills every
/// running child through the session's kill-on-drop arming; `shutdown` is
/// the graceful path that also asks each engine to exit first.
#[derive(Debug)]
pub struct EnginePool {
    engines: BTreeMap<LspProcessKey, Arc<EngineSlot>>,
    served: BTreeMap<String, LspProcessKey>,
}

/// Identity of one accepted LSP process definition.
///
/// Named definitions and inline definitions remain different even when
/// their visible text is equal. Inline text is one exact language identity
/// segment.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum LspProcessKey {
    /// Key from one shared top-level `[lsp.<name>]` definition.
    Named(String),
    /// Exact language identity owning one inline LSP definition.
    Inline(String),
}

impl LspProcessKey {
    /// Key for one named top-level LSP definition.
    #[must_use]
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named(name.into())
    }

    /// Key for an inline definition owned by one exact language.
    #[must_use]
    pub fn inline(language: &Language) -> Self {
        Self::Inline(language.identity_segment())
    }

    /// Configured name or exact language identity segment.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Named(name) | Self::Inline(name) => name,
        }
    }
}

impl EnginePool {
    /// Builds a pool from accepted process definitions and exact language
    /// bindings, spawning nothing.
    #[must_use]
    pub fn new(
        workspace_root: &Path,
        definitions: BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: BTreeMap<String, LspProcessKey>,
    ) -> Self {
        Self::build(None, workspace_root, definitions, bindings)
    }

    /// Builds a replacement pool, reusing slots whose process key,
    /// configuration, and workspace root are unchanged.
    #[must_use]
    pub fn reconfigure(
        &self,
        workspace_root: &Path,
        definitions: BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: BTreeMap<String, LspProcessKey>,
    ) -> Self {
        Self::build(Some(self), workspace_root, definitions, bindings)
    }

    fn build(
        prior: Option<&Self>,
        workspace_root: &Path,
        definitions: BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: BTreeMap<String, LspProcessKey>,
    ) -> Self {
        let engines = definitions
            .into_iter()
            .map(|(key, configuration)| {
                let reusable = prior
                    .and_then(|pool| pool.engines.get(&key))
                    .filter(|slot| {
                        slot.configuration == configuration && slot.workspace_root == workspace_root
                    })
                    .cloned();
                let slot = reusable.unwrap_or_else(|| {
                    let (reported_state, _state_receiver) = watch::channel(LspState::Stopped);
                    Arc::new(EngineSlot {
                        key: key.clone(),
                        configuration,
                        workspace_root: workspace_root.to_path_buf(),
                        state: Mutex::new(SlotState::default()),
                        owed: std::sync::Mutex::new(OwedChanges::default()),
                        reported_state,
                    })
                });
                (key, slot)
            })
            .collect();
        Self {
            engines,
            served: bindings,
        }
    }

    /// The slot serving `language`, absent when no engine claims its
    /// identity segment.
    #[must_use]
    pub fn engine_for(&self, language: &Language) -> Option<&EngineSlot> {
        let name = self.served.get(&language.identity_segment())?;
        self.engines.get(name).map(Arc::as_ref)
    }

    /// Whether this pool was built from exactly these process definitions
    /// and language bindings.
    #[must_use]
    pub fn built_from(
        &self,
        definitions: &BTreeMap<LspProcessKey, LspConfiguration>,
        bindings: &BTreeMap<String, LspProcessKey>,
    ) -> bool {
        self.served == *bindings
            && self.engines.len() == definitions.len()
            && definitions.iter().all(|(name, table)| {
                self.engines
                    .get(name)
                    .is_some_and(|slot| &slot.configuration == table)
            })
    }

    /// Slot for one accepted process key.
    #[must_use]
    pub fn engine_by_key(&self, key: &LspProcessKey) -> Option<&EngineSlot> {
        self.engines.get(key).map(Arc::as_ref)
    }

    /// Each exact language identity segment this pool binds, with the slot serving it.
    pub(crate) fn served_slots(&self) -> impl Iterator<Item = (&str, &EngineSlot)> {
        self.served.iter().filter_map(|(language, key)| {
            let slot = self.engines.get(key)?;
            Some((language.as_str(), slot.as_ref()))
        })
    }

    /// Whether any slot reports an engine it started and has not stopped.
    #[must_use]
    pub fn runs_an_engine(&self) -> bool {
        self.engines.values().any(|slot| {
            !matches!(
                *slot.reported_state.borrow(),
                LspState::Stopped | LspState::Failed
            )
        })
    }

    /// Records one publication's changed files for every slot, without waiting.
    ///
    /// Each slot sends what it holds as one `workspace/didChangeWatchedFiles` batch
    /// to its live session before that session's next exchange, so a request that
    /// reads the publication naming these changes asks an engine that was told of
    /// them. A slot with no live session drops them when it starts one, since a
    /// new session reads every file from disk.
    pub fn owe_changed_paths(&self, changes: &PathChanges) {
        if changes.is_empty() {
            return;
        }
        for slot in self.engines.values() {
            slot.owe(changes);
        }
    }

    /// Current state of one accepted process definition.
    #[must_use]
    pub fn state_for_key(&self, key: &LspProcessKey) -> Option<LspState> {
        let slot = self.engines.get(key)?;
        Some(*slot.reported_state.borrow())
    }

    /// Ends every running engine at once, each with the session's own bounded shutdown.
    ///
    /// Each engine ends under its own slot's lock, so a request in flight on
    /// it finishes - or times out - first; the engines end side by side, so
    /// the pool's shutdown takes one session's bound rather than their sum.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future aborts every shutdown still running: a session an
    /// aborted shutdown had taken is dropped with kill-on-drop armed, and one
    /// still in its slot is killed the same way when the pool drops.
    pub async fn shutdown(&self) {
        let mut ending = JoinSet::new();
        for slot in self.engines.values() {
            ending.spawn(Arc::clone(slot).end_session());
        }
        while let Some(ended) = ending.join_next().await {
            if let Err(error) = ended {
                rift_tracing::warn!(component = "engine", %error, "an engine shutdown task failed");
            }
        }
    }

    /// Ends sessions not shared with one replacement pool.
    ///
    /// A reused slot remains live because both pools point to the same allocation.
    pub async fn shutdown_replaced_by(&self, replacement: &Self) {
        for (key, slot) in &self.engines {
            let reused = replacement
                .engines
                .get(key)
                .is_some_and(|replacement| Arc::ptr_eq(slot, replacement));
            if reused {
                continue;
            }
            Arc::clone(slot).end_session().await;
        }
    }
}

/// One accepted LSP process definition and the session state behind it.
#[derive(Debug)]
pub struct EngineSlot {
    key: LspProcessKey,
    configuration: LspConfiguration,
    workspace_root: PathBuf,
    state: Mutex<SlotState>,
    /// Changed files the live session was not told of yet. A std lock, taken
    /// only for a push or a take and never across an await, so a publication
    /// records changes without waiting for a request that holds `state`.
    owed: std::sync::Mutex<OwedChanges>,
    reported_state: watch::Sender<LspState>,
}

/// Changed files one slot owes its live session, one classification per path.
///
/// A later change to the same path replaces the earlier one. Past
/// [`OWED_CHANGES_MAX`] paths the slot stops recording and replaces the
/// session at its next request instead.
#[derive(Debug, Default)]
struct OwedChanges {
    paths: BTreeMap<ProjectPath, FileChangeType>,
    overflowed: bool,
}

impl OwedChanges {
    /// Records `changes`, or marks the bound spent.
    fn record(&mut self, changes: &PathChanges) {
        if self.overflowed {
            return;
        }
        for (path, change) in changes.iter() {
            if self.paths.len() >= OWED_CHANGES_MAX && !self.paths.contains_key(path) {
                self.paths.clear();
                self.overflowed = true;
                return;
            }
            self.paths.insert(path.clone(), watched_change(change));
        }
    }

    /// Sends the held changes to `session` as one batch, before its next exchange.
    ///
    /// Every failure leaves the session ended: a write that fails or times out ends it
    /// inside the notification, and a project path always forms a document URI. The
    /// caller then starts a replacement, which reads every file from disk.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the session ended or the connection broke.
    async fn send(self, session: &mut EngineSession) -> Result<(), RiftError> {
        if self.paths.is_empty() {
            return Ok(());
        }
        let changes: Vec<(ProjectPath, FileChangeType)> = self.paths.into_iter().collect();
        session
            .notify_changed_paths(&changes)
            .await
            .map(|_matched| ())
    }
}

/// The watched-file event one index classification names.
fn watched_change(change: PathChange) -> FileChangeType {
    match change {
        PathChange::Added => FileChangeType::CREATED,
        PathChange::Modified => FileChangeType::CHANGED,
        PathChange::Removed => FileChangeType::DELETED,
    }
}

/// Everything one slot's lock guards: the running engine, a start still in flight,
/// and the restarts already spent on it.
#[derive(Debug, Default)]
struct SlotState {
    session: Option<EngineSession>,
    /// A start running as a task the slot owns. A request that is dropped while it
    /// waits for the start leaves it here, and the next request waits for the same
    /// start instead of claiming another: the start ends when the engine answers
    /// `initialize` or `startup_timeout` passes, whatever happens to the requests.
    starting: Option<tokio::task::JoinHandle<Result<EngineSession, RiftError>>>,
    restarts: RestartBudget,
}

/// Decides, under the slot lock, whether a session outlives an exchange dropped midway.
///
/// A dropped exchange can skip its asynchronous document close, leave a request
/// pending, or cut a frame part-written. The session records the first and discards the
/// answer to the second at its next exchange, so it survives both: it stays in the slot
/// while it reads intact ([`EngineSession::is_intact`]), and the next exchange closes
/// the documents the dropped one left open before anything else. A cut frame, an ended
/// engine, or a drop while the slot hands the session its owed changes drops the session
/// under the slot lock instead, so the next request starts a replacement. Owed changes
/// left the slot's record when the hand-over began, so a session that missed part of
/// them would answer about files it was never told had moved.
struct RequestSessionGuard<'slot> {
    state: &'slot mut SlotState,
    reported_state: &'slot watch::Sender<LspState>,
    finished: bool,
    sending_owed_changes: bool,
}

impl Drop for RequestSessionGuard<'_> {
    fn drop(&mut self) {
        let survives = self.finished
            || (!self.sending_owed_changes
                && self
                    .state
                    .session
                    .as_ref()
                    .is_some_and(EngineSession::is_intact));
        if !survives {
            drop(self.state.session.take());
            if self.state.starting.is_none() {
                self.reported_state.send_replace(LspState::Failed);
            }
        }
    }
}

/// The restarts one slot spent, and whether it ever started an engine.
///
/// A slot's first start is the start, not a restart, so it is free. Every
/// start after it replaces an engine that ended, failed to start, or
/// stopped answering, and must fit the configured budget; a restart older
/// than the window stops counting against it.
#[derive(Debug, Default)]
struct RestartBudget {
    started: bool,
    spent: VecDeque<Instant>,
}

impl RestartBudget {
    /// Claims one start against `policy`, `false` when the budget is spent.
    ///
    /// The queue holds at most `policy.attempts` instants, because a claim
    /// that would exceed the bound is refused instead of recorded.
    fn claim(&mut self, policy: &RestartPolicy, now: Instant) -> bool {
        if !self.started {
            self.started = true;
            return true;
        }
        let window = policy.window();
        while self
            .spent
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= window)
        {
            self.spent.pop_front();
        }
        if self.spent.len() as u64 >= policy.attempts {
            return false;
        }
        self.spent.push_back(now);
        true
    }
}

/// Whether one diagnostic report can be returned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Settlement {
    /// Report can be returned.
    Ready,
    /// The engine announced no work since the last change and stayed quiet
    /// past `settle_delay`, and the full report repeated: the read takes it
    /// and records the engine unconfirmed, as a walk does.
    Unconfirmed,
    /// Report must be requested again.
    Retry,
}

/// Shape of one diagnostic report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReportShape {
    /// Report carries only a partial result.
    Partial,
    /// Full report carries no finding.
    FullEmpty,
    /// Full report carries at least one finding.
    FullNonempty,
}

impl ReportShape {
    fn from_report(full: bool, empty: bool) -> Self {
        if !full {
            Self::Partial
        } else if empty {
            Self::FullEmpty
        } else {
            Self::FullNonempty
        }
    }
}

/// Evidence around one diagnostic report attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DiagnosticEvidence {
    shape: ReportShape,
    repeated: bool,
    final_attempt: bool,
    /// The session read engine output at least `settle_delay` after the last
    /// change ([`EngineSession::walk_is_quiet`]).
    quiet: bool,
}

/// Decides whether one diagnostic report is ready to return.
///
/// An unconfirmed engine's repeated full report is taken once the session is
/// quiet past `settle_delay`: after a file change it was told of,
/// rust-analyzer announces no progress, so the first incoming read after
/// each edit would otherwise spend the whole retry table.
fn diagnostic_settlement(readiness: EngineReadiness, evidence: DiagnosticEvidence) -> Settlement {
    if evidence.shape == ReportShape::Partial || readiness == EngineReadiness::Analyzing {
        return Settlement::Retry;
    }
    if readiness == EngineReadiness::Unconfirmed && evidence.repeated && evidence.quiet {
        Settlement::Unconfirmed
    } else if (readiness == EngineReadiness::Ready && evidence.shape == ReportShape::FullNonempty)
        || (evidence.repeated && evidence.final_attempt)
    {
        Settlement::Ready
    } else {
        Settlement::Retry
    }
}

/// What one outgoing calls answer decides for a walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutgoingSettlement {
    /// The engine read ready; its answer is final.
    Ready,
    /// The engine announced no work since the last change and stayed quiet
    /// past `settle_delay`; the walk trusts its answer and warns.
    Unconfirmed,
    /// The answer is provisional; ask again.
    Retry,
}

/// Decides whether one outgoing calls answer is final for a walk.
///
/// An outgoing answer has no evidence of its own, unlike an incoming one
/// that holds the declaration's own occurrence: rust-analyzer answers an
/// empty prepare or `content modified` while it loads, never a partial
/// callee list, so readiness alone decides. Once the engine reads ready, an
/// empty prepare and an empty callee list are both final: the first refuses
/// the seed, the second is a walk with no callee. Whether prepare gave
/// an item does not change the verdict; [`EngineSlot::request_outgoing`]
/// reads it after.
fn outgoing_settlement(readiness: EngineReadiness, quiet: bool) -> OutgoingSettlement {
    match readiness {
        EngineReadiness::Ready => OutgoingSettlement::Ready,
        EngineReadiness::Unconfirmed if quiet => OutgoingSettlement::Unconfirmed,
        EngineReadiness::Unconfirmed | EngineReadiness::Analyzing => OutgoingSettlement::Retry,
    }
}

/// Whether a walk's wait between two attempts can end: the next attempt's
/// answer would be final.
fn walk_settled(session: &EngineSession) -> bool {
    outgoing_settlement(session.walk_readiness(), session.walk_is_quiet())
        != OutgoingSettlement::Retry
}

/// What one outgoing calls exchange settled on.
#[derive(Debug, Eq, PartialEq)]
pub enum OutgoingAnswer<T> {
    /// The engine read ready and prepared an item; the answer is final.
    Ready(T),
    /// The engine announced no work since the last change and stayed quiet
    /// past `settle_delay`: the walk takes the answer and warns.
    Unconfirmed(T),
    /// The engine read ready and prepared no call hierarchy item at the
    /// seed, so the walk refuses the seed and names its kind.
    Unprepared,
    /// The readiness wait was spent before the engine read ready. The
    /// session stays live, so a later walk continues from its load.
    Unsettled {
        /// Attempts the walk made before the wait was spent.
        attempts: u64,
    },
}

/// The wait before a walk's next attempt, absent once the wait would pass
/// `deadline` or [`RETRY_ATTEMPTS_MAX`] attempts were made.
///
/// The retry table's waits apply as they do to every request, and hold at
/// `delay_limit` once its attempt bound is spent: a walk waits under
/// `[server] readiness_timeout`, not under `retry.attempts`, since a
/// loading engine's readiness, not its answer, is what the walk waits for.
fn walk_wait(
    retry: &RetryPolicy,
    attempt: u64,
    now: Instant,
    deadline: Instant,
) -> Option<Duration> {
    if attempt >= RETRY_ATTEMPTS_MAX {
        return None;
    }
    let wait = retry
        .delay_after(attempt)
        .unwrap_or_else(|| Duration::from_millis(retry.delay_limit.milliseconds()));
    (now + wait < deadline).then_some(wait)
}

/// One condition the slot absorbs: the engine may answer the same request
/// differently, so the operation is worth sending again.
///
/// A dead engine is not one of these. It is answered by starting a
/// replacement under the restart budget, not by waiting.
#[derive(Debug)]
enum Transient<T> {
    /// The engine answered while it still had work-done progress
    /// outstanding, so what it answered - a result or a refusal - is
    /// provisional.
    Analyzing,
    /// The engine refused the request. A refusal can precede its settled
    /// verdict, so every refusal receives the same bounded retry schedule.
    Refused(RiftError),
    /// The engine answered nothing where something was expected, so its
    /// silence proves nothing about semantic settlement.
    ///
    /// Answer rides along: once retry table ends, this is only answer.
    AnsweredNothing(T),
    /// A full diagnostic report has not yet repeated without progress.
    Unready,
}

impl<T> Transient<T> {
    /// Whether the condition can mean the engine is still loading, so a walk
    /// keeps waiting for it past the retry table's attempt bound.
    ///
    /// A refusal whose code does not invite the same request again is the
    /// engine's verdict on the request, which no wait changes: it ends a
    /// walk at the retry table's bound, as it ends every other request.
    fn is_loading(&self) -> bool {
        match self {
            Self::Analyzing | Self::Unready => true,
            Self::Refused(refusal) => refusal.slug() == errors::lsp::engine_refused_retryable::SLUG,
            Self::AnsweredNothing(_) => false,
        }
    }

    /// The wait before the attempt that follows this condition, decided at
    /// `now`, absent once the request's attempts end.
    ///
    /// Without `deadline`, the retry table's attempt bound ends the attempts;
    /// with it, [`walk_wait`] does while the condition can mean the engine is
    /// still loading ([`Transient::is_loading`]), and the retry table's bound
    /// or `deadline`, whichever comes first, does otherwise.
    fn wait_after(
        &self,
        retry: &RetryPolicy,
        attempt: u64,
        now: Instant,
        deadline: Option<Instant>,
    ) -> Option<Duration> {
        match deadline {
            Some(deadline) if self.is_loading() => walk_wait(retry, attempt, now, deadline),
            Some(deadline) => retry
                .delay_after(attempt)
                .filter(|wait| now + *wait < deadline),
            None => retry.delay_after(attempt),
        }
    }
}

/// What one spawned start ended with: the session or the start's failure, a panic
/// resumed on the waiting request, or an ended error for a start the slot
/// aborted while shutting down.
fn joined_start(
    joined: Result<Result<EngineSession, RiftError>, tokio::task::JoinError>,
) -> Result<EngineSession, RiftError> {
    match joined {
        Ok(started) => started,
        Err(failure) => match failure.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_aborted) => errors::lsp::engine_ended().fail(),
        },
    }
}

/// Closes the documents the slot's live session holds open, before an exchange begins.
///
/// An exchange's `finish` closes what its `begin` opened, so a document still open here
/// was left by an exchange dropped midway. A failed close ends the session inside the
/// notification, and the failure comes back as the one the slot reports if no
/// replacement can start.
async fn close_left_open(state: &mut SlotState) -> Option<RiftError> {
    let running = state
        .session
        .as_mut()
        .filter(|running| !running.is_ended())?;
    running.close_open_documents().await.err()
}

/// How one attempt of an exchange ended.
enum AttemptEnd<T> {
    /// The operation returned: the engine answered or refused, or the exchange broke.
    Completed(Result<T, RiftError>),
    /// The walk's wait was spent with this retry in flight; carries the condition the
    /// retry was sent for.
    Spent(Transient<T>),
}

/// Runs one attempt, abandoning a walk's retry still in flight once the walk's deadline
/// passes.
///
/// `retrying` holds the walk's deadline and the condition an earlier attempt answered
/// with. It holds nothing for a walk's first attempt, which is the request itself and is
/// bounded by the engine's `request_timeout` as every request is, and nothing for an
/// exchange that is no walk. A retry belongs to the readiness wait: the wait before it
/// already ends before the deadline, and a retry started just before the deadline would
/// otherwise hold the walk a whole round trip past it, and the request past its own
/// deadline, which refuses the request in place of the walk's warning. Dropping an
/// engine operation leaves its request pending, and the session discards the engine's
/// late answer at its next exchange, so the session stays live. A retry that completes
/// hands `retrying` back unchanged, so a rerun on a replacement session stays bounded by
/// the same deadline.
async fn attempt_within<T>(
    retrying: &mut Option<(Instant, Transient<T>)>,
    operation: impl Future<Output = Result<T, RiftError>>,
) -> AttemptEnd<T> {
    let Some((deadline, retried)) = retrying.take() else {
        return AttemptEnd::Completed(operation.await);
    };
    match tokio::time::timeout_at(deadline, operation).await {
        Ok(outcome) => {
            *retrying = Some((deadline, retried));
            AttemptEnd::Completed(outcome)
        }
        Err(_elapsed) => AttemptEnd::Spent(retried),
    }
}

/// What the retry loop does with one answer.
enum Answer<T> {
    /// Return this answer.
    Ready(T),
    /// Request same operation again.
    Retry(Transient<T>),
}

/// Whether restarting the engine could change this failure's answer.
///
/// A configuration error - an empty program, an absolute one - answers the
/// same way every time, so it surfaces at once instead of spending the
/// restart budget on a start that cannot succeed.
/// The causes under one start failure, joined for a record.
///
/// An engine failure renders its registry text, which names the error and the
/// caller's next step. The operating error behind it - the missing program, the
/// refused permission - lives in the source chain alone, and that is what an
/// operator reading `rift://logs` needs.
fn start_cause(failure: &RiftError) -> String {
    rift_error::causes(failure).join(": ")
}

fn restart_may_help(error: &RiftError) -> bool {
    error.slug() != errors::lsp::engine_program_empty::SLUG
        && error.slug() != errors::lsp::engine_program_absolute::SLUG
}

impl EngineSlot {
    /// Records `changes` for the live session's next exchange.
    fn owe(&self, changes: &PathChanges) {
        self.owed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(changes);
    }

    /// Takes every change recorded so far.
    fn take_owed(&self) -> OwedChanges {
        std::mem::take(
            &mut *self
                .owed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Ends the running session under the slot's lock and reports the slot stopped.
    ///
    /// A start still in flight is aborted, and a start that already finished is shut
    /// down as a running session is.
    async fn end_session(self: Arc<Self>) {
        let mut held = self.state.lock().await;
        if let Some(start) = held.starting.take() {
            start.abort();
            if let Ok(Ok(started)) = start.await {
                started.shutdown().await;
            }
        }
        let Some(session) = held.session.take() else {
            return;
        };
        let stderr = session.shutdown().await;
        self.report_state(LspState::Stopped);
        let engine = self.name();
        rift_tracing::debug!(
            component = "engine",
            engine,
            stderr_bytes = stderr.total_bytes,
            "language engine shut down"
        );
    }

    /// Publishes one nonblocking workspace-resource state observation.
    fn report_state(&self, state: LspState) {
        self.reported_state.send_replace(state);
    }

    /// Publishes readiness from one live LSP session.
    fn report_readiness(&self, readiness: EngineReadiness) {
        let state = match readiness {
            EngineReadiness::Unconfirmed => LspState::Starting,
            EngineReadiness::Analyzing => LspState::Analyzing,
            EngineReadiness::Ready => LspState::Ready,
        };
        self.report_state(state);
    }

    /// The workspace root spelling used in this engine's document URIs.
    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Named process key or inline exact language identity segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.key.as_str()
    }

    /// Identity of this accepted process definition.
    #[must_use]
    pub fn process_key(&self) -> &LspProcessKey {
        &self.key
    }

    /// The accepted LSP process configuration this slot serves under.
    #[must_use]
    pub fn configuration(&self) -> &LspConfiguration {
        &self.configuration
    }

    /// Runs one operation against this engine under configured restart and retry bounds.
    ///
    /// Empty answers receive requests through configured retry table.
    ///
    /// # Errors
    ///
    /// Returns operation failure, retry refusal, analyzing exhaustion, start
    /// failure, or ended session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future keeps the session while it reads intact
    /// ([`EngineSession::is_intact`]): its pending request's late answer is discarded at
    /// the next exchange. A session with a frame cut part-written, or one dropped while
    /// the slot hands it its owed changes, is discarded, and the next request starts a
    /// replacement within the configured restart budget.
    pub async fn request<T>(
        &self,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<T, RiftError>> + Send + 'session>,
        >,
    ) -> Result<T, RiftError> {
        self.request_deciding(
            begin_immediately,
            operation,
            finish_immediately,
            None,
            |_session| false,
            |session, _session_generation, answer, _final_attempt| {
                if session.is_analyzing() {
                    Answer::Retry(Transient::Analyzing)
                } else if session.latest_answer_is_empty() {
                    Answer::Retry(Transient::AnsweredNothing(answer))
                } else {
                    Answer::Ready(answer)
                }
            },
        )
        .await
    }

    /// Runs one document exchange under configured restart and retry bounds.
    ///
    /// `begin` runs once for each live session, retries repeat only
    /// `operation`, and `finish` runs once before that session returns or
    /// exhausts its retry table.
    ///
    /// # Errors
    ///
    /// Returns operation failure, exhausted refusal, analyzing exhaustion,
    /// start failure, or ended session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future can skip `finish`. The session stays while it reads intact
    /// ([`EngineSession::is_intact`]), and the next exchange closes the documents this
    /// one left open before its own `begin`. A session with a frame cut part-written,
    /// or one dropped while the slot hands it its owed changes, is discarded, and the
    /// next request starts a replacement within the configured restart budget.
    pub async fn request_exchange<T>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), RiftError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<T, RiftError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
    ) -> Result<T, RiftError> {
        self.request_deciding(
            begin,
            operation,
            finish,
            None,
            |_session| false,
            |session, _session_generation, answer, _final_attempt| {
                if session.is_analyzing() {
                    Answer::Retry(Transient::Analyzing)
                } else if session.latest_answer_is_empty() {
                    Answer::Retry(Transient::AnsweredNothing(answer))
                } else {
                    Answer::Ready(answer)
                }
            },
        )
        .await
    }

    /// Runs one document exchange until a nonempty report follows completed
    /// progress or two equal full reports reach the configured bound.
    ///
    /// `begin` runs once on each live session before the first operation.
    /// Retries repeat only `operation`. `finish` runs once before a live
    /// session returns or exhausts its retry table. A replacement session
    /// receives its own begin call.
    ///
    /// The exchange is an incoming walk's, and waits as an outgoing one does
    /// ([`EngineSlot::request_outgoing`]): past the retry table's attempt
    /// bound, up to `deadline`, so a spent wait ends inside the loop with
    /// an analyzing error and the session kept. A retry still in
    /// flight at `deadline` is abandoned there and counts among the attempts
    /// the walk reports. A wait that follows an attempt the engine answered
    /// while analyzing ends once the session no longer reads analyzing, and
    /// one that follows an unconfirmed attempt read before the quiet ends
    /// once the session reads quiet.
    ///
    /// An unconfirmed engine's repeated full report is taken once the session
    /// is quiet past `settle_delay`, and the answer comes back with `true`, so
    /// the read records the engine unconfirmed. An unconfirmed session
    /// has seen no progress transition since the change, so the walk's quiet
    /// ([`EngineSession::walk_is_quiet`]) is the quiet of every token,
    /// flycheck included.
    ///
    /// # Errors
    ///
    /// Returns operation failure, retry refusal, unready exhaustion, start
    /// failure, or ended session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future can skip `finish`. The session stays while it reads intact
    /// ([`EngineSession::is_intact`]), and the next exchange closes the documents this
    /// one left open before its own `begin`. A session with a frame cut part-written,
    /// or one dropped while the slot hands it its owed changes, is discarded, and the
    /// next request starts a replacement within the configured restart budget.
    pub async fn request_settled<T: PartialEq>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), RiftError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<T, RiftError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        mut report_state: impl FnMut(&T) -> (bool, bool),
        deadline: Instant,
    ) -> Result<(T, bool), RiftError> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let mut previous = None;
        let mut settled = Settlement::Retry;
        let analyzing = AtomicBool::new(false);
        let awaiting_quiet = AtomicBool::new(false);
        let answer = self
            .request_deciding(
                begin,
                operation,
                finish,
                Some(deadline),
                |session| {
                    (analyzing.load(Ordering::Relaxed) && !session.is_analyzing())
                        || (awaiting_quiet.load(Ordering::Relaxed)
                            && session.readiness() == EngineReadiness::Unconfirmed
                            && session.walk_is_quiet())
                },
                |session, session_generation, answer, final_attempt| {
                    analyzing.store(session.is_analyzing(), Ordering::Relaxed);
                    let quiet = session.walk_is_quiet();
                    awaiting_quiet.store(
                        session.readiness() == EngineReadiness::Unconfirmed && !quiet,
                        Ordering::Relaxed,
                    );
                    let repeated = previous.as_ref().is_some_and(|(prior_generation, prior)| {
                        *prior_generation == session_generation && prior == &answer
                    });
                    let (full, empty) = report_state(&answer);
                    settled = diagnostic_settlement(
                        session.readiness(),
                        DiagnosticEvidence {
                            shape: ReportShape::from_report(full, empty),
                            repeated,
                            final_attempt,
                            quiet,
                        },
                    );
                    if settled == Settlement::Retry {
                        previous = Some((session_generation, answer));
                        Answer::Retry(Transient::Unready)
                    } else {
                        Answer::Ready(answer)
                    }
                },
            )
            .await?;
        Ok((answer, settled == Settlement::Unconfirmed))
    }

    /// Runs one outgoing calls exchange until the engine reads ready for a
    /// walk, or `deadline` passes.
    ///
    /// `operation` answers `None` when prepare gave no item at the seed and
    /// the callees otherwise. Readiness is the walk's
    /// ([`EngineSession::walk_readiness`]): rust-analyzer's `cargo check`
    /// does not hold a walk back. The waits between attempts follow the
    /// retry table and continue past its attempt bound up to `deadline`, so a
    /// spent wait ends inside the loop: `finish` runs, the session stays
    /// live, and the answer is `OutgoingAnswer::Unsettled` instead of a
    /// dropped request that refuses the caller. A retry still in flight at
    /// `deadline` is abandoned there and ends the walk the same way.
    ///
    /// # Errors
    ///
    /// Returns operation failure, exhausted refusal, start failure, or ended
    /// session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future can skip `finish`. The session stays while it reads intact
    /// ([`EngineSession::is_intact`]), and the next exchange closes the documents this
    /// one left open before its own `begin`. A session with a frame cut part-written,
    /// or one dropped while the slot hands it its owed changes, is discarded, and the
    /// next request starts a replacement within the configured restart budget.
    pub async fn request_outgoing<T>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), RiftError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        )
            -> SessionFuture<'session, Result<Option<T>, RiftError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        deadline: Instant,
    ) -> Result<OutgoingAnswer<T>, RiftError> {
        let mut settled = OutgoingSettlement::Retry;
        let answer = self
            .request_deciding(
                begin,
                operation,
                finish,
                Some(deadline),
                walk_settled,
                |session, _session_generation, answer, _final_attempt| {
                    settled =
                        outgoing_settlement(session.walk_readiness(), session.walk_is_quiet());
                    match settled {
                        OutgoingSettlement::Retry => Answer::Retry(Transient::Analyzing),
                        OutgoingSettlement::Ready | OutgoingSettlement::Unconfirmed => {
                            Answer::Ready(answer)
                        }
                    }
                },
            )
            .await;
        match answer {
            Ok(None) => Ok(OutgoingAnswer::Unprepared),
            Ok(Some(callees)) if settled == OutgoingSettlement::Unconfirmed => {
                Ok(OutgoingAnswer::Unconfirmed(callees))
            }
            Ok(Some(callees)) => Ok(OutgoingAnswer::Ready(callees)),
            Err(error) if error.slug() == errors::lsp::engine_analyzing::SLUG => {
                let attempts = error
                    .context()
                    .find(|(key, _)| *key == "attempts")
                    .and_then(|(_, value)| value.parse().ok())
                    .unwrap_or_default();
                Ok(OutgoingAnswer::Unsettled { attempts })
            }
            Err(error) => error.fail(),
        }
    }

    /// Shared bounded request loop.
    ///
    /// The absorbed condition decides the wait between two attempts and when
    /// the loop ends ([`Transient::wait_after`]). The wait reads engine output
    /// ([`EngineSession::read_output`]), so progress is stamped when it
    /// arrives; the wait also ends once `wake` holds. With `deadline`, a retry
    /// still in flight when it passes ends the loop on the condition that
    /// retry was sent for ([`attempt_within`]).
    ///
    /// A live session first closes the documents an earlier, dropped exchange left
    /// open, then receives its owed changes, and only then does `begin` run.
    /// [`RequestSessionGuard`] decides whether a session survives this exchange
    /// being dropped.
    async fn request_deciding<T>(
        &self,
        mut begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), RiftError>>,
        mut operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        )
            -> SessionFuture<'session, Result<T, RiftError>>,
        mut finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        deadline: Option<Instant>,
        wake: impl Fn(&EngineSession) -> bool,
        mut decide: impl FnMut(&mut EngineSession, u64, T, bool) -> Answer<T>,
    ) -> Result<T, RiftError> {
        let retry = self.configuration.retry;
        let mut held = self.state.lock().await;
        let mut guarded = RequestSessionGuard {
            state: &mut held,
            reported_state: &self.reported_state,
            finished: false,
            sending_owed_changes: false,
        };
        let mut attempt: u64 = 1;
        let mut retrying: Option<(Instant, Transient<T>)> = None;
        let mut exchange_started = false;
        let mut session_generation = 0_u64;
        let mut reported = Box::pin(close_left_open(&mut *guarded.state)).await;
        loop {
            let state = &mut *guarded.state;
            let owed = self.take_owed();
            let session = match state.session.take() {
                Some(running) if !running.is_ended() && !owed.overflowed => {
                    let running = state.session.insert(running);
                    guarded.sending_owed_changes = true;
                    let sent = owed.send(running).await;
                    guarded.sending_owed_changes = false;
                    if let Err(error) = sent {
                        reported = Some(error);
                        continue;
                    }
                    running
                }
                dead => {
                    // A replacement opens nothing on its own, so `begin` runs on it
                    // again, even when the session it replaces was still live.
                    exchange_started = false;
                    Box::pin(self.start_replacement(state, dead, reported.take())).await?
                }
            };
            if !exchange_started {
                match begin(session).await {
                    Ok(()) => {
                        exchange_started = true;
                        session_generation = session_generation.saturating_add(1);
                    }
                    Err(error) if session.is_ended() => {
                        reported = Some(error);
                        continue;
                    }
                    Err(error) => return error.fail(),
                }
            }
            self.report_readiness(session.readiness());
            let attempted = Box::pin(attempt_within(&mut retrying, operation(session))).await;
            let ended = session.is_ended();
            if ended {
                self.report_state(LspState::Failed);
            } else {
                self.report_readiness(session.readiness());
            }
            let final_attempt = retry.delay_after(attempt).is_none();
            let absorbed = match attempted {
                // A spent retry ends at the walk's deadline, so no wait fits after it and
                // the loop ends below on the condition the retry was sent for.
                AttemptEnd::Spent(retried) => retried,
                AttemptEnd::Completed(Ok(answer)) => {
                    match decide(session, session_generation, answer, final_attempt) {
                        Answer::Ready(answer) => {
                            finish(session).await;
                            guarded.finished = true;
                            return Ok(answer);
                        }
                        Answer::Retry(absorbed) => absorbed,
                    }
                }
                AttemptEnd::Completed(Err(error)) if ended => {
                    exchange_started = false;
                    reported = Some(error);
                    continue;
                }
                AttemptEnd::Completed(Err(error))
                    if error.slug() == errors::lsp::engine_refused_retryable::SLUG
                        || error.slug() == errors::lsp::engine_refused_terminal::SLUG =>
                {
                    Transient::Refused(error)
                }
                AttemptEnd::Completed(Err(error)) => {
                    finish(session).await;
                    guarded.finished = true;
                    return error.fail();
                }
            };
            // One clock read decides the wait and schedules its end, so the end
            // checked against the deadline is the end the wait keeps.
            let now = Instant::now();
            let Some(wait) = absorbed.wait_after(&retry, attempt, now, deadline) else {
                finish(session).await;
                guarded.finished = true;
                return self.exhausted(absorbed, attempt, deadline.is_some());
            };
            retrying = deadline.map(|deadline| (deadline, absorbed));
            // Boxed: the wait reads engine frames, which would otherwise size every
            // exchange's future.
            let waited = Box::pin(session.read_output(now + wait, &wake)).await;
            if let Err(error) = waited {
                exchange_started = false;
                reported = Some(error);
            }
            attempt += 1;
        }
    }

    /// What one absorbed condition surfaces once attempt bound is spent.
    ///
    /// A walk's wait spent on a retryable refusal surfaces as
    /// an analyzing error, as a wait spent on analyzing answers does:
    /// rust-analyzer answers `-32801` content modified while it loads, and an
    /// empty prepare at the same moment means the same load.
    fn exhausted<T>(
        &self,
        absorbed: Transient<T>,
        attempts: u64,
        walk: bool,
    ) -> Result<T, RiftError> {
        let engine = self.name();
        match absorbed {
            Transient::Refused(refusal)
                if walk && refusal.slug() == errors::lsp::engine_refused_retryable::SLUG =>
            {
                rift_tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    refusal = %refusal,
                    "language engine refused with a retryable code when the walk's wait was spent"
                );
                errors::lsp::engine_analyzing().attempts(attempts).fail()
            }
            Transient::Analyzing | Transient::Unready => {
                rift_tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    "language engine was not ready on every attempt"
                );
                errors::lsp::engine_analyzing().attempts(attempts).fail()
            }
            Transient::Refused(refusal) => {
                rift_tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    "language engine refused every configured attempt"
                );
                Err(refusal)
            }
            Transient::AnsweredNothing(answer) => {
                rift_tracing::debug!(
                    component = "engine",
                    engine,
                    attempts,
                    "language engine answered nothing through every configured attempt"
                );
                Ok(answer)
            }
        }
    }

    /// Starts one engine, restarting past a failed start while the budget
    /// allows.
    ///
    /// Each start runs as a task the slot owns ([`EngineSlot::spawn_start`]), kept in
    /// `state` until it ends, so a request dropped while it waits leaves the start
    /// running, and the next request waits for that start instead of claiming
    /// another. After the start it may find in flight, the loop claims at most
    /// `restart.attempts` + 1 starts, and a refused claim ends it. A refused claim
    /// surfaces `reported` - the failure that sent the caller back here -
    /// or an ended error when this call has no failure of its own
    /// to report, which is the honest answer for a budget an earlier
    /// request already spent.
    ///
    /// Every start that fails is recorded before this returns, cause included.
    /// The refusal reaches the caller, and `rift://logs` is where the agent
    /// holding that refusal looks for the program that could not run.
    async fn start_within_budget(
        &self,
        state: &mut SlotState,
        mut reported: Option<RiftError>,
    ) -> Result<EngineSession, RiftError> {
        loop {
            if let Some(start) = state.starting.as_mut() {
                let started = joined_start(start.await);
                state.starting = None;
                match self.settled_start(started) {
                    ControlFlow::Break(outcome) => return outcome,
                    ControlFlow::Continue(failure) => reported = Some(failure),
                }
            }
            if !state
                .restarts
                .claim(&self.configuration.restart, Instant::now())
            {
                self.report_state(LspState::Failed);
                let engine = self.name();
                let surfaced = reported
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string);
                let cause = reported.as_ref().map_or_else(String::new, start_cause);
                rift_tracing::warn!(
                    component = "engine",
                    engine,
                    program = self.program(),
                    attempts = self.configuration.restart.attempts,
                    error = surfaced,
                    cause,
                    "language engine restart budget is spent for this window"
                );
                if let Some(reported) = reported {
                    return reported.fail();
                }
                return errors::lsp::engine_ended().fail();
            }
            self.report_state(LspState::Starting);
            state.starting = Some(self.spawn_start());
        }
    }

    /// Settles what one start ended with: a session, or a failure no restart can fix,
    /// ends the start loop; a failure a restart may fix is recorded and continues it.
    fn settled_start(
        &self,
        started: Result<EngineSession, RiftError>,
    ) -> ControlFlow<Result<EngineSession, RiftError>, RiftError> {
        match started {
            Ok(session) => {
                self.report_readiness(session.readiness());
                ControlFlow::Break(Ok(session))
            }
            Err(failure) if !restart_may_help(&failure) => {
                self.report_state(LspState::Failed);
                self.record_start_failure(&failure, false);
                ControlFlow::Break(Err(failure))
            }
            Err(failure) => {
                self.record_start_failure(&failure, true);
                ControlFlow::Continue(failure)
            }
        }
    }

    /// Spawns one start of this slot's engine as a task the slot owns.
    ///
    /// The task ends when the engine answers `initialize` or `startup_timeout` passes;
    /// a request that waits for it can be dropped without ending it.
    fn spawn_start(&self) -> tokio::task::JoinHandle<Result<EngineSession, RiftError>> {
        let root = self.workspace_root.clone();
        match (
            self.configuration.embedded,
            self.configuration.command.as_ref(),
        ) {
            (Some(engine), _) => {
                let launch = self.embedded_launch(engine);
                tokio::spawn(async move { crate::embedded::started_session(launch, &root).await })
            }
            (None, Some(command)) => {
                let launch = self.launch(command);
                tokio::spawn(async move { EngineSession::start(launch, &root).await })
            }
            (None, None) => {
                unreachable!("acceptance refuses an LSP table naming neither command nor embedded")
            }
        }
    }

    /// The program this slot starts, for a record naming what could not run. An embedded
    /// engine has no command line of its own, so it is named by the program it stands for.
    fn program(&self) -> &str {
        match (
            self.configuration.embedded,
            self.configuration.command.as_ref(),
        ) {
            (Some(EmbeddedEngine::Ty), _) => "ty",
            (None, Some(command)) => command.program(),
            (None, None) => {
                unreachable!("acceptance refuses an LSP table naming neither command nor embedded")
            }
        }
    }

    /// Records one start that failed, naming the program and the cause.
    ///
    /// A start gives up in three places, and the budget's own record names the
    /// budget. Without this, a missing program reached the caller as
    /// `launch_failed` and left the workspace log holding nothing that says
    /// which program was missing.
    fn record_start_failure(&self, failure: &RiftError, retrying: bool) {
        rift_tracing::warn!(
            component = "engine",
            engine = self.name(),
            program = self.program(),
            retrying,
            error = %failure,
            cause = start_cause(failure),
            "language engine did not start"
        );
    }

    /// The launch derived from this accepted LSP process configuration;
    /// `command` is the program the session spawns.
    fn launch(&self, command: &CommandInput) -> EngineLaunch {
        EngineLaunch {
            program: command.program().to_owned(),
            arguments: command.arguments().to_vec(),
            environment: self.configuration.environment.clone(),
            initialization_options: self.configuration.initialization_options.clone(),
            startup_timeout: Duration::from_millis(
                self.configuration.startup_timeout.milliseconds(),
            ),
            request_timeout: Duration::from_millis(
                self.configuration.request_timeout.milliseconds(),
            ),
            settle_delay: Duration::from_millis(self.configuration.settle_delay.milliseconds()),
            stderr_capture_bytes: usize::try_from(self.configuration.output_limit.bytes())
                .unwrap_or(usize::MAX),
        }
    }

    /// The launch for an embedded engine: the engine's name stands in for
    /// a program, and the session's bounds come from the accepted table.
    /// Acceptance refuses `environment` and `initialization_options`
    /// beside `embedded`, so both stay empty here.
    fn embedded_launch(&self, engine: EmbeddedEngine) -> EngineLaunch {
        EngineLaunch {
            program: match engine {
                EmbeddedEngine::Ty => "ty".to_owned(),
            },
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            initialization_options: None,
            startup_timeout: Duration::from_millis(
                self.configuration.startup_timeout.milliseconds(),
            ),
            request_timeout: Duration::from_millis(
                self.configuration.request_timeout.milliseconds(),
            ),
            settle_delay: Duration::from_millis(self.configuration.settle_delay.milliseconds()),
            stderr_capture_bytes: usize::try_from(self.configuration.output_limit.bytes())
                .unwrap_or(usize::MAX),
        }
    }

    /// Reaps `replaced`, if any, and starts the session that takes its place in `state`
    /// within the restart budget, surfacing `reported` when the budget is spent.
    ///
    /// The exchange loop awaits this boxed: a start holds a whole session and its
    /// handshake, which would otherwise size every exchange's future.
    async fn start_replacement<'state>(
        &self,
        state: &'state mut SlotState,
        replaced: Option<EngineSession>,
        reported: Option<RiftError>,
    ) -> Result<&'state mut EngineSession, RiftError> {
        if let Some(replaced) = replaced {
            // Boxed: a reap awaits the session's shutdown request.
            Box::pin(self.reap(replaced)).await;
        }
        // Boxed: a start's wait and its failure records stay out of every exchange's future.
        let started = Box::pin(self.start_within_budget(state, reported)).await?;
        Ok(state.session.insert(started))
    }

    /// Reaps one session the slot replaces, and keeps an ended engine's
    /// captured standard error visible in the log.
    ///
    /// A live session reaches here only when more files changed between two
    /// requests than [`OWED_CHANGES_MAX`] allows telling it of: its replacement
    /// reads every file from disk.
    async fn reap(&self, replaced: EngineSession) {
        let ended = replaced.is_ended();
        let stderr = replaced.shutdown().await;
        let engine = self.name();
        if ended {
            rift_tracing::warn!(
                component = "engine",
                engine,
                stderr = %stderr.text,
                "language engine ended and was reaped"
            );
        } else {
            rift_tracing::info!(
                component = "engine",
                engine,
                owed_changes_max = OWED_CHANGES_MAX,
                "more files changed between two requests than the language engine is told of, so its session was replaced"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::configuration::{ByteSize, CommandInput, Duration as ConfiguredDuration};
    use rift_protocol::retry::RetryPolicy;
    use rift_tracing::LogRecord;

    /// The one record whose message says `message`, or a panic naming every message seen.
    fn naming<'records>(records: &'records [LogRecord], message: &str) -> &'records LogRecord {
        records
            .iter()
            .find(|record| record.message().contains(message))
            .unwrap_or_else(|| {
                let messages: Vec<&str> = records.iter().map(LogRecord::message).collect();
                panic!("no record says {message:?}: {messages:#?}")
            })
    }

    /// The fields `record` carried, as the JSON object the store holds.
    fn fields(record: &LogRecord) -> serde_json::Value {
        serde_json::from_str(record.fields()).expect("a record's fields are a JSON object")
    }

    /// Serves one slot whose configured program is `command`, and returns the refusal
    /// a request earns beside every record the attempt emitted.
    async fn start_refusal(command: &str, attempts: u64) -> (RiftError, Vec<LogRecord>) {
        let directory = tempfile::tempdir().expect("workspace");
        let configuration: LspConfiguration = serde_json::from_value(serde_json::json!({
            "command": [command], "restart": { "attempts": attempts }
        }))
        .expect("configuration");
        let key = LspProcessKey::named("rust");
        let pool = EnginePool::new(
            directory.path(),
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("rust".to_owned(), key.clone())]),
        );
        let slot = pool.engine_by_key(&key).expect("slot");
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let failure = slot
            .request(|session| Box::pin(async move { Ok(session.document_version()) }))
            .await
            .expect_err("a program that cannot start answers nothing");
        drop(recorder);
        pool.shutdown().await;
        (failure, drain.queued_records())
    }

    /// An embedded engine is named by the program it stands for. It has no command line, and
    /// a record naming an empty program would say nothing about which engine failed.
    #[tokio::test]
    async fn an_embedded_engine_is_named_by_the_program_it_stands_for() {
        let directory = tempfile::tempdir().expect("workspace");
        let configuration: LspConfiguration =
            serde_json::from_value(serde_json::json!({ "embedded": "ty" })).expect("configuration");
        let key = LspProcessKey::named("python");
        let pool = EnginePool::new(
            directory.path(),
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("python".to_owned(), key.clone())]),
        );
        let slot = pool.engine_by_key(&key).expect("slot");
        assert_eq!(slot.program(), "ty");
        pool.shutdown().await;
    }

    /// A program no machine running these tests provides.
    const MISSING_PROGRAM: &str = "rift-engine-that-does-not-exist";

    /// What the platform answers when asked to start [`MISSING_PROGRAM`].
    ///
    /// The cause a record carries is the platform's own: Unix reports the operating error
    /// `No such file or directory (os error 2)`, and Rust's Windows spawn searches `PATH`
    /// itself and answers `program not found` before any operating call.
    fn missing_program_cause() -> String {
        std::process::Command::new(MISSING_PROGRAM)
            .spawn()
            .map_or_else(
                |error| error.to_string(),
                |_child| "the program must not exist".to_owned(),
            )
    }

    /// A configured program that does not exist reaches the caller as `launch_failed`, and
    /// the workspace log names the program and the operating error behind it. Cold first use
    /// read `rift://logs/component/engine` after exactly this refusal and found it empty.
    #[tokio::test]
    async fn a_missing_program_is_recorded_with_its_cause() {
        let (failure, recorded) = start_refusal(MISSING_PROGRAM, 1).await;
        assert!(
            failure.slug() == errors::lsp::engine_launch_failed::SLUG,
            "a missing program answers launch_failed: {failure:?}"
        );
        let record = naming(&recorded, "language engine did not start");
        assert_eq!(record.level(), "warn", "{record:?}");
        assert_eq!(record.component(), "engine", "{record:?}");
        assert_eq!(fields(record)["program"], MISSING_PROGRAM, "{record:?}");
        assert!(
            record.fields().contains(&missing_program_cause()),
            "the record names the cause: {record:?}"
        );
    }

    /// The spent budget names the failure it is surfacing. On its own it said the budget was
    /// spent, which is the outcome and not the reason the caller was refused.
    #[tokio::test]
    async fn the_spent_restart_budget_names_the_failure_it_surfaces() {
        let (_, recorded) = start_refusal(MISSING_PROGRAM, 1).await;
        let record = naming(&recorded, "restart budget is spent");
        assert_eq!(fields(record)["program"], MISSING_PROGRAM, "{record:?}");
        assert!(
            record.fields().contains(&missing_program_cause()),
            "the budget record carries the cause: {record:?}"
        );
    }

    /// A program no restart can fix returns at once, and records before it does. That arm
    /// never reaches the budget, so nothing else would have recorded it.
    #[tokio::test]
    async fn a_program_no_restart_can_fix_is_recorded_before_it_refuses() {
        let (failure, recorded) = start_refusal("/rift-engine-absolute", 4).await;
        assert!(
            !restart_may_help(&failure),
            "an absolute program is refused without spending a restart: {failure:?}"
        );
        let record = naming(&recorded, "language engine did not start");
        let carried = fields(record);
        assert_eq!(carried["retrying"], "false", "{record:?}");
        assert_eq!(carried["program"], "/rift-engine-absolute", "{record:?}");
        assert!(
            !recorded
                .iter()
                .any(|record| record.message().contains("restart budget is spent")),
            "the budget is untouched: {recorded:#?}"
        );
    }

    /// A `sh` engine that appends what it reads to the file its first argument names:
    /// `start` once, `open <file>` and `close <file>` per document notification, and
    /// each request's method. It answers every request with one location, and once it
    /// has answered `deaf_after` requests, `initialize` included, it stops reading its
    /// input, as a hung engine does; `0` keeps it reading. With `watching`, it
    /// registers one `**/*.rs` file watcher once initialized.
    #[cfg(unix)]
    fn logging_slot(directory: &Path, watching: bool, deaf_after: u32) -> EnginePool {
        const SCRIPT: &str = r#"log="$1"
echo start >> "$log"
answered=0
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
answer() {
  frame "$1"
  answered=$((answered + 1))
  if [ "$answered" -eq DEAF_AFTER ]; then exec sleep 60; fi
}
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  file=$(printf '%s' "$body" | grep -o '"uri":"[^"]*"' | head -1 | sed 's|.*/||; s|"$||')
  case "$body" in
    *'"method":"initialize"'*)
      answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}" ;;
    *'"method":"initialized"'*)
      REGISTER ;;
    *'"method":"textDocument/didOpen"'*)
      echo "open $file" >> "$log" ;;
    *'"method":"textDocument/didClose"'*)
      echo "close $file" >> "$log" ;;
    *'"method":"shutdown"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      printf '%s\n' "$body" | grep -o '"method":"[^"]*"' | head -1 | sed 's/"method":"//; s/"$//' >> "$log"
      answer "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"file:///lib.rs\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":1}}}]}" ;;
  esac
done
"#;
        const REGISTER: &str = r#"frame '{"jsonrpc":"2.0","id":"watch","method":"client/registerCapability","params":{"registrations":[{"id":"watch-rust","method":"workspace/didChangeWatchedFiles","registerOptions":{"watchers":[{"globPattern":"**/*.rs"}]}}]}}'"#;
        let script = directory.join("engine.sh");
        std::fs::write(
            &script,
            SCRIPT
                .replace("DEAF_AFTER", &deaf_after.to_string())
                .replace("REGISTER", if watching { REGISTER } else { ":" }),
        )
        .expect("engine script");
        let mut configuration = table("sh");
        configuration.command = Some(CommandInput::ProgramAndArguments(vec![
            "sh".to_owned(),
            script.display().to_string(),
            directory.join("engine.log").display().to_string(),
        ]));
        configuration.initialization_options = None;
        let key = LspProcessKey::named("rust");
        EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    /// The `finish` step of an exchange about `lib.rs`: closes it.
    #[cfg(unix)]
    fn close_lib(session: &mut EngineSession) -> SessionFuture<'_, ()> {
        Box::pin(async move {
            let _closed = session
                .close(&rift_core::ProjectPath::new("lib.rs").expect("path"))
                .await;
        })
    }

    /// An operation that asks the engine nothing and answers the session's document
    /// version, so a request runs it on a session without waiting on the engine.
    #[cfg(unix)]
    fn document_version(session: &mut EngineSession) -> SessionFuture<'_, Result<i32, RiftError>> {
        Box::pin(async move { Ok(session.document_version()) })
    }

    /// The lines the `logging_slot` engine appended.
    #[cfg(unix)]
    fn engine_log(directory: &Path) -> Vec<String> {
        std::fs::read_to_string(directory.join("engine.log"))
            .expect("the engine records what it reads")
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// An exchange dropped while its request waits for the engine keeps the session:
    /// nothing was cut part-written, so the slot keeps it live and spends no restart,
    /// and the next exchange sends `didClose` for the document the dropped one left
    /// open before its own `didOpen`, on the same engine.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_exchange_keeps_its_session_and_the_next_one_closes_its_document() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = logging_slot(directory.path(), false, 0);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        let asked = Arc::new(tokio::sync::Notify::new());
        {
            let operation_started = Arc::clone(&asked);
            let exchange = slot.request_exchange(
                open_lib,
                move |_session| {
                    let started = Arc::clone(&operation_started);
                    Box::pin(async move {
                        started.notify_one();
                        std::future::pending::<Result<(), RiftError>>().await
                    })
                },
                close_lib,
            );
            tokio::pin!(exchange);
            tokio::select! {
                result = &mut exchange => panic!("the operation must wait: {result:?}"),
                () = asked.notified() => {},
            }
        }
        let state = slot.state.lock().await;
        assert!(
            state.session.as_ref().is_some_and(EngineSession::is_intact),
            "the dropped exchange cut no frame, so its session stays"
        );
        assert!(state.restarts.spent.is_empty(), "no restart was spent");
        drop(state);
        assert_ne!(pool.state_for_key(&key), Some(LspState::Failed));

        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let answered = slot
            .request_exchange(open_lib, counted_references(&attempts), close_lib)
            .await
            .expect("the kept session serves the next exchange");
        assert_eq!(answered, Some(1));
        pool.shutdown().await;
        assert_eq!(
            engine_log(directory.path()),
            [
                "start",
                "open lib.rs",
                "close lib.rs",
                "open lib.rs",
                "textDocument/references",
                "close lib.rs",
            ],
            "one engine, and the left-open document closed before it opens again"
        );
    }

    /// Polls `exchange` until `reached` holds, as a barrier that proves where the
    /// exchange waits before the test drops it. Each poll runs the exchange to its next
    /// wait, and an engine that stopped reading holds a large write there, so the first
    /// poll normally reaches it; the bound turns a wait elsewhere into a failure.
    #[cfg(unix)]
    async fn poll_until<T: std::fmt::Debug>(
        exchange: &mut std::pin::Pin<&mut impl Future<Output = Result<T, RiftError>>>,
        reached: impl Fn() -> bool,
    ) {
        const POLLS_MAX: usize = 64;
        for _poll in 0..POLLS_MAX {
            tokio::select! {
                biased;
                result = exchange.as_mut() => panic!("the exchange must wait: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
            if reached() {
                return;
            }
        }
        panic!("the exchange never reached the wait under test in {POLLS_MAX} polls");
    }

    /// An exchange dropped while its `didOpen` frame is part-written to an engine that
    /// stopped reading discards the session, and the next request starts a replacement
    /// within the restart budget.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exchange_dropped_mid_frame_discards_its_session() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = logging_slot(directory.path(), false, 1);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        slot.request(document_version)
            .await
            .expect("the engine answers initialize before it stops reading");
        let opening = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let began = Arc::clone(&opening);
            let exchange = slot.request_exchange(
                move |session| {
                    began.store(true, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async move {
                        session
                            .open(
                                &rift_core::ProjectPath::new("lib.rs").expect("path"),
                                "rust",
                                "x".repeat(1 << 20),
                            )
                            .await
                    })
                },
                |_session| Box::pin(async { Ok(()) }),
                close_lib,
            );
            tokio::pin!(exchange);
            poll_until(&mut exchange, || {
                opening.load(std::sync::atomic::Ordering::SeqCst)
            })
            .await;
        }
        assert!(
            slot.state.lock().await.session.is_none(),
            "a didOpen frame cut part-written discards the session"
        );
        assert_eq!(pool.state_for_key(&key), Some(LspState::Failed));

        let reopened = slot
            .request(document_version)
            .await
            .expect("a replacement starts within the restart budget");
        assert_eq!(reopened, 0, "the replacement opened nothing");
        assert_eq!(
            slot.state.lock().await.restarts.spent.len(),
            1,
            "the replacement spent one restart"
        );
        assert_eq!(
            engine_log(directory.path()),
            ["start", "start"],
            "the cut didOpen never reached the engine whole"
        );
    }

    /// An exchange dropped while the slot hands its live session the owed changes
    /// discards the session: the changes already left the slot's record, so the
    /// session would answer about files it was never told had moved.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_exchange_dropped_while_sending_owed_changes_discards_its_session() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = logging_slot(directory.path(), true, 2);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        slot.request_exchange(open_lib, counted_references(&attempts), close_lib)
            .await
            .expect("the engine registers its watcher and answers before it stops reading");
        let changed = rift_index::FileRecord::Digest(rift_core::FileDigest::of(b"changed"));
        let many = PathChanges::resolve(
            (0..4_096).map(|index| {
                let path = ProjectPath::new(format!("f{index}.rs")).expect("path");
                (path, Some(changed.clone()))
            }),
            |_path| None,
        );
        pool.owe_changed_paths(&many);
        {
            let exchange =
                slot.request_exchange(open_lib, counted_references(&attempts), close_lib);
            tokio::pin!(exchange);
            poll_until(&mut exchange, || {
                slot.owed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .paths
                    .is_empty()
            })
            .await;
        }
        assert!(
            slot.state.lock().await.session.is_none(),
            "a drop while sending owed changes discards the session"
        );
        assert_eq!(pool.state_for_key(&key), Some(LspState::Failed));
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the dropped exchange never reached its own request"
        );
    }

    /// A start whose request is dropped keeps running as the slot's own task, and the
    /// next request waits for that start instead of claiming another: one engine
    /// process, no restart spent, and the slot never reads failed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_start_outlives_the_request_dropped_while_it_waits() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = logging_slot(directory.path(), false, 0);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        {
            let exchange = slot.request(document_version);
            tokio::pin!(exchange);
            poll_until(&mut exchange, || {
                pool.state_for_key(&key) == Some(LspState::Starting)
            })
            .await;
        }
        assert_eq!(
            pool.state_for_key(&key),
            Some(LspState::Starting),
            "a start in flight is no failure"
        );
        let version = slot
            .request(document_version)
            .await
            .expect("the next request meets the start in flight");
        assert_eq!(version, 0, "the started session opened nothing");
        assert!(
            slot.state.lock().await.restarts.spent.is_empty(),
            "the one start claimed no restart"
        );
        pool.shutdown().await;
        assert_eq!(
            engine_log(directory.path()),
            ["start"],
            "one engine process"
        );
    }

    /// A pool shut down while a start is in flight ends that start with the slot.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutting_down_the_pool_ends_a_start_in_flight() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = logging_slot(directory.path(), false, 0);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        {
            let exchange = slot.request(document_version);
            tokio::pin!(exchange);
            poll_until(&mut exchange, || {
                pool.state_for_key(&key) == Some(LspState::Starting)
            })
            .await;
        }
        pool.shutdown().await;
        let state = slot.state.lock().await;
        assert!(state.starting.is_none(), "the shutdown took the start");
        assert!(state.session.is_none(), "no session outlives the shutdown");
    }

    fn table(program: &str) -> LspConfiguration {
        LspConfiguration {
            command: Some(CommandInput::Program(program.to_owned())),
            embedded: None,
            environment: BTreeMap::new(),
            initialization_options: Some(serde_json::json!({ "engine": "fake" })),
            startup_timeout: ConfiguredDuration::from_millis(10_000),
            request_timeout: ConfiguredDuration::from_millis(20_000),
            settle_delay: ConfiguredDuration::from_millis(500),
            output_limit: ByteSize::from_bytes(2_048),
            retry: RetryPolicy::default(),
            restart: RestartPolicy::default(),
        }
    }

    /// The spawned program a test fixture's table names.
    fn fixture_program(configuration: &LspConfiguration) -> &str {
        configuration
            .command
            .as_ref()
            .expect("the fixture names a command")
            .program()
    }

    fn language(name: &str, dialect: Option<&str>) -> Language {
        Language {
            name: name.to_owned(),
            dialect: dialect.map(str::to_owned),
        }
    }

    fn pool(entries: Vec<(&str, LspConfiguration, &[&str])>) -> EnginePool {
        let mut definitions = BTreeMap::new();
        let mut bindings = BTreeMap::new();
        for (name, configuration, languages) in entries {
            let key = LspProcessKey::named(name);
            definitions.insert(key.clone(), configuration);
            for language in languages {
                bindings.insert((*language).to_owned(), key.clone());
            }
        }
        EnginePool::new(Path::new("/rift-test-root"), definitions, bindings)
    }

    #[test]
    fn engine_for_maps_identity_segments_and_answers_nothing_else() {
        let built = pool(vec![
            ("ty", table("uvx"), &["python"]),
            (
                "typescript",
                table("bunx"),
                &["typescript", "typescript:tsx"],
            ),
        ]);
        let python = built
            .engine_for(&language("python", None))
            .expect("python is served");
        assert_eq!(python.name(), "ty");
        assert_eq!(fixture_program(python.configuration()), "uvx");
        let tsx = built
            .engine_for(&language("typescript", Some("tsx")))
            .expect("the tsx dialect segment is served");
        assert_eq!(tsx.name(), "typescript");
        assert!(
            built.engine_for(&language("go", None)).is_none(),
            "an unclaimed language answers no engine"
        );
        assert!(
            built
                .engine_for(&language("python", Some("cython")))
                .is_none(),
            "a dialect segment is not covered by its bare name"
        );
    }

    #[test]
    fn named_and_inline_process_keys_with_equal_text_remain_separate() {
        let named = LspProcessKey::named("rust");
        let inline = LspProcessKey::inline(&language("rust", None));
        let definitions = BTreeMap::from([
            (named.clone(), table("shared")),
            (inline.clone(), table("inline")),
        ]);
        let bindings = BTreeMap::from([
            ("python".to_owned(), named.clone()),
            ("rust".to_owned(), inline.clone()),
        ]);
        let built = EnginePool::new(Path::new("/rift-test-root"), definitions, bindings);
        assert_eq!(
            built
                .engine_for(&language("python", None))
                .expect("python binding")
                .configuration()
                .command
                .as_ref()
                .expect("the fixture names a command")
                .program(),
            "shared"
        );
        assert_eq!(
            built
                .engine_for(&language("rust", None))
                .expect("rust binding")
                .configuration()
                .command
                .as_ref()
                .expect("the fixture names a command")
                .program(),
            "inline"
        );
        assert_eq!(
            built
                .engine_for(&language("python", None))
                .expect("python binding")
                .process_key(),
            &named,
            "a named definition keeps the name it was configured under"
        );
        assert_eq!(
            built
                .engine_for(&language("rust", None))
                .expect("rust binding")
                .process_key(),
            &inline,
            "an inline definition is keyed by the exact language identity owning it"
        );
    }

    #[test]
    fn built_from_compares_definitions_and_bindings() {
        let key = LspProcessKey::named("ty");
        let definitions = BTreeMap::from([(key.clone(), table("uvx"))]);
        let bindings = BTreeMap::from([("python".to_owned(), key.clone())]);
        let built = EnginePool::new(
            Path::new("/rift-test-root"),
            definitions.clone(),
            bindings.clone(),
        );
        assert!(built.built_from(&definitions, &bindings));
        let renamed = BTreeMap::from([(LspProcessKey::named("pyright"), table("uvx"))]);
        assert!(!built.built_from(&renamed, &bindings));
        let mut retimed = definitions.clone();
        if let Some(engine) = retimed.get_mut(&key) {
            engine.request_timeout = ConfiguredDuration::from_millis(30_000);
        }
        assert!(!built.built_from(&retimed, &bindings));
        assert!(!built.built_from(&definitions, &BTreeMap::new()));
    }

    #[tokio::test]
    async fn an_active_ready_slot_reports_ready_without_waiting_for_its_request() {
        let key = LspProcessKey::named("ty");
        let definitions = BTreeMap::from([(key.clone(), table("uvx"))]);
        let bindings = BTreeMap::from([("python".to_owned(), key.clone())]);
        let built = EnginePool::new(Path::new("/rift-test-root"), definitions, bindings);
        let slot = built.engine_by_key(&key).expect("configured slot");
        slot.report_readiness(EngineReadiness::Ready);

        let held = slot.state.lock().await;
        assert_eq!(built.state_for_key(&key), Some(LspState::Ready));
        drop(held);

        slot.report_readiness(EngineReadiness::Analyzing);
        assert_eq!(built.state_for_key(&key), Some(LspState::Analyzing));
        assert_eq!(built.state_for_key(&LspProcessKey::named("absent")), None);
    }

    #[test]
    fn reconfigure_reuses_only_unchanged_slots() {
        let kept = LspProcessKey::named("kept");
        let changed = LspProcessKey::named("changed");
        let definitions = BTreeMap::from([
            (kept.clone(), table("kept")),
            (changed.clone(), table("before")),
        ]);
        let bindings = BTreeMap::from([
            ("rust".to_owned(), kept.clone()),
            ("python".to_owned(), changed.clone()),
        ]);
        let built = EnginePool::new(
            Path::new("/rift-test-root"),
            definitions.clone(),
            bindings.clone(),
        );
        let kept_before = Arc::clone(built.engines.get(&kept).expect("kept slot"));
        let changed_before = Arc::clone(built.engines.get(&changed).expect("changed slot"));

        let mut replacement = definitions;
        replacement.insert(changed.clone(), table("after"));
        let rebuilt = built.reconfigure(Path::new("/rift-test-root"), replacement, bindings);
        assert!(Arc::ptr_eq(
            &kept_before,
            rebuilt.engines.get(&kept).expect("reused slot")
        ));
        assert!(!Arc::ptr_eq(
            &changed_before,
            rebuilt.engines.get(&changed).expect("new slot")
        ));

        let moved = rebuilt.reconfigure(
            Path::new("/other-root"),
            BTreeMap::from([
                (kept.clone(), table("kept")),
                (changed.clone(), table("after")),
            ]),
            BTreeMap::new(),
        );
        assert!(!Arc::ptr_eq(
            rebuilt.engines.get(&kept).expect("old root slot"),
            moved.engines.get(&kept).expect("new root slot")
        ));
    }

    fn restart_policy(attempts: u64, window_ms: u64) -> RestartPolicy {
        RestartPolicy {
            attempts,
            window: ConfiguredDuration::from_millis(window_ms),
        }
    }

    #[test]
    fn a_slots_first_start_is_the_start_not_a_restart() {
        let policy = restart_policy(0, 60_000);
        let mut budget = RestartBudget::default();
        let start = Instant::now();
        assert!(
            budget.claim(&policy, start),
            "a slot that never started an engine starts one with no budget at all"
        );
        assert!(
            !budget.claim(&policy, start),
            "every later start is a restart, and this policy allows none"
        );
    }

    #[test]
    fn restarts_spend_the_budget_and_stop_at_the_bound() {
        let policy = restart_policy(2, 60_000);
        let mut budget = RestartBudget::default();
        let start = Instant::now();
        assert!(budget.claim(&policy, start), "the start is free");
        assert!(budget.claim(&policy, start), "the first restart fits");
        assert!(budget.claim(&policy, start), "the second restart fits");
        assert!(!budget.claim(&policy, start), "the third is past the bound");
        assert_eq!(
            budget.spent.len(),
            2,
            "a refused claim is never recorded, so the queue stays bounded"
        );
    }

    #[test]
    fn a_restart_older_than_the_window_stops_counting() {
        let policy = restart_policy(1, 60_000);
        let mut budget = RestartBudget::default();
        let start = Instant::now();
        assert!(budget.claim(&policy, start));
        assert!(budget.claim(&policy, start), "the one restart fits");
        let inside = start + Duration::from_millis(59_999);
        assert!(
            !budget.claim(&policy, inside),
            "a restart still inside the window keeps the budget spent"
        );
        let past = start + Duration::from_mins(1);
        assert!(
            budget.claim(&policy, past),
            "the earlier restart left the window, so the budget is free again"
        );
        assert_eq!(budget.spent.len(), 1);
    }

    #[test]
    fn a_configuration_error_is_the_one_failure_no_restart_helps() {
        let absolute = errors::lsp::engine_program_absolute()
            .program("/usr/bin/engine")
            .error();
        assert!(!restart_may_help(&absolute));
        assert!(!restart_may_help(
            &errors::lsp::engine_program_empty().error()
        ));
        assert!(restart_may_help(&errors::lsp::engine_ended().error()));
        assert!(restart_may_help(
            &errors::lsp::engine_timed_out()
                .method("textDocument/rename")
                .timeout_ms(1_000_u64)
                .error()
        ));
    }

    #[test]
    fn outgoing_settlement_trusts_ready_and_a_quiet_unconfirmed_engine_alone() {
        use EngineReadiness::{Analyzing, Ready, Unconfirmed};
        for (readiness, quiet, expected) in [
            (Analyzing, false, OutgoingSettlement::Retry),
            (Analyzing, true, OutgoingSettlement::Retry),
            (Unconfirmed, false, OutgoingSettlement::Retry),
            (Unconfirmed, true, OutgoingSettlement::Unconfirmed),
            (Ready, false, OutgoingSettlement::Ready),
            (Ready, true, OutgoingSettlement::Ready),
        ] {
            assert_eq!(
                outgoing_settlement(readiness, quiet),
                expected,
                "{readiness:?} quiet={quiet}"
            );
        }
    }

    /// A retry still in flight is abandoned at the walk deadline. This paused-clock unit
    /// owns the deadline check; the LSP integration test checks the exchange the dropped
    /// retry leaves behind.
    #[tokio::test(start_paused = true)]
    async fn a_retry_in_flight_ends_at_the_walk_deadline() {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let mut retrying: Option<(Instant, Transient<()>)> = Some((deadline, Transient::Analyzing));

        let ended = attempt_within(
            &mut retrying,
            std::future::pending::<Result<(), RiftError>>(),
        )
        .await;

        assert!(matches!(ended, AttemptEnd::Spent(Transient::Analyzing)));
        assert_eq!(
            Instant::now(),
            deadline,
            "the retry ends at the walk deadline"
        );
        assert!(retrying.is_none(), "the spent retry has no later exchange");
    }

    /// The default retry table spends 8 attempts over 9.75 s of waits; a walk
    /// under the 30 s `readiness_timeout` keeps asking at `delay_limit` and
    /// makes 18 attempts, the last at 29.75 s.
    #[test]
    fn walk_wait_follows_the_retry_table_up_to_the_deadline() {
        let retry = RetryPolicy::default();
        let schedule = |next: &dyn Fn(u64, Duration) -> Option<Duration>| {
            let (mut attempt, mut elapsed) = (1_u64, Duration::ZERO);
            while let Some(wait) = next(attempt, elapsed) {
                elapsed += wait;
                attempt += 1;
            }
            (attempt, elapsed)
        };
        assert_eq!(
            schedule(&|attempt, _| retry.delay_after(attempt)),
            (8, Duration::from_millis(9_750))
        );
        let start = Instant::now();
        let deadline = start + Duration::from_secs(30);
        assert_eq!(
            schedule(&|attempt, elapsed| walk_wait(&retry, attempt, start + elapsed, deadline)),
            (18, Duration::from_millis(29_750))
        );
        let never = start + Duration::from_hours(24);
        assert_eq!(
            schedule(&|attempt, elapsed| walk_wait(&retry, attempt, start + elapsed, never)).0,
            RETRY_ATTEMPTS_MAX,
            "the attempt bound still holds a far deadline"
        );
    }

    /// A loading condition waits the walk's waits under the deadline, past the
    /// retry table's attempt bound; any other condition waits the retry
    /// table's under it; without a deadline both wait the retry table's alone.
    #[test]
    fn the_absorbed_condition_decides_the_wait_after_an_attempt() {
        let retry = RetryPolicy::default();
        let first = retry
            .delay_after(1)
            .expect("the table waits after the first attempt");
        let limit = Duration::from_millis(retry.delay_limit.milliseconds());
        let past_the_table = retry.attempts;
        let now = Instant::now();
        let far = Some(now + Duration::from_secs(60));
        let near = Some(now + first / 2);
        let loading = Transient::<()>::Analyzing;
        let answered_nothing = Transient::AnsweredNothing(());
        for (transient, attempt, deadline, expected) in [
            (&loading, 1, far, Some(first)),
            (&loading, 1, near, None),
            (&loading, past_the_table, far, Some(limit)),
            (&loading, past_the_table, None, None),
            (&answered_nothing, 1, far, Some(first)),
            (&answered_nothing, 1, near, None),
            (&answered_nothing, 1, None, Some(first)),
            (&answered_nothing, past_the_table, far, None),
        ] {
            assert_eq!(
                transient.wait_after(&retry, attempt, now, deadline),
                expected,
                "{transient:?} after attempt {attempt} with deadline {:?}",
                deadline.map(|deadline| deadline - now)
            );
        }
    }

    /// A `sh` engine that answers `initialize` and then announces work, answers
    /// each request with no location once it is sent, and ends its work with its
    /// answer to request number `analyzing_requests`, counted from the first
    /// request after `initialize`: the `$/progress` end and that request's
    /// answer arrive in one read, as they would from an engine whose end landed
    /// between two requests. Counting requests, not time, fixes the attempt
    /// that reads the end whatever each request costs. It leaves on `exit`, so
    /// a pool shutdown ends with the engine instead of at the shutdown timeout.
    #[cfg(unix)]
    fn analyzing_engine(directory: &Path, analyzing_requests: u64) -> LspConfiguration {
        const SCRIPT: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
asked=0
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'*)
      asked=$((asked + 1))
      if [ "$asked" -eq ANALYZING_REQUESTS ]; then
        frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}'
      fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;
        let script = directory.join("engine.sh");
        std::fs::write(
            &script,
            SCRIPT.replace("ANALYZING_REQUESTS", &analyzing_requests.to_string()),
        )
        .expect("engine script");
        let mut configuration = table("sh");
        configuration.command = Some(CommandInput::ProgramAndArguments(vec![
            "sh".to_owned(),
            script.display().to_string(),
        ]));
        configuration.initialization_options = None;
        configuration
    }

    #[cfg(unix)]
    fn analyzing_slot(directory: &Path, analyzing_requests: u64) -> EnginePool {
        let key = LspProcessKey::named("rust");
        EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), analyzing_engine(directory, analyzing_requests))]),
            BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    /// One references request against the analyzing engine, counted.
    #[cfg(unix)]
    fn counted_references(
        attempts: &Arc<std::sync::atomic::AtomicU64>,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<Option<usize>, RiftError>> {
        let attempts = Arc::clone(attempts);
        move |session: &mut EngineSession| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let path = rift_core::ProjectPath::new("lib.rs").expect("path");
                let position = lsp_types::Position {
                    line: 0,
                    character: 3,
                };
                session
                    .references(&path, position)
                    .await
                    .map(|locations| Some(locations.len()))
            })
        }
    }

    #[cfg(unix)]
    fn open_lib(session: &mut EngineSession) -> SessionFuture<'_, Result<(), RiftError>> {
        Box::pin(async move {
            session
                .open(
                    &rift_core::ProjectPath::new("lib.rs").expect("path"),
                    "rust",
                    "fn beacon() {}\n".to_owned(),
                )
                .await
        })
    }

    /// Starts `slot`'s engine with one walk whose deadline has already passed.
    ///
    /// A walk's first attempt is bounded by no deadline, so this walk waits out
    /// the engine's start, makes that one attempt, and ends with the session
    /// kept. A walk after it takes its deadline on a running engine, and the
    /// start's cost never decides whether that walk reaches a second attempt.
    #[cfg(unix)]
    async fn start_engine(slot: &EngineSlot) {
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = slot
            .request_outgoing(
                begin_immediately,
                counted_references(&attempts),
                finish_immediately,
                Instant::now(),
            )
            .await;
        assert!(
            slot.state.lock().await.session.is_some(),
            "the engine runs once its first walk ends: {started:?}"
        );
    }

    /// An exchange that is no walk ends at the retry table's 8th attempt,
    /// 9.75 s of waits in, while the engine reads analyzing until its 9th
    /// request: an engine whose load outlasts the retry table answers such a
    /// request with an analyzing error, inside the default 30 s
    /// `readiness_timeout`.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_analyzing_engine_spends_the_retry_table_before_the_readiness_timeout() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 9);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let failure = slot
            .request_exchange(open_lib, counted_references(&attempts), finish_immediately)
            .await
            .expect_err("the engine never reads ready inside the retry table");
        let elapsed = started.elapsed();
        eprintln!(
            "exchange: attempts={attempts:?} elapsed={elapsed:?} error={:?}",
            failure.slug()
        );
        assert_eq!(failure.slug(), errors::lsp::engine_analyzing::SLUG);
        assert!(
            failure
                .context()
                .any(|(key, value)| key == "attempts" && value == "8")
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 8);
        let readiness_timeout = Duration::from_millis(
            rift_protocol::configuration::ServerConfiguration::default()
                .readiness_timeout
                .milliseconds(),
        );
        assert!(
            elapsed >= Duration::from_millis(9_750) && elapsed < readiness_timeout,
            "the exchange waits the retry table's 9.75 s and ends inside the default \
             `readiness_timeout` of {readiness_timeout:?}: {elapsed:?}"
        );
        pool.shutdown().await;
    }

    /// One references request against the analyzing engine, stamped with the
    /// instant it starts.
    #[cfg(unix)]
    fn stamped_references(
        stamps: &Arc<std::sync::Mutex<Vec<Instant>>>,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<Option<usize>, RiftError>> {
        let stamps = Arc::clone(stamps);
        move |session: &mut EngineSession| {
            stamps.lock().expect("attempt stamps").push(Instant::now());
            Box::pin(async move {
                let path = rift_core::ProjectPath::new("lib.rs").expect("path");
                let position = lsp_types::Position {
                    line: 0,
                    character: 3,
                };
                session
                    .references(&path, position)
                    .await
                    .map(|locations| Some(locations.len()))
            })
        }
    }

    /// `operation`, with each attempt's start and end pushed to `times`.
    #[cfg(unix)]
    fn timed<T: Send + 'static>(
        times: &Arc<std::sync::Mutex<Vec<(Instant, Instant)>>>,
        mut operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        )
            -> SessionFuture<'session, Result<T, RiftError>>,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<T, RiftError>> {
        let times = Arc::clone(times);
        move |session: &mut EngineSession| {
            let start = Instant::now();
            let answer = operation(session);
            let times = Arc::clone(&times);
            Box::pin(async move {
                let answer = answer.await;
                times
                    .lock()
                    .expect("attempt times")
                    .push((start, Instant::now()));
                answer
            })
        }
    }

    /// Asserts what a walk's deadline bounds, from each answered attempt's
    /// start and end and the instant the walk returned: the wait after every
    /// attempt but the last was scheduled to end before `deadline`, and the
    /// wait after the last would have passed it. A walk's first attempt runs
    /// unbounded by the deadline and a retry still in flight at it is
    /// abandoned there, so a walk returns past it only by what its first
    /// attempt takes.
    ///
    /// The loop decides a wait after its attempt ends and before the walk
    /// returns, so a wait [`walk_wait`] allows from the attempt's end was
    /// allowed, and one it refuses from the return was refused.
    #[cfg(unix)]
    fn assert_waits_under_the_deadline(
        retry: &RetryPolicy,
        deadline: Instant,
        times: &[(Instant, Instant)],
        returned: Instant,
    ) {
        let Some((_last, earlier)) = times.split_last() else {
            panic!("the walk made no attempt");
        };
        for (index, (_start, end)) in earlier.iter().enumerate() {
            let attempt = u64::try_from(index + 1).expect("an attempt number");
            assert!(
                walk_wait(retry, attempt, *end, deadline).is_some(),
                "the wait after attempt {attempt} was scheduled to end before the deadline: \
                 attempt ended {:?} before it",
                deadline.saturating_duration_since(*end)
            );
        }
        let attempts = u64::try_from(times.len()).expect("an attempt count");
        assert!(
            walk_wait(retry, attempts, returned, deadline).is_none(),
            "the walk ended once the wait after attempt {attempts} would pass the deadline: \
             returned {:?} before it",
            deadline.saturating_duration_since(returned)
        );
    }

    /// A walk waits under the deadline, past the retry table's attempt
    /// bound, and reads ready one `settle_delay` after the attempt that read
    /// the engine's end: this engine sends its end with its answer to the
    /// 10th request, attempt 10, 13.75 s of waits in, and the wait that
    /// follows reads the engine quiet 0.5 s later and ends there, so attempt
    /// 11 settles.
    ///
    /// The assertions read the gaps between attempt starts, so the time each
    /// request takes cannot move them: every wait before the end ran its
    /// scheduled length, and the last one ended between `settle_delay` and
    /// its own scheduled length.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_walk_waits_under_the_deadline_until_the_engine_reads_ready() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 10);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        let stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started = Instant::now();
        let deadline = started + Duration::from_secs(30);
        let answer = slot
            .request_outgoing(
                open_lib,
                stamped_references(&stamps),
                finish_immediately,
                deadline,
            )
            .await
            .expect("the walk settles");
        let stamps = stamps.lock().expect("attempt stamps").clone();
        eprintln!(
            "walk: attempts={} elapsed={:?} answer={answer:?}",
            stamps.len(),
            started.elapsed()
        );
        assert_eq!(answer, OutgoingAnswer::Ready(0));
        assert_eq!(
            stamps.len(),
            11,
            "the end arrives with attempt 10, and attempt 11 settles"
        );

        // The analyzing engine keeps this table's retry schedule and settle_delay.
        let configuration = table("sh");
        let retry = configuration.retry;
        let settle = Duration::from_millis(configuration.settle_delay.milliseconds());
        let scheduled = |attempt: usize, start: Instant| {
            let attempt = u64::try_from(attempt).expect("an attempt number");
            walk_wait(&retry, attempt, start, deadline).expect("a wait under the deadline")
        };
        let gaps: Vec<(Instant, Instant)> =
            stamps.windows(2).map(|pair| (pair[0], pair[1])).collect();
        let Some(((end_read, settled), earlier)) = gaps.split_last() else {
            panic!("the walk made more than one attempt: {stamps:?}");
        };
        for (index, (start, next)) in earlier.iter().enumerate() {
            let attempt = index + 1;
            let wait = scheduled(attempt, *start);
            assert!(
                *next - *start >= wait,
                "the wait after attempt {attempt} ran its {wait:?}: {:?}",
                *next - *start
            );
        }
        let last_wait = scheduled(earlier.len() + 1, *end_read);
        let gap = *settled - *end_read;
        assert!(
            gap >= settle && gap < last_wait,
            "the last wait ends one settle_delay after the end, before its {last_wait:?}: {gap:?}"
        );
        pool.shutdown().await;
    }

    /// A wait spent before the engine reads ready answers `Unsettled` and
    /// keeps the session, where a request dropped at its deadline discards it.
    ///
    /// The deadline bounds the waits between attempts and the retries they
    /// lead to: the second attempt starts under it and is held past it, and
    /// the walk abandons that attempt at the deadline and still answers. The
    /// engine starts before the deadline is taken ([`start_engine`]).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spent_walk_wait_answers_unsettled_and_keeps_the_session() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 50);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        start_engine(slot).await;
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let held_until = deadline + Duration::from_secs(5);
        let mut references = counted_references(&attempts);
        let counted = Arc::clone(&attempts);
        let answer = slot
            .request_outgoing(
                open_lib,
                timed(&times, move |session: &mut EngineSession| {
                    let answer = references(session);
                    let later_attempt = counted.load(std::sync::atomic::Ordering::SeqCst) >= 2;
                    Box::pin(async move {
                        let answer = answer.await;
                        if later_attempt {
                            tokio::time::sleep_until(held_until).await;
                        }
                        answer
                    })
                }),
                finish_immediately,
                deadline,
            )
            .await
            .expect("a spent wait is an answer, not a failure");
        let returned = Instant::now();
        let times = times.lock().expect("attempt times").clone();
        eprintln!(
            "spent walk: completed={} elapsed={:?} answer={answer:?}",
            times.len(),
            returned - started
        );
        assert_eq!(
            answer,
            OutgoingAnswer::Unsettled { attempts: 2 },
            "the wait after the first attempt fits the deadline, the held second one is abandoned"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the answer counts every attempt the walk sent"
        );
        assert_eq!(times.len(), 1, "the held second attempt never completed");
        assert!(
            returned >= deadline && returned < held_until,
            "the walk returns at the deadline, not when the held attempt ends: returned {:?} \
             past the deadline",
            returned.saturating_duration_since(deadline)
        );
        assert_waits_under_the_deadline(&table("sh").retry, deadline, &times, returned);
        assert!(
            slot.state.lock().await.session.is_some(),
            "the session survives the spent wait"
        );
        assert_eq!(pool.state_for_key(&key), Some(LspState::Analyzing));
        pool.shutdown().await;
    }

    /// A `sh` engine serving references and call hierarchy that refuses every
    /// later request with JSON-RPC error `code`, as rust-analyzer answers
    /// `-32801` content modified while it loads. It answers `shutdown` and
    /// leaves on `exit`, so a pool shutdown ends with the engine.
    #[cfg(unix)]
    fn refusing_slot(directory: &Path, code: i64) -> EnginePool {
        const SCRIPT: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}" ;;
    *'"method":"shutdown"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":REFUSAL_CODE,\"message\":\"refused\"}}" ;;
  esac
done
"#;
        let script = directory.join("engine.sh");
        std::fs::write(&script, SCRIPT.replace("REFUSAL_CODE", &code.to_string()))
            .expect("engine script");
        let mut configuration = table("sh");
        configuration.command = Some(CommandInput::ProgramAndArguments(vec![
            "sh".to_owned(),
            script.display().to_string(),
            directory.join("notified.log").display().to_string(),
        ]));
        configuration.initialization_options = None;
        let key = LspProcessKey::named("rust");
        EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    /// One call hierarchy prepare at `lib.rs`'s `beacon`, counted: the item
    /// count, `None` for an empty prepare.
    #[cfg(unix)]
    fn counted_prepare(
        attempts: &Arc<std::sync::atomic::AtomicU64>,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<Option<usize>, RiftError>> {
        let attempts = Arc::clone(attempts);
        move |session: &mut EngineSession| {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                let path = rift_core::ProjectPath::new("lib.rs").expect("path");
                let position = lsp_types::Position {
                    line: 0,
                    character: 3,
                };
                let items = session.prepare_call_hierarchy(&path, position).await?;
                Ok((!items.is_empty()).then_some(items.len()))
            })
        }
    }

    /// A retryable refusal as the last condition when a walk's wait is
    /// spent answers like analyzing: an outgoing walk answers `Unsettled`
    /// and an incoming one an analyzing error, both with the session
    /// kept. A refusal that is the engine's verdict on the request stays the
    /// walk's error once the walk's wait is spent. The retryable engine starts
    /// before the outgoing walk's deadline is taken ([`start_engine`]).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_retryable_refusal_at_a_spent_walk_wait_answers_unsettled() {
        const CONTENT_MODIFIED: i64 = -32_801;
        const INVALID_REQUEST: i64 = -32_600;
        let key = LspProcessKey::named("rust");

        let directory = tempfile::tempdir().expect("workspace");
        let pool = refusing_slot(directory.path(), CONTENT_MODIFIED);
        let slot = pool.engine_by_key(&key).expect("slot");
        start_engine(slot).await;
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let answer = slot
            .request_outgoing(
                open_lib,
                counted_prepare(&attempts),
                finish_immediately,
                started + Duration::from_secs(1),
            )
            .await
            .expect("a retryable refusal at the spent wait is an answer");
        eprintln!(
            "retryable refusal: elapsed={:?} answer={answer:?}",
            started.elapsed()
        );
        assert!(
            matches!(answer, OutgoingAnswer::Unsettled { attempts } if (2..=3).contains(&attempts)),
            "{answer:?}"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            match answer {
                OutgoingAnswer::Unsettled { attempts } => attempts,
                _ => unreachable!("matched above"),
            }
        );
        assert!(
            slot.state.lock().await.session.is_some(),
            "the session survives the spent wait"
        );

        let incoming = slot
            .request_settled(
                open_lib,
                counted_references(&attempts),
                finish_immediately,
                |_count| (true, false),
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .expect_err("the incoming walk reports the spent wait");
        assert!(
            incoming.slug() == errors::lsp::engine_analyzing::SLUG,
            "{incoming:?}"
        );
        assert!(slot.state.lock().await.session.is_some());
        pool.shutdown().await;

        let directory = tempfile::tempdir().expect("workspace");
        let pool = refusing_slot(directory.path(), INVALID_REQUEST);
        let slot = pool.engine_by_key(&key).expect("slot");
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deadline = Instant::now() + Duration::from_secs(1);
        let refused = slot
            .request_outgoing(
                open_lib,
                timed(&times, counted_prepare(&attempts)),
                finish_immediately,
                deadline,
            )
            .await
            .expect_err("the engine's verdict on the request stays the walk's error");
        let returned = Instant::now();
        let times = times.lock().expect("attempt times").clone();
        let retry = table("sh").retry;
        let made = u64::try_from(times.len()).expect("an attempt count");
        assert!(
            made < retry.attempts,
            "the walk ended inside the retry table, where a verdict waits a walk's waits: \
             {made} attempts"
        );
        assert_waits_under_the_deadline(&retry, deadline, &times, returned);
        assert!(
            refused.slug() == errors::lsp::engine_refused_terminal::SLUG
                && refused
                    .context()
                    .any(|(key, value)| key == "code" && value == INVALID_REQUEST.to_string()),
            "{refused:?}"
        );
        pool.shutdown().await;
    }

    /// A fake engine that announces one begin and end once initialized, then
    /// no progress again, appends every `workspace/didChangeWatchedFiles` body
    /// to `notified.log` and one line per start to `starts.log`, and answers
    /// every request with no location. With
    /// `watching`, it registers one `**/*.rs` file watcher, as rust-analyzer
    /// does; without, it registers none, as typescript-language-server does.
    #[cfg(unix)]
    fn fed_slot(directory: &Path, watching: bool) -> EnginePool {
        const SCRIPT: &str = r#"printf 'start\n' >> "$2"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}" ;;
    *'"method":"initialized"'*)
      REGISTER
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"method":"workspace/didChangeWatchedFiles"'*)
      printf '%s\n' "$body" >> "$1" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;
        const REGISTER: &str = r#"frame '{"jsonrpc":"2.0","id":"watch","method":"client/registerCapability","params":{"registrations":[{"id":"watch-rust","method":"workspace/didChangeWatchedFiles","registerOptions":{"watchers":[{"globPattern":"**/*.rs"}]}}]}}'"#;
        let script = directory.join("engine.sh");
        std::fs::write(
            &script,
            SCRIPT.replace("REGISTER", if watching { REGISTER } else { ":" }),
        )
        .expect("engine script");
        let mut configuration = table("sh");
        configuration.command = Some(CommandInput::ProgramAndArguments(vec![
            "sh".to_owned(),
            script.display().to_string(),
            directory.join("notified.log").display().to_string(),
            directory.join("starts.log").display().to_string(),
        ]));
        configuration.initialization_options = None;
        let key = LspProcessKey::named("rust");
        EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    /// One incoming read against `slot` under the default retry table: the
    /// answer's unconfirmed record, the attempts it took, and its time.
    #[cfg(unix)]
    async fn fed_read(slot: &EngineSlot) -> (bool, u64, Duration) {
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let (_answer, unconfirmed) = slot
            .request_settled(
                open_lib,
                counted_references(&attempts),
                finish_immediately,
                |_count| (true, false),
                started + Duration::from_secs(30),
            )
            .await
            .expect("the read answers");
        (
            unconfirmed,
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            started.elapsed(),
        )
    }

    /// One modified file no read opens, as a publication hands it on.
    fn edited_other() -> PathChanges {
        PathChanges::resolve(
            [(
                ProjectPath::new("other.rs").expect("path"),
                Some(rift_index::FileRecord::Digest(rift_core::FileDigest::of(
                    b"edited",
                ))),
            )],
            |_path| {
                Some(rift_index::FileRecord::Digest(rift_core::FileDigest::of(
                    b"held",
                )))
            },
        )
    }

    /// After a feed resets readiness, an engine that announces no progress
    /// reads unconfirmed, and the incoming read takes its repeated full report
    /// once the session is quiet past the 500 ms `settle_delay`, with the
    /// unconfirmed record, instead of spending the 9.75 s retry table.
    ///
    /// The attempt count carries the bound, not the clock: the first attempt
    /// has no report to repeat, and the wait after the second runs the table's
    /// 500 ms, one whole `settle_delay`, unless the session reads quiet sooner,
    /// so the third attempt's answer is read quiet and settles however long
    /// each request takes.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_incoming_read_after_a_feed_settles_once_quiet_past_the_settle_delay() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = fed_slot(directory.path(), true);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        let (unconfirmed, attempts, elapsed) = fed_read(slot).await;
        eprintln!("before the feed: attempts={attempts} elapsed={elapsed:?}");
        assert!(!unconfirmed, "the engine read ready before the feed");

        pool.owe_changed_paths(&edited_other());
        let (unconfirmed, attempts, elapsed) = fed_read(slot).await;
        eprintln!(
            "after the feed: attempts={attempts} elapsed={elapsed:?} unconfirmed={unconfirmed}"
        );
        let notified = std::fs::read_to_string(directory.path().join("notified.log"))
            .expect("the engine was told");
        assert!(notified.contains("other.rs"), "{notified}");
        assert!(unconfirmed, "the read records the engine unconfirmed");
        assert!(
            elapsed >= Duration::from_millis(500),
            "the read settles once quiet past the `settle_delay` after the feed: {elapsed:?}"
        );
        assert!(
            (2..=3).contains(&attempts),
            "the read takes its repeated report on its second or third attempt: {attempts}"
        );
        pool.shutdown().await;
    }

    /// An engine that registered no watcher is told nothing of a feed, so its
    /// readiness stands: the next incoming read settles ready on its first attempt,
    /// with no unconfirmed record.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_engine_with_no_watcher_keeps_its_readiness_after_a_feed() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = fed_slot(directory.path(), false);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        let (unconfirmed, _attempts, _elapsed) = fed_read(slot).await;
        assert!(!unconfirmed, "the engine read ready before the feed");

        pool.owe_changed_paths(&edited_other());
        let (unconfirmed, attempts, elapsed) = fed_read(slot).await;
        eprintln!(
            "unwatched after the feed: attempts={attempts} elapsed={elapsed:?} unconfirmed={unconfirmed}"
        );
        assert!(
            !directory.path().join("notified.log").exists(),
            "no registered watcher matched, so nothing was sent"
        );
        assert!(!unconfirmed);
        assert_eq!(attempts, 1, "the engine still reads ready");
        pool.shutdown().await;
    }

    /// An incoming report that never settles - a ready engine answering no reference
    /// beyond the seed's own - keeps the walk waiting until its deadline, and the spent
    /// wait answers an analyzing error with the session kept, as a wait spent on
    /// an engine still analyzing does. The engine starts before the deadline is taken
    /// ([`start_engine`]).
    #[cfg(unix)]
    #[tokio::test]
    async fn an_incoming_report_that_never_settles_ends_at_the_spent_wait_with_the_session_kept() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = fed_slot(directory.path(), false);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        start_engine(slot).await;
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let times = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deadline = Instant::now() + Duration::from_secs(1);
        let spent = slot
            .request_settled(
                open_lib,
                timed(&times, counted_references(&attempts)),
                finish_immediately,
                |_count| (true, true),
                deadline,
            )
            .await
            .expect_err("an empty report from a ready engine never settles inside a second");
        let returned = Instant::now();
        let times = times.lock().expect("attempt times").clone();
        let made = attempts.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            spent.context().any(|(key, value)| {
                key == "attempts" && value == made.to_string() && made >= 2
            }),
            "{made} attempts: {spent:?}"
        );
        assert_waits_under_the_deadline(&table("sh").retry, deadline, &times, returned);
        assert!(
            slot.state.lock().await.session.is_some(),
            "the session survives the spent wait"
        );
        assert_ne!(pool.state_for_key(&key), Some(LspState::Failed));
        pool.shutdown().await;
    }

    /// More changed paths than `OWED_CHANGES_MAX`, recorded while a request
    /// waits between two attempts, tell the live session nothing: the slot
    /// replaces it at the next attempt, which claims one restart, and `begin`
    /// opens the document again on the replacement.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_slot_past_the_owed_changes_bound_replaces_its_live_session() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = fed_slot(directory.path(), true);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        fed_read(slot).await;

        let begins = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted_begins = Arc::clone(&begins);
        let asked = Arc::new(tokio::sync::Notify::new());
        let first_ask = Arc::clone(&asked);
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut references = counted_references(&attempts);
        let held = rift_index::FileRecord::Digest(rift_core::FileDigest::of(b"held"));
        let beyond_the_bound = PathChanges::resolve(
            (0..=OWED_CHANGES_MAX).map(|index| {
                let path = ProjectPath::new(format!("f{index}.rs")).expect("path");
                (path, Some(held.clone()))
            }),
            |_path| None,
        );
        let request = slot.request_settled(
            move |session: &mut EngineSession| {
                counted_begins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                open_lib(session)
            },
            move |session: &mut EngineSession| {
                first_ask.notify_one();
                references(session)
            },
            finish_immediately,
            |_count| (false, false),
            Instant::now() + Duration::from_secs(2),
        );
        let owe = async {
            asked.notified().await;
            pool.owe_changed_paths(&beyond_the_bound);
        };
        let (spent, ()) = tokio::join!(request, owe);
        let spent = spent.expect_err("a partial report never settles");
        assert!(
            spent.context().any(|(key, _)| key == "attempts"),
            "{spent:?}"
        );
        assert!(
            !directory.path().join("notified.log").exists(),
            "past the bound the live session is told nothing"
        );
        let starts = std::fs::read_to_string(directory.path().join("starts.log"))
            .expect("the engine records its starts");
        assert_eq!(starts.lines().count(), 2, "the slot replaced its session");
        assert_eq!(
            begins.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the replacement opened the document again"
        );
        pool.shutdown().await;
    }

    /// One embedded walk from `root` in `directory`'s `graph.py`, opened as
    /// it stands on disk: the answer, the attempts it took, and its time.
    async fn walk_from_root(slot: &EngineSlot, directory: &Path) -> (OutgoingAnswer<usize>, u64) {
        let source = std::fs::read_to_string(directory.join("graph.py")).expect("source reads");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = Arc::clone(&attempts);
        let started = Instant::now();
        let answer = slot
            .request_outgoing(
                move |session: &mut EngineSession| {
                    let source = source.clone();
                    Box::pin(async move {
                        let path = rift_core::ProjectPath::new("graph.py").expect("path");
                        session.open(&path, "python", source).await
                    })
                },
                move |session: &mut EngineSession| {
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async move {
                        let path = rift_core::ProjectPath::new("graph.py").expect("path");
                        let position = lsp_types::Position {
                            line: 3,
                            character: 4,
                        };
                        let items = session.prepare_call_hierarchy(&path, position).await?;
                        let Some(item) = items.into_iter().next() else {
                            return Ok(None);
                        };
                        Ok(Some(session.outgoing_calls(item).await?.len()))
                    })
                },
                finish_immediately,
                started + Duration::from_secs(30),
            )
            .await
            .expect("the walk settles");
        (answer, attempts.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// A pool serving the embedded engine for Python over `directory`, and
    /// the key of its one slot.
    fn embedded_pool(directory: &Path) -> (EnginePool, LspProcessKey) {
        let mut configuration = table("ty");
        configuration.command = None;
        configuration.embedded = Some(EmbeddedEngine::Ty);
        configuration.initialization_options = None;
        let key = LspProcessKey::named("python");
        let pool = EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("python".to_owned(), key.clone())]),
        );
        (pool, key)
    }

    /// The embedded engine declares itself ready at its start, so its
    /// first walk answers `Ready` on the first attempt instead of reading
    /// unconfirmed until the 500 ms `settle_delay` passes. `Ready` is the
    /// settlement of an engine reading ready alone, and one attempt means no
    /// wait ran before it.
    #[tokio::test]
    async fn the_embedded_engine_walks_ready_on_its_first_attempt() {
        let directory = tempfile::tempdir().expect("workspace");
        let source = "def leaf() -> int:\n    return 1\n\ndef root() -> int:\n    return leaf()\n";
        std::fs::write(directory.path().join("graph.py"), source).expect("source");
        let (pool, key) = embedded_pool(directory.path());
        let slot = pool.engine_by_key(&key).expect("slot");
        let (answer, attempts) = walk_from_root(slot, directory.path()).await;
        assert_eq!(answer, OutgoingAnswer::Ready(1), "root calls leaf");
        assert_eq!(attempts, 1);
        assert_eq!(pool.state_for_key(&key), Some(LspState::Ready));
        pool.shutdown().await;
    }

    /// The embedded engine's ready declaration survives a file change,
    /// since it answers from its database at request time. A walk right after
    /// an edit answers `Ready` with the edit's callees on its first attempt,
    /// not `Unconfirmed` after the 500 ms `settle_delay`.
    #[tokio::test]
    async fn the_embedded_engine_walks_ready_on_its_first_attempt_after_a_change() {
        let directory = tempfile::tempdir().expect("workspace");
        let graph = directory.path().join("graph.py");
        std::fs::write(
            &graph,
            "def leaf() -> int:\n    return 1\n\ndef root() -> int:\n    return leaf()\n",
        )
        .expect("source");
        let (pool, key) = embedded_pool(directory.path());
        let slot = pool.engine_by_key(&key).expect("slot");
        let (before, _) = walk_from_root(slot, directory.path()).await;
        assert_eq!(before, OutgoingAnswer::Ready(1), "root calls leaf");

        std::fs::write(
            &graph,
            "def leaf() -> int:\n    return 1\n\ndef root() -> int:\n    return leaf() + twig()\n\ndef twig() -> int:\n    return 2\n",
        )
        .expect("edit");
        let readiness = slot
            .request(|session: &mut EngineSession| {
                Box::pin(async move {
                    let path = rift_core::ProjectPath::new("graph.py").expect("path");
                    session
                        .notify_changed_paths(&[(path, lsp_types::FileChangeType::CHANGED)])
                        .await?;
                    Ok((session.readiness(), session.walk_readiness()))
                })
            })
            .await
            .expect("the change reaches the session");
        assert_eq!(
            readiness,
            (EngineReadiness::Ready, EngineReadiness::Ready),
            "the declaration survives the change"
        );

        let (after, attempts) = walk_from_root(slot, directory.path()).await;
        assert_eq!(after, OutgoingAnswer::Ready(2), "root calls leaf and twig");
        assert_eq!(attempts, 1);
        assert_eq!(pool.state_for_key(&key), Some(LspState::Ready));
        pool.shutdown().await;
    }

    /// The index's three classifications reach the engine as created, changed and
    /// deleted; a later change to one path replaces the earlier one; past
    /// `OWED_CHANGES_MAX` paths the slot stops recording and marks the bound spent.
    #[test]
    fn owed_changes_classify_merge_and_mark_the_bound_spent() {
        let path = |name: &str| ProjectPath::new(name).expect("path");
        let record =
            |bytes: &[u8]| rift_index::FileRecord::Digest(rift_core::FileDigest::of(bytes));
        let held = record(b"held");
        let first = PathChanges::resolve(
            [
                (path("added.rs"), Some(record(b"new"))),
                (path("changed.rs"), Some(record(b"edited"))),
                (path("removed.rs"), None),
            ],
            |observed| (observed.as_str() != "added.rs").then(|| held.clone()),
        );
        let mut owed = OwedChanges::default();
        owed.record(&first);
        let recorded = |owed: &OwedChanges| {
            owed.paths
                .iter()
                .map(|(path, change)| (path.as_str().to_owned(), *change))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            recorded(&owed),
            [
                ("added.rs".to_owned(), FileChangeType::CREATED),
                ("changed.rs".to_owned(), FileChangeType::CHANGED),
                ("removed.rs".to_owned(), FileChangeType::DELETED),
            ]
        );
        owed.record(&PathChanges::resolve([(path("added.rs"), None)], |_| {
            Some(held.clone())
        }));
        assert_eq!(
            owed.paths.get(&path("added.rs")),
            Some(&FileChangeType::DELETED),
            "the later change replaces the earlier one"
        );

        let many = PathChanges::resolve(
            (0..=OWED_CHANGES_MAX).map(|index| (path(&format!("f{index}.rs")), Some(held.clone()))),
            |_| None,
        );
        let mut owed = OwedChanges::default();
        owed.record(&many);
        assert!(
            owed.overflowed && owed.paths.is_empty(),
            "{:?}",
            owed.paths.len()
        );
        owed.record(&first);
        assert!(
            owed.overflowed && owed.paths.is_empty(),
            "a spent bound records nothing more"
        );
    }

    #[test]
    fn diagnostic_settlement_covers_progress_and_stable_full_reports() {
        use EngineReadiness::{Analyzing, Ready, Unconfirmed};
        // (name, readiness, full, empty, repeated, final attempt, quiet, expected)
        let rows = [
            (
                "stale nonempty",
                Unconfirmed,
                true,
                false,
                false,
                false,
                false,
                Settlement::Retry,
            ),
            (
                "stale empty",
                Unconfirmed,
                true,
                true,
                false,
                true,
                false,
                Settlement::Retry,
            ),
            (
                "progress start",
                Analyzing,
                true,
                false,
                true,
                true,
                true,
                Settlement::Retry,
            ),
            (
                "progress end",
                Ready,
                true,
                false,
                false,
                false,
                false,
                Settlement::Ready,
            ),
            (
                "progress end empty",
                Ready,
                true,
                true,
                false,
                false,
                true,
                Settlement::Retry,
            ),
            (
                "oscillating",
                Unconfirmed,
                true,
                false,
                false,
                true,
                false,
                Settlement::Retry,
            ),
            (
                "stable before bound",
                Unconfirmed,
                true,
                false,
                true,
                false,
                false,
                Settlement::Retry,
            ),
            (
                "stable at bound",
                Unconfirmed,
                true,
                false,
                true,
                true,
                false,
                Settlement::Ready,
            ),
            (
                "partial",
                Unconfirmed,
                false,
                false,
                true,
                true,
                true,
                Settlement::Retry,
            ),
        ];
        assert_settlements(&rows);
    }

    /// An unconfirmed engine's repeated full report settles once quiet past
    /// `settle_delay`, empty or not, and a ready engine's rules are unchanged.
    #[test]
    fn diagnostic_settlement_takes_a_quiet_unconfirmed_repeated_report() {
        use EngineReadiness::{Ready, Unconfirmed};
        let rows = [
            (
                "quiet once",
                Unconfirmed,
                true,
                false,
                false,
                false,
                true,
                Settlement::Retry,
            ),
            (
                "quiet repeated",
                Unconfirmed,
                true,
                false,
                true,
                false,
                true,
                Settlement::Unconfirmed,
            ),
            (
                "quiet repeated empty",
                Unconfirmed,
                true,
                true,
                true,
                false,
                true,
                Settlement::Unconfirmed,
            ),
            (
                "quiet repeated at bound",
                Unconfirmed,
                true,
                false,
                true,
                true,
                true,
                Settlement::Unconfirmed,
            ),
            (
                "quiet partial",
                Unconfirmed,
                false,
                false,
                true,
                false,
                true,
                Settlement::Retry,
            ),
            (
                "ready quiet repeated empty",
                Ready,
                true,
                true,
                true,
                false,
                true,
                Settlement::Retry,
            ),
        ];
        assert_settlements(&rows);
    }

    /// One settlement row: name, readiness, full, empty, repeated, final attempt,
    /// quiet, and the expected verdict.
    type SettlementRow = (
        &'static str,
        EngineReadiness,
        bool,
        bool,
        bool,
        bool,
        bool,
        Settlement,
    );

    fn assert_settlements(rows: &[SettlementRow]) {
        for &(name, readiness, full, empty, repeated, final_attempt, quiet, expected) in rows {
            assert_eq!(
                diagnostic_settlement(
                    readiness,
                    DiagnosticEvidence {
                        shape: ReportShape::from_report(full, empty),
                        repeated,
                        final_attempt,
                        quiet,
                    },
                ),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn launch_carries_the_accepted_table_verbatim() {
        let mut configuration = table("uvx");
        configuration.command = Some(CommandInput::ProgramAndArguments(vec![
            "uvx".to_owned(),
            "ty".to_owned(),
            "server".to_owned(),
        ]));
        let built = pool(vec![("ty", configuration, &["python"])]);
        let slot = built
            .engine_for(&language("python", None))
            .expect("python is served");
        let launch = slot.launch(
            slot.configuration()
                .command
                .as_ref()
                .expect("the fixture names a command"),
        );
        assert_eq!(launch.program, "uvx");
        assert_eq!(launch.arguments, ["ty", "server"]);
        assert_eq!(launch.startup_timeout, Duration::from_secs(10));
        assert_eq!(launch.request_timeout, Duration::from_secs(20));
        assert_eq!(launch.stderr_capture_bytes, 2_048);
        assert_eq!(
            launch.initialization_options,
            Some(serde_json::json!({ "engine": "fake" }))
        );
    }
}
