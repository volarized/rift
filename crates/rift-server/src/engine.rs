//! The pool of language engine sessions the server holds across requests.
//!
//! One [`EngineSlot`] exists per accepted `[engines.<name>]` table. A slot
//! spawns its engine on the first request for a language it serves, reuses
//! the running session across requests, and replaces a dead engine at most
//! [`RESPAWN_PER_REQUEST_MAX`] times per request. The pool never invents an
//! engine: a language no table claims answers no slot, and the caller turns
//! that absence into its own refusal.
//!
//! Locking: each slot owns one Tokio mutex over its session. A request
//! holds that slot's lock while it speaks to the engine, so requests for
//! one engine serialize while requests for different engines proceed
//! independently. No std lock is held across an await anywhere in the
//! pool.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_lsp::session::{EngineError, EngineLaunch, EngineSession};
use rift_protocol::configuration::EngineConfiguration;
use rift_protocol::read::Language;
use tokio::sync::Mutex;

/// Replacements of one dead engine a single request may trigger, at most.
pub const RESPAWN_PER_REQUEST_MAX: usize = 1;

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
                    session: Mutex::new(None),
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
            let mut held = slot.session.lock().await;
            if let Some(session) = held.take() {
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
    session: Mutex<Option<EngineSession>>,
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

    /// Runs one operation against this engine, spawning it when none runs.
    ///
    /// The operation returns a boxed future borrowing the session, the one
    /// shape that lets callers state the future's `Send` bound.
    ///
    /// The slot's lock is held for the whole call, so operations against
    /// one engine serialize. A session found dead - ended by an earlier
    /// request, or ended by this operation's own failure - is replaced at
    /// most [`RESPAWN_PER_REQUEST_MAX`] times within one call, and the
    /// operation runs again on the replacement; the loop therefore runs at
    /// most [`RESPAWN_PER_REQUEST_MAX`] + 1 times. A failure that leaves
    /// the engine serving - a refusal, an absent capability - returns
    /// without any respawn.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when a spawn fails, or when the operation
    /// fails past the respawn budget.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future releases the slot lock. A session already
    /// spawned stays in the slot for the next request; an operation
    /// cancelled mid-exchange leaves its stale engine response for the
    /// session to discard later, as the session documents.
    pub async fn request<T>(
        &self,
        mut operation: impl for<'session> FnMut(
            &'session mut EngineSession,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<T, EngineError>> + Send + 'session>,
        >,
    ) -> Result<T, EngineError> {
        let mut held = self.session.lock().await;
        let mut respawn_count: usize = 0;
        loop {
            let session = match held.take() {
                Some(running) if !running.is_ended() => held.insert(running),
                dead => {
                    if let Some(dead) = dead {
                        respawn_count += 1;
                        self.reap(dead).await;
                    }
                    let started = EngineSession::start(self.launch(), &self.workspace_root).await?;
                    held.insert(started)
                }
            };
            match operation(session).await {
                Ok(answer) => return Ok(answer),
                Err(error) => {
                    if !session.is_ended() || respawn_count >= RESPAWN_PER_REQUEST_MAX {
                        return Err(error);
                    }
                }
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
