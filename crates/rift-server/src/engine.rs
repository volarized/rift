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

use rift_core::{Error, ErrorCode, ErrorName};
use rift_lsp::session::{EngineError, EngineFault, EngineLaunch, EngineSession};
use rift_protocol::configuration::{CommandInput, EmbeddedEngine, LspConfiguration};
use rift_protocol::read::Language;
use rift_protocol::retry::RestartPolicy;
use rift_protocol::workspace::LspState;
use tokio::sync::{Mutex, watch};
use tokio::time::Instant;

use rift_lsp::session::EngineReadiness;

type SessionFuture<'session, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'session>>;

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

    /// Current state of one accepted process definition.
    #[must_use]
    pub fn state_for_key(&self, key: &LspProcessKey) -> Option<LspState> {
        let slot = self.engines.get(key)?;
        Some(*slot.reported_state.borrow())
    }

    /// Ends every running engine with the session's own bounded shutdown.
    ///
    /// The walk locks one slot at a time, so a request in flight on an
    /// engine finishes - or times out - before that engine is ended.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future mid-walk leaves the remaining sessions in
    /// their slots; dropping the pool then kills their children through
    /// the session's kill-on-drop arming.
    pub async fn shutdown(&self) {
        for slot in self.engines.values() {
            let mut held = slot.state.lock().await;
            if let Some(session) = held.session.take() {
                let stderr = session.shutdown().await;
                slot.report_state(LspState::Stopped);
                let engine = slot.name();
                tracing::debug!(
                    component = "engine",
                    engine,
                    stderr_bytes = stderr.total_bytes,
                    "language engine shut down"
                );
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
            let mut held = slot.state.lock().await;
            if let Some(session) = held.session.take() {
                let _stderr = session.shutdown().await;
                slot.report_state(LspState::Stopped);
            }
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
    reported_state: watch::Sender<LspState>,
}

/// Everything one slot's lock guards: the running engine and the restarts
/// already spent on it.
#[derive(Debug, Default)]
struct SlotState {
    session: Option<EngineSession>,
    restarts: RestartBudget,
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
}

/// Decides whether one diagnostic report is ready to return.
fn diagnostic_settlement(readiness: EngineReadiness, evidence: DiagnosticEvidence) -> Settlement {
    if evidence.shape == ReportShape::Partial || readiness == EngineReadiness::Analyzing {
        return Settlement::Retry;
    }
    if (readiness == EngineReadiness::Ready && evidence.shape == ReportShape::FullNonempty)
        || (evidence.repeated && evidence.final_attempt)
    {
        Settlement::Ready
    } else {
        Settlement::Retry
    }
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
fn restart_may_help(error: &EngineError) -> bool {
    error.name() != ErrorName::Wire(ErrorCode::ConfigurationInvalid)
}

impl EngineSlot {
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
    ) -> Result<T, EngineError> {
        let mut previous = None;
        self.request_deciding(
            begin,
            operation,
            finish,
            |session, session_generation, answer, final_attempt| {
                let stale = answer_version(&answer)
                    .is_some_and(|version| version != session.document_version());
                if stale {
                    return Answer::Retry(Transient::Unready);
                }
                let repeated = previous.as_ref().is_some_and(|(prior_generation, prior)| {
                    *prior_generation == session_generation && prior == &answer
                });
                let (full, empty) = report_state(&answer);
                if diagnostic_settlement(
                    session.readiness(),
                    DiagnosticEvidence {
                        shape: ReportShape::from_report(full, empty),
                        repeated,
                        final_attempt,
                    },
                ) == Settlement::Ready
                {
                    Answer::Ready(answer)
                } else {
                    previous = Some((session_generation, answer));
                    Answer::Retry(Transient::Unready)
                }
            },
        )
        .await
    }

    /// Shared bounded request loop.
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
        mut decide: impl FnMut(&mut EngineSession, u64, T, bool) -> Answer<T>,
    ) -> Result<T, EngineError> {
        let retry = self.configuration.retry;
        let mut held = self.state.lock().await;
        let mut attempt: u64 = 1;
        let mut reported: Option<EngineError> = None;
        let mut exchange_started = false;
        let mut session_generation = 0_u64;
        loop {
            let state = &mut *held;
            let session = match state.session.take() {
                Some(running) if !running.is_ended() => state.session.insert(running),
                dead => {
                    if let Some(dead) = dead {
                        self.reap(dead).await;
                    }
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
                    return Err(error);
                }
            };
            let Some(wait) = retry.delay_after(attempt) else {
                finish(session).await;
                return self.exhausted(absorbed, retry.attempts);
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// What one absorbed condition surfaces once attempt bound is spent.
    fn exhausted<T>(&self, absorbed: Transient<T>, attempts: u64) -> Result<T, EngineError> {
        let engine = self.name();
        match absorbed {
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
    async fn start_within_budget(
        &self,
        budget: &mut RestartBudget,
        mut reported: Option<EngineError>,
    ) -> Result<EngineSession, EngineError> {
        loop {
            if !budget.claim(&self.configuration.restart, Instant::now()) {
                self.report_state(LspState::Failed);
                let engine = self.name();
                tracing::warn!(
                    component = "engine",
                    engine,
                    attempts = self.configuration.restart.attempts,
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
                    return Err(failure);
                }
                Err(failure) => reported = Some(failure),
            }
        }
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
            stderr_capture_bytes: usize::try_from(self.configuration.output_limit.bytes())
                .unwrap_or(usize::MAX),
        }
    }

    /// Reaps one ended engine and keeps its captured standard error
    /// visible in the log.
    async fn reap(&self, dead: EngineSession) {
        let stderr = dead.shutdown().await;
        let engine = self.name();
        tracing::warn!(
            component = "engine",
            engine,
            stderr = %stderr.text,
            "language engine ended and was reaped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::configuration::{ByteSize, CommandInput, Duration as ConfiguredDuration};
    use rift_protocol::retry::RetryPolicy;

    fn table(program: &str) -> LspConfiguration {
        LspConfiguration {
            command: Some(CommandInput::Program(program.to_owned())),
            embedded: None,
            environment: BTreeMap::new(),
            initialization_options: Some(serde_json::json!({ "engine": "fake" })),
            startup_timeout: ConfiguredDuration::from_millis(10_000),
            request_timeout: ConfiguredDuration::from_millis(20_000),
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
    fn diagnostic_settlement_covers_progress_and_stable_full_reports() {
        let rows = [
            (
                "stale nonempty",
                EngineReadiness::Unconfirmed,
                true,
                false,
                false,
                false,
                Settlement::Retry,
            ),
            (
                "stale empty",
                EngineReadiness::Unconfirmed,
                true,
                true,
                false,
                true,
                Settlement::Retry,
            ),
            (
                "progress start",
                EngineReadiness::Analyzing,
                true,
                false,
                true,
                true,
                Settlement::Retry,
            ),
            (
                "progress end",
                EngineReadiness::Ready,
                true,
                false,
                false,
                false,
                Settlement::Ready,
            ),
            (
                "progress end empty",
                EngineReadiness::Ready,
                true,
                true,
                false,
                false,
                Settlement::Retry,
            ),
            (
                "oscillating",
                EngineReadiness::Unconfirmed,
                true,
                false,
                false,
                true,
                Settlement::Retry,
            ),
            (
                "stable before bound",
                EngineReadiness::Unconfirmed,
                true,
                false,
                true,
                false,
                Settlement::Retry,
            ),
            (
                "stable at bound",
                EngineReadiness::Unconfirmed,
                true,
                false,
                true,
                true,
                Settlement::Ready,
            ),
            (
                "partial",
                EngineReadiness::Unconfirmed,
                false,
                false,
                true,
                true,
                Settlement::Retry,
            ),
        ];
        for (name, readiness, full, empty, repeated, final_attempt, expected) in rows {
            assert_eq!(
                diagnostic_settlement(
                    readiness,
                    DiagnosticEvidence {
                        shape: ReportShape::from_report(full, empty),
                        repeated,
                        final_attempt,
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
