//! The pool of language engine sessions the server holds across requests.
//!
//! One [`EngineSlot`] exists per accepted `[engines.<name>]` table. A slot
//! spawns its engine on the first request for a language it serves, reuses
//! the running session across requests, and replaces an engine that ended,
//! failed to start, or stopped answering within the budget its
//! `[engines.<name>.restart]` table states. The pool never invents an
//! engine: a language no table claims answers no slot, and the caller turns
//! that absence into its own refusal.
//!
//! The slot is also where every transient condition between Rift and one
//! engine is absorbed. [`EngineSlot::request`] runs the caller's operation
//! again while the engine answers provisionally or refuses retryably, under
//! that engine's `[engines.<name>.retry]` table, and starts a replacement
//! engine under its `[engines.<name>.restart]` table when the one it has
//! dies. It also sends the operation again once when an engine that has
//! never announced any work answers nothing, because until the first
//! announcement arrives an empty answer is indistinguishable from a real
//! one. Callers hold no retry loop of their own: an operation returns
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
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_core::{Error, ErrorCode, ErrorName};
use rift_lsp::session::{EngineError, EngineFault, EngineLaunch, EngineSession};
use rift_protocol::configuration::EngineConfiguration;
use rift_protocol::read::Language;
use rift_protocol::retry::RestartPolicy;
use tokio::sync::Mutex;
use tokio::time::Instant;

/// The language engines one workspace serves, spawned lazily and reused.
///
/// Dropping the pool without [`EnginePool::shutdown`] still kills every
/// running child through the session's kill-on-drop arming; `shutdown` is
/// the graceful path that also asks each engine to exit first.
#[derive(Debug)]
pub struct EnginePool {
    engines: BTreeMap<String, EngineSlot>,
    served: BTreeMap<String, String>,
}

impl EnginePool {
    /// Builds the pool for the accepted `[engines]` tables, spawning
    /// nothing.
    ///
    /// Acceptance proves each language identity segment is claimed by one
    /// engine; on unvalidated input the engine earliest in name order
    /// keeps the segment.
    #[must_use]
    pub fn new(workspace_root: &Path, engines: BTreeMap<String, EngineConfiguration>) -> Self {
        let mut slots = BTreeMap::new();
        let mut served = BTreeMap::new();
        for (name, configuration) in engines {
            for language in &configuration.languages {
                served
                    .entry(language.clone())
                    .or_insert_with(|| name.clone());
            }
            slots.insert(
                name.clone(),
                EngineSlot {
                    name,
                    configuration,
                    workspace_root: workspace_root.to_path_buf(),
                    state: Mutex::new(SlotState::default()),
                },
            );
        }
        Self {
            engines: slots,
            served,
        }
    }

    /// The slot serving `language`, absent when no engine claims its
    /// identity segment.
    #[must_use]
    pub fn engine_for(&self, language: &Language) -> Option<&EngineSlot> {
        let name = self.served.get(&language.identity_segment())?;
        self.engines.get(name)
    }

    /// Whether this pool was built from exactly these engine tables.
    #[must_use]
    pub fn built_from(&self, engines: &BTreeMap<String, EngineConfiguration>) -> bool {
        self.engines.len() == engines.len()
            && engines.iter().all(|(name, table)| {
                self.engines
                    .get(name)
                    .is_some_and(|slot| &slot.configuration == table)
            })
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
                tracing::debug!(
                    component = "engine",
                    engine = %slot.name,
                    stderr_bytes = stderr.total_bytes,
                    "language engine shut down"
                );
            }
        }
    }
}

/// One accepted engine table and the session state behind it.
#[derive(Debug)]
pub struct EngineSlot {
    name: String,
    configuration: EngineConfiguration,
    workspace_root: PathBuf,
    state: Mutex<SlotState>,
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
    /// The engine refused with a code that invites the same request again.
    Refused(EngineError),
    /// The engine has never announced any work and answered nothing where
    /// something was expected, so its silence proves nothing.
    ///
    /// The answer rides along: the engine did answer, so a `retry` table
    /// too narrow to carry the one resend leaves this as the only answer
    /// there is.
    AnsweredNothing(T),
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
    /// The engine's name: the `[engines.<name>]` key.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The accepted table this slot serves under.
    #[must_use]
    pub fn configuration(&self) -> &EngineConfiguration {
        &self.configuration
    }

    /// Runs one operation against this engine, absorbing every transient
    /// condition between Rift and it.
    ///
    /// The operation runs again, unchanged, for every attempt: one that
    /// opens a document opens it again on the next attempt.
    ///
    /// The slot's lock is held for the whole call, so operations against
    /// one engine serialize. Three outcomes send the operation back:
    ///
    /// - The engine answered while it was still analyzing. That answer is
    ///   provisional, so it is discarded and the operation is sent again
    ///   after the `[engines.<name>.retry]` wait. A refusal it answered
    ///   mid-analysis is provisional for the same reason: rust-analyzer
    ///   refuses a rename with `No references found at position` for a
    ///   declaration it has not indexed yet, which is not its verdict on
    ///   the request.
    /// - The engine refused with a code that invites the same request
    ///   again ([`EngineFault::is_retryable_refusal`]). Same treatment.
    /// - The engine has never announced any work and answered nothing
    ///   where something was expected. Announcing is what makes the wait
    ///   above possible, so until the engine has announced once, an empty
    ///   answer is indistinguishable from a real one: the session claims
    ///   one resend for it
    ///   ([`EngineSession::claim_empty_answer_resend`]) and the operation
    ///   goes back after the same wait. The claim is spent per operation
    ///   per session, so an engine that announces nothing ever pays this
    ///   once and answers at once from then on.
    /// - The session found or left the engine dead. A replacement starts
    ///   while the `[engines.<name>.restart]` budget allows, and the
    ///   operation runs on it.
    ///
    /// Every other failure - a verdict the settled engine reached, an
    /// absent capability, a broken exchange - returns at once, because
    /// sending it again changes nothing. So does a settled answer that
    /// said something, and so does the second empty answer: what the
    /// engine says twice is what it has to say.
    ///
    /// Both tables bound the loop. Each pass either returns, spends one of
    /// `retry.attempts`, or claims one of `restart.attempts` inside its
    /// window, so the loop runs at most their sum plus one.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`]: the operation's own failure when the
    /// engine keeps serving, the last retryable refusal once the retry
    /// budget is spent, [`EngineFault::Analyzing`] when every attempt was
    /// answered mid-analysis, the start failure when no engine could be
    /// started, and [`EngineFault::Ended`] when the restart budget was
    /// already spent before this call asked anything.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future releases the slot lock. A session already
    /// spawned stays in the slot for the next request; an operation
    /// cancelled mid-exchange leaves its stale engine response for the
    /// session to discard later, as the session documents.
    pub async fn request<T>(
        &self,
        mut operation: impl AsyncFnMut(&mut EngineSession) -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let retry = self.configuration.retry;
        let mut held = self.state.lock().await;
        let mut attempt: u64 = 1;
        let mut reported: Option<EngineError> = None;
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
            let outcome = operation(session).await;
            let analyzing = session.is_analyzing();
            let ended = session.is_ended();
            let absorbed = match outcome {
                Ok(answer) if !analyzing => {
                    if !session.claim_empty_answer_resend() {
                        return Ok(answer);
                    }
                    Transient::AnsweredNothing(answer)
                }
                Ok(_provisional) => Transient::Analyzing,
                Err(error) if ended => {
                    reported = Some(error);
                    continue;
                }
                Err(error) if analyzing && error.fault().is_refusal() => Transient::Analyzing,
                Err(error) if error.fault().is_retryable_refusal() => Transient::Refused(error),
                Err(error) => return Err(error),
            };
            let Some(wait) = retry.delay_after(attempt) else {
                return self.exhausted(absorbed, retry.attempts);
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// What one absorbed condition surfaces once the attempt bound is
    /// spent, logged as the boundary a caller is about to see.
    ///
    /// Two of the three end the call with a failure. The third does not:
    /// an engine that answered nothing did answer, and a `retry` table
    /// with no second attempt in it leaves that answer as the only one
    /// there is.
    fn exhausted<T>(&self, absorbed: Transient<T>, attempts: u64) -> Result<T, EngineError> {
        match absorbed {
            Transient::Analyzing => {
                tracing::warn!(
                    component = "engine",
                    engine = %self.name,
                    attempts,
                    "language engine was still analyzing on every attempt"
                );
                Err(Error::new(EngineFault::Analyzing { attempts }))
            }
            Transient::Refused(refusal) => {
                tracing::warn!(
                    component = "engine",
                    engine = %self.name,
                    attempts,
                    "language engine refused every attempt retryably"
                );
                Err(refusal)
            }
            Transient::AnsweredNothing(answer) => {
                tracing::debug!(
                    component = "engine",
                    engine = %self.name,
                    attempts,
                    "language engine answered nothing and the attempts allow no resend"
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
                tracing::warn!(
                    component = "engine",
                    engine = %self.name,
                    attempts = self.configuration.restart.attempts,
                    "language engine restart budget is spent for this window"
                );
                return Err(reported.unwrap_or_else(|| Error::new(EngineFault::Ended)));
            }
            match EngineSession::start(self.launch(), &self.workspace_root).await {
                Ok(session) => return Ok(session),
                Err(failure) if !restart_may_help(&failure) => return Err(failure),
                Err(failure) => reported = Some(failure),
            }
        }
    }

    /// The launch derived from this engine's accepted table.
    fn launch(&self) -> EngineLaunch {
        EngineLaunch {
            program: self.configuration.program.clone(),
            arguments: self.configuration.arguments.clone(),
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

    /// Reaps one ended engine and keeps its captured standard error
    /// visible in the log.
    async fn reap(&self, dead: EngineSession) {
        let stderr = dead.shutdown().await;
        tracing::warn!(
            component = "engine",
            engine = %self.name,
            stderr = %stderr.text,
            "language engine ended and was reaped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::configuration::{ByteSize, Duration as ConfiguredDuration};
    use rift_protocol::retry::RetryPolicy;

    fn table(program: &str, languages: &[&str]) -> EngineConfiguration {
        EngineConfiguration {
            program: program.to_owned(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            languages: languages
                .iter()
                .map(|&language| language.to_owned())
                .collect(),
            initialization_options: Some(serde_json::json!({ "engine": "fake" })),
            startup_timeout: ConfiguredDuration::from_millis(10_000),
            request_timeout: ConfiguredDuration::from_millis(20_000),
            output_limit: ByteSize::from_bytes(2_048),
            retry: RetryPolicy::default(),
            restart: RestartPolicy::default(),
        }
    }

    fn language(name: &str, dialect: Option<&str>) -> Language {
        Language {
            name: name.to_owned(),
            dialect: dialect.map(str::to_owned),
        }
    }

    fn pool(entries: Vec<(&str, EngineConfiguration)>) -> EnginePool {
        let engines = entries
            .into_iter()
            .map(|(name, engine)| (name.to_owned(), engine))
            .collect();
        EnginePool::new(Path::new("/rift-test-root"), engines)
    }

    #[test]
    fn engine_for_maps_identity_segments_and_answers_nothing_else() {
        let built = pool(vec![
            ("ty", table("uvx", &["python"])),
            (
                "typescript",
                table("bunx", &["typescript", "typescript:tsx"]),
            ),
        ]);
        let python = built
            .engine_for(&language("python", None))
            .expect("python is served");
        assert_eq!(python.name(), "ty");
        assert_eq!(python.configuration().program, "uvx");
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
    fn first_engine_in_name_order_keeps_a_contested_segment() {
        let built = pool(vec![
            ("b", table("second", &["python"])),
            ("a", table("first", &["python"])),
        ]);
        let claimed = built
            .engine_for(&language("python", None))
            .expect("python is served");
        assert_eq!(claimed.name(), "a");
    }

    #[test]
    fn built_from_compares_names_and_tables() {
        let entries = vec![("ty", table("uvx", &["python"]))];
        let built = pool(entries.clone());
        let same: BTreeMap<String, EngineConfiguration> = entries
            .into_iter()
            .map(|(name, engine)| (name.to_owned(), engine))
            .collect();
        assert!(built.built_from(&same));
        let mut renamed = same.clone();
        let moved = renamed.remove("ty").expect("the entry exists");
        renamed.insert("pyright".to_owned(), moved);
        assert!(!built.built_from(&renamed));
        let mut retimed = same.clone();
        if let Some(engine) = retimed.get_mut("ty") {
            engine.request_timeout = ConfiguredDuration::from_millis(30_000);
        }
        assert!(!built.built_from(&retimed));
        assert!(!built.built_from(&BTreeMap::new()));
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
    fn launch_carries_the_accepted_table_verbatim() {
        let built = pool(vec![("ty", table("uvx", &["python"]))]);
        let slot = built
            .engine_for(&language("python", None))
            .expect("python is served");
        let launch = slot.launch();
        assert_eq!(launch.program, "uvx");
        assert_eq!(launch.startup_timeout, Duration::from_secs(10));
        assert_eq!(launch.request_timeout, Duration::from_secs(20));
        assert_eq!(launch.stderr_capture_bytes, 2_048);
        assert_eq!(
            launch.initialization_options,
            Some(serde_json::json!({ "engine": "fake" }))
        );
    }
}
