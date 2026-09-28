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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lsp_types::FileChangeType;
use rift_core::{Error, ErrorCode, ErrorName, ProjectPath};
use rift_index::{PathChange, PathChanges};
use rift_lsp::session::{EngineError, EngineFault, EngineLaunch, EngineSession};
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

fn begin_immediately(_session: &mut EngineSession) -> SessionFuture<'_, Result<(), EngineError>> {
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
                tracing::warn!(component = "engine", %error, "an engine shutdown task failed");
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
    /// Returns [`EngineError`] when the session ended or the connection broke.
    async fn send(self, session: &mut EngineSession) -> Result<(), EngineError> {
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

/// Everything one slot's lock guards: the running engine and the restarts
/// already spent on it.
#[derive(Debug, Default)]
struct SlotState {
    session: Option<EngineSession>,
    restarts: RestartBudget,
}

/// Keeps a session reusable only after its exchange finishes.
///
/// Cancellation can skip asynchronous document closes. Dropping this guard then drops
/// the session under the slot lock, so the next request starts with no open documents.
struct RequestSessionGuard<'slot> {
    state: &'slot mut SlotState,
    reported_state: &'slot watch::Sender<LspState>,
    finished: bool,
}

impl Drop for RequestSessionGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            drop(self.state.session.take());
            self.reported_state.send_replace(LspState::Failed);
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
    Refused(EngineError),
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
            Self::Refused(refusal) => refusal.fault().is_retryable_refusal(),
            Self::AnsweredNothing(_) => false,
        }
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
/// A configuration fault - an empty program, an absolute one - answers the
/// same way every time, so it surfaces at once instead of spending the
/// restart budget on a start that cannot succeed.
/// The causes under one start failure, joined for a record.
///
/// An engine failure renders its registry text, which names the fault and the
/// caller's next step. The operating error behind it - the missing program, the
/// refused permission - lives in the source chain alone, and that is what an
/// operator reading `rift://logs` needs.
fn start_cause(failure: &EngineError) -> String {
    rift_core::causes(failure).join(": ")
}

fn restart_may_help(error: &EngineError) -> bool {
    error.name() != ErrorName::Wire(ErrorCode::ConfigurationInvalid)
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
    async fn end_session(self: Arc<Self>) {
        let mut held = self.state.lock().await;
        let Some(session) = held.session.take() else {
            return;
        };
        let stderr = session.shutdown().await;
        self.report_state(LspState::Stopped);
        let engine = self.name();
        tracing::debug!(
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
    /// Dropping the future discards its session. The next request starts a replacement
    /// within the configured restart budget.
    pub async fn request<T>(
        &self,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<T, EngineError>> + Send + 'session>,
        >,
    ) -> Result<T, EngineError> {
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
    /// Dropping the future can skip `finish`, so its session is discarded. The next
    /// request starts a replacement within the configured restart budget.
    pub async fn request_exchange<T>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), EngineError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<T, EngineError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
    ) -> Result<T, EngineError> {
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
    /// [`EngineFault::Analyzing`] and the session kept. A wait that
    /// follows an attempt the engine answered while analyzing ends once the
    /// session no longer reads analyzing, and one that follows an unconfirmed
    /// attempt read before the quiet ends once the session reads quiet.
    ///
    /// An unconfirmed engine's repeated full report is taken once the session
    /// is quiet past `settle_delay`, and the answer comes back with `true`, so
    /// the read records the engine unconfirmed. An unconfirmed session
    /// has seen no progress transition since the change, so the walk's quiet
    /// ([`EngineSession::walk_is_quiet`]) is the quiet of every token,
    /// flycheck included.
    ///
    /// `answer_version` reads the document version one answer names, when it
    /// names one at all - `textDocument/publishDiagnostics` carries an
    /// optional `version`, and a caller may ride that alongside an answer
    /// for the same document even when the answer's own wire shape carries
    /// no version of its own. An answer whose named version differs from
    /// the version this exchange opened the document with describes the
    /// engine's previous open: it is discarded before settlement runs, so
    /// it never becomes the returned answer and never counts toward
    /// `repeated`, and the exchange keeps waiting for a report of its own
    /// open. An answer naming no version - most engines never publish one -
    /// is judged exactly as it was before this gate existed.
    ///
    /// # Errors
    ///
    /// Returns operation failure, retry refusal, unready exhaustion, start
    /// failure, or ended session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future can skip `finish`, so its session is discarded. The next
    /// request starts a replacement within the configured restart budget.
    pub async fn request_settled<T: PartialEq>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), EngineError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<T, EngineError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        mut report_state: impl FnMut(&T) -> (bool, bool),
        mut answer_version: impl FnMut(&T) -> Option<i32>,
        deadline: Instant,
    ) -> Result<(T, bool), EngineError> {
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
                    let stale = answer_version(&answer)
                        .is_some_and(|version| version != session.document_version());
                    if stale {
                        return Answer::Retry(Transient::Unready);
                    }
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
    /// live, and the answer is [`OutgoingAnswer::Unsettled`] instead of a
    /// dropped request that discards the session.
    ///
    /// # Errors
    ///
    /// Returns operation failure, exhausted refusal, start failure, or ended
    /// session.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future can skip `finish`, so its session is discarded. The next
    /// request starts a replacement within the configured restart budget.
    pub async fn request_outgoing<T>(
        &self,
        begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), EngineError>>,
        operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        )
            -> SessionFuture<'session, Result<Option<T>, EngineError>>,
        finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        deadline: Instant,
    ) -> Result<OutgoingAnswer<T>, EngineError> {
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
            Err(error) => match error.fault() {
                EngineFault::Analyzing { attempts } => Ok(OutgoingAnswer::Unsettled {
                    attempts: *attempts,
                }),
                _ => Err(error),
            },
        }
    }

    /// Shared bounded request loop.
    ///
    /// Without `deadline`, the retry table's attempt bound ends the loop; with
    /// it, [`walk_wait`] does while the absorbed condition can mean the engine
    /// is still loading ([`Transient::is_loading`]), and the retry table's
    /// bound or `deadline`, whichever comes first, does otherwise. The wait
    /// between two attempts reads engine output
    /// ([`EngineSession::read_output`]), so progress is stamped when it
    /// arrives; the wait also ends once `wake` holds.
    ///
    /// A canceled request discards its session before releasing the slot lock. A later
    /// request spends the configured restart budget to start without open documents.
    async fn request_deciding<T>(
        &self,
        mut begin: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> SessionFuture<'session, Result<(), EngineError>>,
        mut operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        )
            -> SessionFuture<'session, Result<T, EngineError>>,
        mut finish: impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()>,
        deadline: Option<Instant>,
        wake: impl Fn(&EngineSession) -> bool,
        mut decide: impl FnMut(&mut EngineSession, u64, T, bool) -> Answer<T>,
    ) -> Result<T, EngineError> {
        let retry = self.configuration.retry;
        let mut held = self.state.lock().await;
        let mut guarded = RequestSessionGuard {
            state: &mut held,
            reported_state: &self.reported_state,
            finished: false,
        };
        let mut attempt: u64 = 1;
        let mut reported: Option<EngineError> = None;
        let mut exchange_started = false;
        let mut session_generation = 0_u64;
        loop {
            let state = &mut *guarded.state;
            let owed = self.take_owed();
            let session = match state.session.take() {
                Some(running) if !running.is_ended() && !owed.overflowed => {
                    let running = state.session.insert(running);
                    if let Err(error) = owed.send(running).await {
                        reported = Some(error);
                        continue;
                    }
                    running
                }
                dead => {
                    if let Some(dead) = dead {
                        self.reap(dead).await;
                    }
                    // A replacement opens nothing on its own, so `begin` runs on it
                    // again, even when the session it replaces was still live.
                    exchange_started = false;
                    let started = self
                        .start_within_budget(&mut state.restarts, reported.take())
                        .await?;
                    state.session.insert(started)
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
                    Err(error) => return Err(error),
                }
            }
            self.report_readiness(session.readiness());
            let outcome = operation(session).await;
            let ended = session.is_ended();
            if ended {
                self.report_state(LspState::Failed);
            } else {
                self.report_readiness(session.readiness());
            }
            let final_attempt = retry.delay_after(attempt).is_none();
            let absorbed = match outcome {
                Ok(answer) => match decide(session, session_generation, answer, final_attempt) {
                    Answer::Ready(answer) => {
                        finish(session).await;
                        guarded.finished = true;
                        return Ok(answer);
                    }
                    Answer::Retry(absorbed) => absorbed,
                },
                Err(error) if ended => {
                    exchange_started = false;
                    reported = Some(error);
                    continue;
                }
                Err(error) if error.fault().is_refusal() => Transient::Refused(error),
                Err(error) => {
                    finish(session).await;
                    guarded.finished = true;
                    return Err(error);
                }
            };
            let wait = match deadline {
                Some(deadline) if absorbed.is_loading() => {
                    walk_wait(&retry, attempt, Instant::now(), deadline)
                }
                Some(deadline) => retry
                    .delay_after(attempt)
                    .filter(|wait| Instant::now() + *wait < deadline),
                None => retry.delay_after(attempt),
            };
            let Some(wait) = wait else {
                finish(session).await;
                guarded.finished = true;
                return self.exhausted(absorbed, attempt, deadline.is_some());
            };
            let waited = session.read_output(Instant::now() + wait, &wake).await;
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
    /// [`EngineFault::Analyzing`], as a wait spent on analyzing answers does:
    /// rust-analyzer answers `-32801` content modified while it loads, and an
    /// empty prepare at the same moment means the same load.
    fn exhausted<T>(
        &self,
        absorbed: Transient<T>,
        attempts: u64,
        walk: bool,
    ) -> Result<T, EngineError> {
        let engine = self.name();
        match absorbed {
            Transient::Refused(refusal) if walk && refusal.fault().is_retryable_refusal() => {
                tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    refusal = %refusal,
                    "language engine refused with a retryable code when the walk's wait was spent"
                );
                Err(Error::new(EngineFault::Analyzing { attempts }))
            }
            Transient::Analyzing | Transient::Unready => {
                tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    "language engine was not ready on every attempt"
                );
                Err(Error::new(EngineFault::Analyzing { attempts }))
            }
            Transient::Refused(refusal) => {
                tracing::warn!(
                    component = "engine",
                    engine,
                    attempts,
                    "language engine refused every configured attempt"
                );
                Err(refusal)
            }
            Transient::AnsweredNothing(answer) => {
                tracing::debug!(
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
    /// The loop runs at most `restart.attempts` + 1 times: each pass
    /// claims one start, and a refused claim ends it. A refused claim
    /// surfaces `reported` - the failure that sent the caller back here -
    /// or [`EngineFault::Ended`] when this call has no failure of its own
    /// to report, which is the honest answer for a budget an earlier
    /// request already spent.
    ///
    /// Every start that fails is recorded before this returns, cause included.
    /// The refusal reaches the caller, and `rift://logs` is where the agent
    /// holding that refusal looks for the program that could not run.
    async fn start_within_budget(
        &self,
        budget: &mut RestartBudget,
        mut reported: Option<EngineError>,
    ) -> Result<EngineSession, EngineError> {
        loop {
            if !budget.claim(&self.configuration.restart, Instant::now()) {
                self.report_state(LspState::Failed);
                let engine = self.name();
                let surfaced = reported
                    .as_ref()
                    .map_or_else(String::new, ToString::to_string);
                let cause = reported.as_ref().map_or_else(String::new, start_cause);
                tracing::warn!(
                    component = "engine",
                    engine,
                    program = self.program(),
                    attempts = self.configuration.restart.attempts,
                    error = surfaced,
                    cause,
                    "language engine restart budget is spent for this window"
                );
                return Err(reported.unwrap_or_else(|| Error::new(EngineFault::Ended)));
            }
            self.report_state(LspState::Starting);
            let started = match (
                self.configuration.embedded,
                self.configuration.command.as_ref(),
            ) {
                (Some(engine), _) => {
                    crate::embedded::started_session(
                        self.embedded_launch(engine),
                        &self.workspace_root,
                    )
                    .await
                }
                (None, Some(command)) => {
                    EngineSession::start(self.launch(command), &self.workspace_root).await
                }
                (None, None) => unreachable!(
                    "acceptance refuses an LSP table naming neither command nor embedded"
                ),
            };
            match started {
                Ok(session) => {
                    self.report_readiness(session.readiness());
                    return Ok(session);
                }
                Err(failure) if !restart_may_help(&failure) => {
                    self.report_state(LspState::Failed);
                    self.record_start_failure(&failure, false);
                    return Err(failure);
                }
                Err(failure) => {
                    self.record_start_failure(&failure, true);
                    reported = Some(failure);
                }
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
    fn record_start_failure(&self, failure: &EngineError, retrying: bool) {
        tracing::warn!(
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
            tracing::warn!(
                component = "engine",
                engine,
                stderr = %stderr.text,
                "language engine ended and was reaped"
            );
        } else {
            tracing::info!(
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

    /// Collects the records one test's engine start emits, so a test reads what
    /// `rift://logs` would carry without opening a store.
    #[derive(Clone, Default)]
    struct RecordedEvents(Arc<std::sync::Mutex<Vec<String>>>);

    impl RecordedEvents {
        /// The records emitted so far, one rendered line each.
        fn lines(&self) -> Vec<String> {
            self.0.lock().expect("the recorder is not poisoned").clone()
        }

        /// The one record whose message matches, or a panic naming everything seen.
        fn naming(&self, message: &str) -> String {
            let lines = self.lines();
            lines
                .iter()
                .find(|line| line.contains(message))
                .unwrap_or_else(|| panic!("no record says {message:?}: {lines:#?}"))
                .clone()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RecordedEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Rendered(String);
            impl tracing::field::Visit for Rendered {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, " {}={value:?}", field.name());
                }
            }
            let mut rendered = Rendered(event.metadata().level().to_string());
            event.record(&mut rendered);
            self.0
                .lock()
                .expect("the recorder is not poisoned")
                .push(rendered.0);
        }
    }

    /// Serves one slot whose configured program is `command`, and returns the refusal
    /// a request earns beside every record the attempt emitted.
    async fn start_refusal(command: &str, attempts: u64) -> (EngineError, RecordedEvents) {
        use tracing_subscriber::layer::SubscriberExt as _;

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
        let recorded = RecordedEvents::default();
        let failure = {
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(recorded.clone()),
            );
            slot.request(|session| Box::pin(async move { Ok(session.document_version()) }))
                .await
                .expect_err("a program that cannot start answers nothing")
        };
        pool.shutdown().await;
        (failure, recorded)
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
            matches!(failure.fault(), EngineFault::LaunchFailed { .. }),
            "a missing program answers launch_failed: {failure:?}"
        );
        let record = recorded.naming("language engine did not start");
        assert!(record.starts_with("WARN"), "{record}");
        assert!(
            record.contains("component=\"engine\"")
                && record.contains(&format!("program=\"{MISSING_PROGRAM}\""))
                && record.contains(&missing_program_cause()),
            "the record names the component, the program and the cause: {record}"
        );
    }

    /// The spent budget names the failure it is surfacing. On its own it said the budget was
    /// spent, which is the outcome and not the reason the caller was refused.
    #[tokio::test]
    async fn the_spent_restart_budget_names_the_failure_it_surfaces() {
        let (_, recorded) = start_refusal(MISSING_PROGRAM, 1).await;
        let record = recorded.naming("restart budget is spent");
        assert!(
            record.contains(&format!("program=\"{MISSING_PROGRAM}\""))
                && record.contains(&missing_program_cause()),
            "the budget record carries the cause: {record}"
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
        let record = recorded.naming("language engine did not start");
        assert!(
            record.contains("retrying=false")
                && record.contains("program=\"/rift-engine-absolute\""),
            "the record names the program and that no restart follows: {record}"
        );
        assert!(
            !recorded
                .lines()
                .iter()
                .any(|line| line.contains("restart budget is spent")),
            "the budget is untouched: {:#?}",
            recorded.lines()
        );
    }

    #[tokio::test]
    async fn canceled_open_document_discards_session_and_preserves_restart_budget() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(directory.path().join("a.py"), "def beacon(): return 1\n").expect("source");
        std::fs::write(
            directory.path().join("b.py"),
            "from a import beacon\nvalue = beacon()\n",
        )
        .expect("caller");
        let configuration: LspConfiguration = serde_json::from_value(serde_json::json!({
            "embedded": "ty", "restart": { "attempts": 1 }
        }))
        .expect("embedded configuration");
        let key = LspProcessKey::named("python");
        let pool = EnginePool::new(
            directory.path(),
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("python".to_owned(), key.clone())]),
        );
        let slot = pool.engine_by_key(&key).expect("slot");
        let opened = Arc::new(tokio::sync::Notify::new());
        {
            let operation_started = Arc::clone(&opened);
            let exchange = slot.request_exchange(
                |session| {
                    Box::pin(async move {
                        session
                            .open(
                                &rift_core::ProjectPath::new("a.py").expect("path"),
                                "python",
                                "def beacon(): return 1\n".to_owned(),
                            )
                            .await
                    })
                },
                move |_session| {
                    let started = Arc::clone(&operation_started);
                    Box::pin(async move {
                        started.notify_one();
                        std::future::pending::<Result<(), EngineError>>().await
                    })
                },
                |session| {
                    Box::pin(async move {
                        let _ = session
                            .close(&rift_core::ProjectPath::new("a.py").expect("path"))
                            .await;
                    })
                },
            );
            tokio::pin!(exchange);
            tokio::select! {
                result = &mut exchange => panic!("operation must wait: {result:?}"),
                () = opened.notified() => {},
            }
        }
        assert!(
            slot.state.lock().await.session.is_none(),
            "cancellation must drop open documents"
        );
        assert_eq!(pool.state_for_key(&key), Some(LspState::Failed));
        std::fs::write(directory.path().join("a.py"), "\ndef beacon(): return 2\n")
            .expect("changed source");
        let version = slot
            .request(|session| {
                Box::pin(async move {
                    session
                        .open(
                            &rift_core::ProjectPath::new("b.py").expect("path"),
                            "python",
                            "from a import beacon\nvalue = beacon()\n".to_owned(),
                        )
                        .await?;
                    Ok(session.document_version())
                })
            })
            .await
            .expect("remaining restart starts a clean session");
        assert_eq!(version, 1, "the other document opens in a new session");
        let state = slot.state.lock().await;
        assert_eq!(
            state.restarts.spent.len(),
            1,
            "cancellation spends the existing restart budget"
        );
        drop(state);
        pool.shutdown().await;
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
    fn a_configuration_fault_is_the_one_failure_no_restart_helps() {
        let absolute = Error::new(EngineFault::ProgramAbsolute {
            program: "/usr/bin/engine".to_owned(),
        });
        assert!(!restart_may_help(&absolute));
        assert!(!restart_may_help(&Error::new(EngineFault::ProgramEmpty)));
        assert!(restart_may_help(&Error::new(EngineFault::Ended)));
        assert!(restart_may_help(&Error::new(EngineFault::TimedOut {
            method: "textDocument/rename".to_owned(),
            timeout_ms: 1_000,
        })));
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

    /// A `sh` engine that answers `initialize` and then announces work, answers
    /// each request with no location once it is sent, and ends its work at the
    /// first request `analyzing_secs` or more whole seconds after its start:
    /// the `$/progress` end and that request's answer arrive in one read, as
    /// they would from an engine whose end landed between two requests.
    #[cfg(unix)]
    fn analyzing_engine(directory: &Path, analyzing_secs: u64) -> LspConfiguration {
        const SCRIPT: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
start=$(date +%s)
ended=0
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*)
      if [ "$ended" -eq 0 ] && [ $(( $(date +%s) - start )) -ge ANALYZING_SECS ]; then
        frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}'
        ended=1
      fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;
        let script = directory.join("engine.sh");
        std::fs::write(
            &script,
            SCRIPT.replace("ANALYZING_SECS", &analyzing_secs.to_string()),
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
    fn analyzing_slot(directory: &Path, analyzing_secs: u64) -> EnginePool {
        let key = LspProcessKey::named("rust");
        EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), analyzing_engine(directory, analyzing_secs))]),
            BTreeMap::from([("rust".to_owned(), key)]),
        )
    }

    /// One references request against the analyzing engine, counted.
    #[cfg(unix)]
    fn counted_references(
        attempts: &Arc<std::sync::atomic::AtomicU64>,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<Option<usize>, EngineError>> {
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
    fn open_lib(session: &mut EngineSession) -> SessionFuture<'_, Result<(), EngineError>> {
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

    /// An exchange that is no walk ends at the retry table's 8th attempt,
    /// 9.75 s of waits in, while the engine reads analyzing until 13 s: an
    /// engine whose load passes 9.75 s answers such a request with
    /// [`EngineFault::Analyzing`], well inside the 30 s `readiness_timeout`.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_analyzing_engine_spends_the_retry_table_before_the_readiness_timeout() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 13);
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
            "exchange: attempts={attempts:?} elapsed={elapsed:?} fault={:?}",
            failure.fault()
        );
        assert!(matches!(
            failure.fault(),
            EngineFault::Analyzing { attempts: 8 }
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 8);
        assert!(elapsed >= Duration::from_millis(9_750) && elapsed < Duration::from_secs(13));
        pool.shutdown().await;
    }

    /// A walk waits under the deadline, past the retry table's attempt
    /// bound, and reads ready one `settle_delay` after the attempt
    /// that read the engine's end: this engine sends its end with an answer,
    /// about 13.75 s in at attempt 10, and the wait that follows reads the
    /// engine quiet 0.5 s later and ends there, so attempt 11 settles. The
    /// engine's whole-second clock can end its work one attempt early.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_walk_waits_under_the_deadline_until_the_engine_reads_ready() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 13);
        let slot = pool
            .engine_by_key(&LspProcessKey::named("rust"))
            .expect("slot");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let answer = slot
            .request_outgoing(
                open_lib,
                counted_references(&attempts),
                finish_immediately,
                started + Duration::from_secs(30),
            )
            .await
            .expect("the walk settles");
        let elapsed = started.elapsed();
        eprintln!("walk: attempts={attempts:?} elapsed={elapsed:?} answer={answer:?}");
        assert_eq!(answer, OutgoingAnswer::Ready(0));
        assert!((10..=11).contains(&attempts.load(std::sync::atomic::Ordering::SeqCst)));
        assert!(elapsed > Duration::from_secs(13) && elapsed < Duration::from_secs(20));
        pool.shutdown().await;
    }

    /// A wait spent before the engine reads ready answers `Unsettled` and
    /// keeps the session, where a request dropped at its deadline discards it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_spent_walk_wait_answers_unsettled_and_keeps_the_session() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = analyzing_slot(directory.path(), 50);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let answer = slot
            .request_outgoing(
                open_lib,
                counted_references(&attempts),
                finish_immediately,
                started + Duration::from_secs(1),
            )
            .await
            .expect("a spent wait is an answer, not a failure");
        eprintln!(
            "spent walk: attempts={attempts:?} elapsed={:?} answer={answer:?}",
            started.elapsed()
        );
        assert!(
            matches!(answer, OutgoingAnswer::Unsettled { attempts } if (2..=3).contains(&attempts)),
            "the engine's start shares the first 250 ms: {answer:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            slot.state.lock().await.session.is_some(),
            "the session survives the spent wait"
        );
        assert_eq!(pool.state_for_key(&key), Some(LspState::Analyzing));
        pool.shutdown().await;
    }

    /// A `sh` engine serving references and call hierarchy that refuses every
    /// later request with JSON-RPC error `code`, as rust-analyzer answers
    /// `-32801` content modified while it loads.
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
    ) -> SessionFuture<'session, Result<Option<usize>, EngineError>> {
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
    /// and an incoming one [`EngineFault::Analyzing`], both with the session
    /// kept. A refusal that is the engine's verdict on the request stays the
    /// walk's error, inside the walk's deadline.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_retryable_refusal_at_a_spent_walk_wait_answers_unsettled() {
        const CONTENT_MODIFIED: i64 = -32_801;
        const INVALID_REQUEST: i64 = -32_600;
        let key = LspProcessKey::named("rust");

        let directory = tempfile::tempdir().expect("workspace");
        let pool = refusing_slot(directory.path(), CONTENT_MODIFIED);
        let slot = pool.engine_by_key(&key).expect("slot");
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
                |_count| None,
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .expect_err("the incoming walk reports the spent wait");
        assert!(
            matches!(incoming.fault(), EngineFault::Analyzing { .. }),
            "{:?}",
            incoming.fault()
        );
        assert!(slot.state.lock().await.session.is_some());
        pool.shutdown().await;

        let directory = tempfile::tempdir().expect("workspace");
        let pool = refusing_slot(directory.path(), INVALID_REQUEST);
        let slot = pool.engine_by_key(&key).expect("slot");
        let started = Instant::now();
        let refused = slot
            .request_outgoing(
                open_lib,
                counted_prepare(&attempts),
                finish_immediately,
                started + Duration::from_secs(1),
            )
            .await
            .expect_err("the engine's verdict on the request stays the walk's error");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(
                refused.fault(),
                EngineFault::Refused {
                    code: INVALID_REQUEST,
                    ..
                }
            ),
            "{:?}",
            refused.fault()
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
                |_count| None,
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
                Some(rift_core::FileDigest::of(b"edited")),
            )],
            |_path| Some(rift_core::FileDigest::of(b"held")),
        )
    }

    /// After a feed resets readiness, an engine that announces no progress
    /// reads unconfirmed, and the incoming read takes its repeated full report
    /// once the session is quiet past the 500 ms `settle_delay`, with the
    /// unconfirmed record, instead of spending the 9.75 s retry table.
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
            elapsed >= Duration::from_millis(500) && elapsed < Duration::from_millis(1_500),
            "{elapsed:?}"
        );
        assert!(attempts <= 4, "{attempts}");
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
    /// wait answers [`EngineFault::Analyzing`] with the session kept, as a wait spent on
    /// an engine still analyzing does.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_incoming_report_that_never_settles_ends_at_the_spent_wait_with_the_session_kept() {
        let directory = tempfile::tempdir().expect("workspace");
        let pool = fed_slot(directory.path(), false);
        let key = LspProcessKey::named("rust");
        let slot = pool.engine_by_key(&key).expect("slot");
        let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let started = Instant::now();
        let spent = slot
            .request_settled(
                open_lib,
                counted_references(&attempts),
                finish_immediately,
                |_count| (true, true),
                |_count| None,
                started + Duration::from_secs(1),
            )
            .await
            .expect_err("an empty report from a ready engine never settles inside a second");
        assert!(
            matches!(spent.fault(), EngineFault::Analyzing { attempts } if *attempts >= 2),
            "{:?}",
            spent.fault()
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
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
        let held = rift_core::FileDigest::of(b"held");
        let beyond_the_bound = PathChanges::resolve(
            (0..=OWED_CHANGES_MAX).map(|index| {
                let path = ProjectPath::new(format!("f{index}.rs")).expect("path");
                (path, Some(held))
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
            |_count| None,
            Instant::now() + Duration::from_secs(2),
        );
        let owe = async {
            asked.notified().await;
            pool.owe_changed_paths(&beyond_the_bound);
        };
        let (spent, ()) = tokio::join!(request, owe);
        let spent = spent.expect_err("a partial report never settles");
        assert!(
            matches!(spent.fault(), EngineFault::Analyzing { .. }),
            "{:?}",
            spent.fault()
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
    async fn walk_from_root(
        slot: &EngineSlot,
        directory: &Path,
    ) -> (OutgoingAnswer<usize>, u64, Duration) {
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
        (
            answer,
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            started.elapsed(),
        )
    }

    /// A pool serving the embedded engine for Python over `directory`, and
    /// the key of its one slot.
    fn embedded_pool(directory: &Path) -> (EnginePool, LspProcessKey, Duration) {
        let mut configuration = table("ty");
        configuration.command = None;
        configuration.embedded = Some(EmbeddedEngine::Ty);
        configuration.initialization_options = None;
        let settle_delay = Duration::from_millis(configuration.settle_delay.milliseconds());
        let key = LspProcessKey::named("python");
        let pool = EnginePool::new(
            directory,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([("python".to_owned(), key.clone())]),
        );
        (pool, key, settle_delay)
    }

    /// The embedded engine declares itself ready at its start, so its
    /// first walk answers `Ready` on the first attempt instead of reading
    /// unconfirmed until the 500 ms `settle_delay` passes.
    #[tokio::test]
    async fn the_embedded_engine_walks_ready_on_its_first_attempt() {
        let directory = tempfile::tempdir().expect("workspace");
        let source = "def leaf() -> int:\n    return 1\n\ndef root() -> int:\n    return leaf()\n";
        std::fs::write(directory.path().join("graph.py"), source).expect("source");
        let (pool, key, settle_delay) = embedded_pool(directory.path());
        let slot = pool.engine_by_key(&key).expect("slot");
        let (answer, attempts, elapsed) = walk_from_root(slot, directory.path()).await;
        eprintln!("embedded first walk: elapsed={elapsed:?} answer={answer:?}");
        assert_eq!(answer, OutgoingAnswer::Ready(1), "root calls leaf");
        assert_eq!(attempts, 1);
        assert!(elapsed < settle_delay, "{elapsed:?}");
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
        let (pool, key, settle_delay) = embedded_pool(directory.path());
        let slot = pool.engine_by_key(&key).expect("slot");
        let (before, _, _) = walk_from_root(slot, directory.path()).await;
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

        let (after, attempts, elapsed) = walk_from_root(slot, directory.path()).await;
        eprintln!("embedded walk after an edit: elapsed={elapsed:?} answer={after:?}");
        assert_eq!(after, OutgoingAnswer::Ready(2), "root calls leaf and twig");
        assert_eq!(attempts, 1);
        assert!(elapsed < settle_delay, "{elapsed:?}");
        assert_eq!(pool.state_for_key(&key), Some(LspState::Ready));
        pool.shutdown().await;
    }

    /// The index's three classifications reach the engine as created, changed and
    /// deleted; a later change to one path replaces the earlier one; past
    /// `OWED_CHANGES_MAX` paths the slot stops recording and marks the bound spent.
    #[test]
    fn owed_changes_classify_merge_and_mark_the_bound_spent() {
        let path = |name: &str| ProjectPath::new(name).expect("path");
        let held = rift_core::FileDigest::of(b"held");
        let first = PathChanges::resolve(
            [
                (path("added.rs"), Some(rift_core::FileDigest::of(b"new"))),
                (
                    path("changed.rs"),
                    Some(rift_core::FileDigest::of(b"edited")),
                ),
                (path("removed.rs"), None),
            ],
            |observed| (observed.as_str() != "added.rs").then_some(held),
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
            Some(held)
        }));
        assert_eq!(
            owed.paths.get(&path("added.rs")),
            Some(&FileChangeType::DELETED),
            "the later change replaces the earlier one"
        );

        let many = PathChanges::resolve(
            (0..=OWED_CHANGES_MAX).map(|index| (path(&format!("f{index}.rs")), Some(held))),
            |_| None,
        );
        let mut owed = OwedChanges::default();
        owed.record(&many);
        assert!(
            owed.overflowed && owed.paths.is_empty(),
            "{:?}",
            owed.paths.len()
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
