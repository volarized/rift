//! Background filesystem validation and bounded Git index-lock waits.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rift_error::errors;
use rift_index::{
    LastCapture, PathChange, PathChanges, capture_visible_digests_with_languages_cancellable,
};
use rift_protocol::configuration::ServerConfiguration;
use rift_server::RiftError;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{INDEX_DEBOUNCE, IndexSupervisorContext, configuration_fingerprint};

/// The reason the supervisor next checks its pending work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Trigger {
    Filesystem,
    Validation,
}

/// Freezes startup settings and resolves the worktree's control path on the worker pool.
pub(super) async fn start(
    context: &IndexSupervisorContext,
) -> Result<Option<(BackgroundValidation, VersionControlHold)>, RiftError> {
    let configuration = context
        .published
        .read()
        .await
        .snapshot()
        .0
        .configuration
        .server_configuration();
    let root = context.root.clone();
    let lock = context
        .blocking
        .run_with_cancellation(
            "Git index lock discovery",
            context.validation.cancellation.clone(),
            move |_| {
                Ok(rift_history::Repository::open(&root)
                    .ok()
                    .map(|repository| repository.index_lock_path()))
            },
        )
        .await?;
    let mut hold = VersionControlHold::new(lock, &configuration);
    if !hold.wait(context).await? {
        return Ok(None);
    }
    Ok(Some((BackgroundValidation::new(&configuration), hold)))
}

/// A deadline that filesystem events and requests never move.
///
/// The validation that deadline makes due runs on the supervisor's next turn, with one
/// exception: on the turn after a superseded rebuild the owed rebuild runs first, until
/// the validation is one interval late. See [`Self::rebuild_runs_first`].
pub(super) struct BackgroundValidation {
    interval: Duration,
    deadline: Instant,
    last: Arc<Mutex<LastCapture>>,
}

impl BackgroundValidation {
    pub(super) fn new(configuration: &ServerConfiguration) -> Self {
        let interval = Duration::from_millis(configuration.validation_interval.milliseconds());
        Self {
            interval,
            deadline: Instant::now() + interval,
            last: Arc::new(Mutex::new(LastCapture::default())),
        }
    }

    /// Cancellation wins, then an elapsed validation deadline, then an event.
    pub(super) async fn next(
        &self,
        invalidations: &mut mpsc::Receiver<()>,
        cancellation: &CancellationToken,
    ) -> Option<Trigger> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => None,
            () = tokio::time::sleep_until(self.deadline) => Some(Trigger::Validation),
            received = invalidations.recv() => received.map(|()| Trigger::Filesystem),
        }
    }

    /// Whether the rebuild a superseded turn left owed runs before this due validation.
    ///
    /// The reads waiting on that rebuild already waited for a capture that published
    /// nothing, and a validation ahead of it adds one whole-tree capture to their wait.
    /// Each of those reads captured the tree itself, so the validation tells them nothing.
    /// The rebuild runs first while the validation is less than one interval late. From
    /// then on the validation runs first, so rebuilds superseded back to back delay a
    /// check by one interval at most, plus the rebuild running when that interval ends.
    pub(super) fn rebuild_runs_first(&self, now: Instant) -> bool {
        now < self.deadline + self.interval
    }

    /// Whether this turn's validation waits for the owed rebuild, recording that it does.
    ///
    /// It waits when `trigger` made it due, the last rebuild turn ended superseded, and
    /// [`Self::rebuild_runs_first`] still holds. The deadline stays elapsed, so the
    /// validation takes the turn after that rebuild.
    pub(super) fn defers(
        &self,
        trigger: Trigger,
        after_superseded: bool,
        observed_epoch: u64,
    ) -> bool {
        let deferred = after_superseded
            && trigger == Trigger::Validation
            && self.rebuild_runs_first(Instant::now());
        if deferred {
            rift_tracing::debug!(
                component = "index",
                operation = "index.validate",
                observed_epoch,
                "background filesystem validation deferred"
            );
        }
        deferred
    }

    /// Captures using the same inclusion and racy-stat rules as current-tree reads.
    /// An unchanged capture queues no observation and derives no syntax or documentation.
    pub(super) async fn validate(
        &mut self,
        context: &IndexSupervisorContext,
    ) -> Result<(), RiftError> {
        let (current, failure) = context.published.read().await.snapshot();
        let recovering = failure.is_some();
        let root = context.root.clone();
        let limits = context.limits;
        let last = Arc::clone(&self.last);
        let observation = context
            .blocking
            .run_with_cancellation(
                "background workspace validation",
                context.validation.cancellation.clone(),
                move |cancellation| {
                    let mut last = last
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let (digests, visible, next) =
                        capture_visible_digests_with_languages_cancellable(
                            &root,
                            current.configuration.index_limits(limits)?,
                            &current.configuration.source_visibility(),
                            &current.configuration.text_inclusion(),
                            &current.configuration.language_file_selections(),
                            &last,
                            &|| cancellation.is_cancelled(),
                        )?;
                    rift_tracing::debug!(
                        component = "index",
                        operation = "index.validate",
                        read_paths = next.read_paths(),
                        "background filesystem validation captured"
                    );
                    *last = next;
                    let held = current.reads.workspace_digests();
                    let mut known_path_removed = false;
                    let compared = if let Some(preparation) = &current.preparation {
                        let known: std::collections::BTreeSet<_> = preparation
                            .map_source_paths
                            .iter()
                            .map(|(path, _)| path)
                            .chain(preparation.map_text_paths.iter())
                            .collect();
                        known_path_removed = known.iter().any(|path| digests.get(path).is_none());
                        // Discovered files awaiting preparation have no published bytes to
                        // compare yet. Their presence alone does not restart preparation.
                        rift_index::WorkspaceDigests::new(
                            digests
                                .iter()
                                .filter(|(path, _)| {
                                    held.get(path).is_some() || !known.contains(path)
                                })
                                .map(|(path, digest)| (path.clone(), digest)),
                        )
                    } else {
                        digests
                    };
                    let changes = PathChanges::between(&held, &compared);
                    let visible_changes = if current.preparation.is_none() {
                        PathChanges::between(&current.visible_digests, &visible)
                    } else {
                        PathChanges::default()
                    };
                    let paths: std::collections::BTreeSet<_> = changes
                        .paths()
                        .chain(visible_changes.paths())
                        .cloned()
                        .collect();
                    let added_or_removed: std::collections::BTreeSet<rift_core::ProjectPath> =
                        changes
                            .iter()
                            .chain(visible_changes.iter())
                            .filter(|(_, change)| !matches!(change, PathChange::Modified))
                            .map(|(path, _)| path.clone())
                            .collect();
                    let full = (current.preparation.is_some() && !changes.is_empty())
                        || known_path_removed
                        || configuration_fingerprint(&root) != current.configuration.fingerprint
                        || (!added_or_removed.is_empty()
                            && !current.rebuild_moves_every(&root, &added_or_removed));
                    Ok((paths, full))
                },
            )
            .await;
        self.deadline = Instant::now() + self.interval;
        let (paths, full) = observation?;
        if full {
            context.validation.observe_whole_workspace()?;
        } else if !paths.is_empty() || recovering {
            context.validation.observe_paths(paths)?;
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HoldDecision {
    Proceed,
    WaitUntil(Instant),
    Expired,
}

/// One lock's first observed appearance determines its entire wait budget.
/// An expired lock stays expired until its absence is observed.
pub(super) struct VersionControlHold {
    lock: Option<PathBuf>,
    timeout: Duration,
    deadline: Option<Instant>,
    expiration_reported: bool,
}

impl VersionControlHold {
    pub(super) fn new(lock: Option<PathBuf>, configuration: &ServerConfiguration) -> Self {
        Self {
            lock,
            timeout: Duration::from_millis(configuration.version_control_timeout.milliseconds()),
            deadline: None,
            expiration_reported: false,
        }
    }

    fn decision(&mut self, present: bool, now: Instant) -> HoldDecision {
        if !present {
            self.deadline = None;
            self.expiration_reported = false;
            return HoldDecision::Proceed;
        }
        let deadline = *self.deadline.get_or_insert(now + self.timeout);
        if now >= deadline {
            HoldDecision::Expired
        } else {
            HoldDecision::WaitUntil(deadline)
        }
    }

    /// Metadata probes run on the bounded worker pool. Cancellation ends the wait;
    /// a retained lock is passed once its original timeout expires.
    pub(super) async fn wait(
        &mut self,
        context: &IndexSupervisorContext,
    ) -> Result<bool, RiftError> {
        let Some(lock) = self.lock.clone() else {
            return Ok(true);
        };
        loop {
            let wait_deadline = self.deadline.filter(|deadline| *deadline > Instant::now());
            let path = lock.clone();
            let present = context.blocking.run_with_cancellation(
                "Git index lock validation",
                context.validation.cancellation.clone(),
                move |_| match std::fs::symlink_metadata(&path) {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => errors::server::read_unavailable()
                        .operation("Git index lock validation")
                        .detail(error.to_string())
                        .fail(),
                },
            );
            let present = tokio::select! {
                biased;
                () = context.validation.cancellation.cancelled() => return Ok(false),
                () = async {
                    if let Some(deadline) = wait_deadline {
                        tokio::time::sleep_until(deadline).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    self.report_expiration(&lock);
                    return Ok(true);
                },
                result = present => result?,
            };
            match self.decision(present, Instant::now()) {
                HoldDecision::Proceed => return Ok(true),
                HoldDecision::Expired => {
                    self.report_expiration(&lock);
                    return Ok(true);
                }
                HoldDecision::WaitUntil(deadline) => {
                    tokio::select! {
                        biased;
                        () = context.validation.cancellation.cancelled() => return Ok(false),
                        () = tokio::time::sleep_until(deadline.min(Instant::now() + INDEX_DEBOUNCE)) => {},
                    }
                }
            }
        }
    }

    fn report_expiration(&mut self, lock: &std::path::Path) {
        if !self.expiration_reported {
            rift_tracing::warn!(component = "index", operation = "index.lock", path = %lock.display(), "Git index lock wait expired");
            self.expiration_reported = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stable_context(
        root: &std::path::Path,
    ) -> Result<(IndexSupervisorContext, mpsc::Receiver<()>), Box<dyn std::error::Error>> {
        let limits = rift_index::WorkspaceIndexLimits::default();
        let super::super::WorkspaceCandidate::Stable {
            published: current, ..
        } = super::super::build_workspace_candidate(
            root,
            limits,
            &super::super::RebuildRequest::initial(0),
        )?
        else {
            return Err("stable fixture required".into());
        };
        let (validation, invalidations) = super::super::IndexValidation::new(limits.files_max());
        Ok((
            IndexSupervisorContext {
                root: root.to_path_buf(),
                limits,
                published: Arc::new(tokio::sync::RwLock::new(super::super::IndexState {
                    current,
                    failure: None,
                })),
                validation,
                blocking: crate::server::BlockingExecutor::isolated(1, 60_000),
                population: None,
                lexical: None,
            },
            invalidations,
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn missing_events_reach_validation_deadline() {
        let configuration = ServerConfiguration::default();
        let background = BackgroundValidation::new(&configuration);
        let (_sender, mut receiver) = mpsc::channel(1);
        assert_eq!(
            background
                .next(&mut receiver, &CancellationToken::new())
                .await,
            Some(Trigger::Validation)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn continuous_events_cannot_postpone_validation_and_cancellation_wins() {
        let background = BackgroundValidation::new(&ServerConfiguration::default());
        let (sender, mut receiver) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        for _ in 0..30 {
            sender
                .try_send(())
                .expect("one event fits the drained channel");
            assert_eq!(
                background.next(&mut receiver, &cancellation).await,
                Some(Trigger::Filesystem)
            );
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        sender
            .try_send(())
            .expect("one event fits the drained channel");
        assert_eq!(
            background.next(&mut receiver, &cancellation).await,
            Some(Trigger::Validation)
        );
        cancellation.cancel();
        assert_eq!(background.next(&mut receiver, &cancellation).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn rebuild_runs_first_until_the_validation_is_one_interval_late() {
        let background = BackgroundValidation::new(&ServerConfiguration::default());
        let interval = background.interval;
        tokio::time::advance(interval).await;
        assert!(
            background.rebuild_runs_first(Instant::now()),
            "a validation that just came due lets the owed rebuild run"
        );
        tokio::time::advance(interval.saturating_sub(Duration::from_millis(1))).await;
        assert!(background.rebuild_runs_first(Instant::now()));
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(
            !background.rebuild_runs_first(Instant::now()),
            "a validation one interval late runs before the rebuild"
        );
    }

    /// Runs the supervisor over one edit whose rebuild is superseded at publication, with
    /// a second file created without an observation, and answers the paths each capture
    /// was asked for, in order, with the records the supervisor wrote.
    ///
    /// The first capture reads the edit and then waits. While it waits the test moves the
    /// clock `late` past the supervisor's start, rewrites the edited file, observes it,
    /// and creates the second file. Only a validation finds that file, so the paths the
    /// second capture names say whether the validation ran before it.
    async fn captures_after_a_superseded_rebuild(
        late: Duration,
    ) -> Result<(Vec<Vec<String>>, Vec<String>), Box<dyn std::error::Error>> {
        use tracing_subscriber::layer::SubscriberExt as _;

        let (sink, mut drain) = rift_tracing::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let _guard = tracing::subscriber::set_default(subscriber);
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(
            root.join("rift.toml"),
            "[server]\nvalidation_interval = \"1s\"\n",
        )?;
        std::fs::write(root.join("lib.rs"), "pub fn old() {}\n")?;
        let (context, invalidations) = stable_context(root)?;
        let published = Arc::clone(&context.published);
        let validation = Arc::clone(&context.validation);
        let (started, mut started_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_receiver) = std::sync::mpsc::sync_channel::<()>(0);
        let release_receiver = Mutex::new(Some(release_receiver));
        let captures = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::clone(&captures);
        let capture = Arc::new(
            move |root: &std::path::Path,
                  limits: rift_index::WorkspaceIndexLimits,
                  request: &super::super::RebuildRequest| {
                asked
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(
                        request
                            .work
                            .paths()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>(),
                    );
                let candidate = super::super::build_workspace_candidate(root, limits, request);
                let held = release_receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(held) = held {
                    let _ = started.send(());
                    held.recv().expect("the test releases the first capture");
                }
                candidate
            },
        );
        let watcher = super::super::unwatched(root, &validation)?;
        let supervisor = tokio::spawn(super::super::run_index_supervisor_with(
            watcher,
            invalidations,
            context,
            move |root: &std::path::Path,
                  limits: rift_index::WorkspaceIndexLimits,
                  request: &super::super::RebuildRequest| {
                capture(root, limits, request)
            },
        ));
        let edited = rift_core::ProjectPath::new("lib.rs")?;
        std::fs::write(root.join("lib.rs"), "pub fn first() {}\n")?;
        validation.observe_paths([edited.clone()])?;
        started_receiver
            .recv()
            .await
            .ok_or("the supervisor must start the first capture")?;
        tokio::time::advance(late).await;
        std::fs::write(root.join("lib.rs"), "pub fn second() {}\n")?;
        validation.observe_paths([edited])?;
        std::fs::write(root.join("unreported.rs"), "pub fn unreported() {}\n")?;
        release
            .send(())
            .map_err(|_| "the first capture must still wait when the test releases it")?;
        let unreported = rift_core::ProjectPath::new("unreported.rs")?;
        let settled = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let changed = validation.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let (current, failure) = published.read().await.snapshot();
                if failure.is_none() && current.reads.workspace_digests().get(&unreported).is_some()
                {
                    return;
                }
                changed.await;
            }
        })
        .await;
        validation.cancellation.cancel();
        supervisor.await?;
        settled.map_err(|_| "the unreported file must publish")?;
        let mut messages = Vec::new();
        while let Ok(record) = drain.try_recv_record() {
            messages.push(record.message().to_owned());
        }
        let captures = captures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok((captures, messages))
    }

    const VALIDATION_DEFERRED: &str = "background filesystem validation deferred";

    #[tokio::test(start_paused = true)]
    async fn supervisor_runs_the_owed_rebuild_before_a_due_validation_after_a_superseded_rebuild()
    -> Result<(), Box<dyn std::error::Error>> {
        let (captures, messages) =
            captures_after_a_superseded_rebuild(Duration::from_secs(1)).await?;
        assert_eq!(
            captures,
            [["lib.rs"], ["lib.rs"], ["unreported.rs"]],
            "the rebuild the superseded one left owed runs before the due validation"
        );
        let deferred = messages
            .iter()
            .filter(|message| *message == VALIDATION_DEFERRED)
            .count();
        assert_eq!(deferred, 1, "one record for the one deferred validation");
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_runs_a_validation_one_interval_late_before_the_owed_rebuild()
    -> Result<(), Box<dyn std::error::Error>> {
        let (captures, messages) =
            captures_after_a_superseded_rebuild(Duration::from_secs(2)).await?;
        assert_eq!(captures.len(), 2, "{captures:?}");
        assert_eq!(captures[0], ["lib.rs"]);
        assert_eq!(
            captures[1],
            ["lib.rs", "unreported.rs"],
            "a validation one interval late runs first, and one rebuild reads both files"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message == VALIDATION_DEFERRED),
            "nothing was deferred"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn held_lock_defers_and_retained_lock_expires_without_renewal() {
        let mut hold = VersionControlHold::new(None, &ServerConfiguration::default());
        let now = Instant::now();
        assert_eq!(
            hold.decision(true, now),
            HoldDecision::WaitUntil(now + Duration::from_secs(3))
        );
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(hold.decision(true, Instant::now()), HoldDecision::Expired);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(hold.decision(true, Instant::now()), HoldDecision::Expired);
        assert_eq!(hold.decision(false, Instant::now()), HoldDecision::Proceed);
        assert!(matches!(
            hold.decision(true, Instant::now()),
            HoldDecision::WaitUntil(_)
        ));
    }

    /// One span closes for each stage of the visible-file capture that follows the
    /// indexed-file capture, so a validation's record says where its time went.
    #[test]
    fn validation_capture_closes_one_span_for_each_visible_stage()
    -> Result<(), Box<dyn std::error::Error>> {
        use tracing_subscriber::layer::SubscriberExt as _;

        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn old() {}\n")?;
        std::fs::write(
            directory.path().join("opaque.unknown"),
            "unclassified bytes",
        )?;
        let configuration = super::super::ConfigurationState::accept(directory.path());
        let (sink, mut drain) = rift_tracing::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let guard = tracing::subscriber::set_default(subscriber);
        let (_indexed, visible, _next) = capture_visible_digests_with_languages_cancellable(
            directory.path(),
            configuration.index_limits(rift_index::WorkspaceIndexLimits::default())?,
            &configuration.source_visibility(),
            &configuration.text_inclusion(),
            &configuration.language_file_selections(),
            &LastCapture::default(),
            &|| false,
        )?;
        drop(guard);
        assert_eq!(visible.len(), 2);
        let mut closed = Vec::new();
        while let Ok(record) = drain.try_recv_record() {
            if record.fields().contains("\"span\":\"closed\"") {
                closed.push(record.message().to_owned());
            }
        }
        for stage in [
            "fingerprint.source_policy",
            "fingerprint.visible_paths",
            "fingerprint.visible_read",
        ] {
            let count = closed.iter().filter(|name| *name == stage).count();
            assert_eq!(
                count, 1,
                "one closed span for the {stage} stage: {closed:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn background_capture_recovers_missing_events_and_leaves_unchanged_publication_alone()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn old() {}\n")?;
        let limits = rift_index::WorkspaceIndexLimits::default();
        let super::super::WorkspaceCandidate::Stable {
            published: current, ..
        } = super::super::build_workspace_candidate(
            directory.path(),
            limits,
            &super::super::RebuildRequest::initial(0),
        )?
        else {
            return Err("fixture configuration must remain stable".into());
        };
        let (validation, _invalidations) = super::super::IndexValidation::new(limits.files_max());
        let published = Arc::new(tokio::sync::RwLock::new(super::super::IndexState {
            current: Arc::clone(&current),
            failure: None,
        }));
        let context = IndexSupervisorContext {
            root: directory.path().to_path_buf(),
            limits,
            published,
            validation,
            blocking: crate::server::BlockingExecutor::isolated(1, 60_000),
            population: None,
            lexical: None,
        };
        let mut background = BackgroundValidation::new(&ServerConfiguration::default());
        background.validate(&context).await?;
        background.validate(&context).await?;
        assert_eq!(
            context.validation.observed_epoch(),
            0,
            "an unchanged validation queues no work"
        );
        assert!(
            Arc::ptr_eq(&context.published.read().await.snapshot().0, &current),
            "an unchanged validation keeps the same publication"
        );
        std::fs::write(directory.path().join("lib.rs"), "pub fn new() {}\n")?;
        std::fs::write(directory.path().join("added.rs"), "pub fn added() {}\n")?;
        background.validate(&context).await?;
        assert_eq!(
            context.validation.observed_epoch(),
            1,
            "a timer capture detects edits and additions without a watcher event"
        );
        let pending = context.validation.take_pending();
        assert!(
            !pending.work.covers_whole_workspace(),
            "ordinary missed edits and creates use incremental work"
        );
        assert_eq!(
            pending
                .work
                .paths()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["added.rs", "lib.rs"]
        );
        Ok(())
    }

    #[tokio::test]
    async fn partial_publication_discovers_a_file_created_without_an_event()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn old() {}\n")?;
        let limits = rift_index::WorkspaceIndexLimits::default();
        let (mut current, _preparation) = super::super::empty_workspace_preparation(
            directory.path(),
            limits,
            super::super::ConfigurationState::accept(directory.path()),
            0,
            rift_index::WorkspaceContentCache::default(),
        )?;
        let preparation = Arc::get_mut(&mut current)
            .expect("fixture owns the empty publication")
            .preparation
            .as_mut()
            .expect("empty fixture is preparing");
        preparation.total = Some(1);
        preparation.map_source_paths = Arc::new(vec![(
            rift_core::ProjectPath::new("lib.rs")?,
            rift_protocol::read::Language::from_identity_segment("rust")?,
        )]);
        let unchanged = Arc::clone(&current);
        let (validation, _invalidations) = super::super::IndexValidation::new(limits.files_max());
        let context = IndexSupervisorContext {
            root: directory.path().to_path_buf(),
            limits,
            published: Arc::new(tokio::sync::RwLock::new(super::super::IndexState {
                current,
                failure: None,
            })),
            validation,
            blocking: crate::server::BlockingExecutor::isolated(1, 60_000),
            population: None,
            lexical: None,
        };
        let mut background = BackgroundValidation::new(&ServerConfiguration::default());
        background.validate(&context).await?;
        assert_eq!(
            context.validation.observed_epoch(),
            0,
            "an unchanged discovered partial publication queues nothing"
        );
        assert!(Arc::ptr_eq(
            &context.published.read().await.snapshot().0,
            &unchanged
        ));
        std::fs::write(directory.path().join("added.rs"), "pub fn added() {}\n")?;
        background.validate(&context).await?;
        let request = context.validation.take_pending();
        assert!(
            request.work.covers_whole_workspace(),
            "a partial publication reaccepts the complete discovery"
        );
        assert_eq!(
            super::super::rebuild_workspace(&context, request, super::super::workspace_capture())
                .await?,
            super::super::RebuildOutcome::Published
        );
        let (current, failure) = context.published.read().await.snapshot();
        assert!(failure.is_none());
        assert!(
            current
                .reads
                .workspace_digests()
                .get(&rift_core::ProjectPath::new("added.rs")?)
                .is_some(),
            "the missed create becomes readable"
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_rebuild_recovers_after_byte_identical_restore_without_a_watcher_event()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let original = "pub fn old() {}\n";
        std::fs::write(directory.path().join("lib.rs"), original)?;
        let (context, _invalidations) = stable_context(directory.path())?;
        let current = context.published.read().await.snapshot().0;
        let original_reads = Arc::clone(&current.reads);
        std::fs::write(directory.path().join("lib.rs"), "pub fn moved() {}\n")?;
        let epoch = context
            .validation
            .observe_paths([rift_core::ProjectPath::new("lib.rs")?])?;
        let request = context.validation.take_pending();
        let error = super::super::rebuild_workspace(
            &context,
            request,
            |_root: &std::path::Path,
             _limits: rift_index::WorkspaceIndexLimits,
             _request: &super::super::RebuildRequest| {
                errors::server::read_unavailable()
                    .operation("workspace capture")
                    .detail("forced capture failure")
                    .fail()
            },
        )
        .await
        .expect_err("the capture failure must be recorded");
        assert!(
            context
                .published
                .write()
                .await
                .record_failure(epoch, epoch, error)
        );
        std::fs::write(directory.path().join("lib.rs"), original)?;
        let mut background = BackgroundValidation::new(&ServerConfiguration::default());
        background.validate(&context).await?;
        let request = context.validation.take_pending();
        assert_eq!(
            super::super::rebuild_workspace(&context, request, super::super::workspace_capture())
                .await?,
            super::super::RebuildOutcome::Unchanged
        );
        let (current, failure) = context.published.read().await.snapshot();
        assert!(
            failure.is_none(),
            "a restored tree clears its recorded failure"
        );
        assert!(
            Arc::ptr_eq(&current.reads, &original_reads),
            "byte-identical recovery derives no syntax or documentation"
        );
        let super::super::WorkspaceCandidate::Stable {
            published: cold, ..
        } = super::super::build_workspace_candidate(
            directory.path(),
            context.limits,
            &super::super::RebuildRequest::initial(current.epoch),
        )?
        else {
            return Err("stable cold fixture required".into());
        };
        assert_eq!(current.fingerprint, cold.fingerprint);
        assert_eq!(
            current.reads.workspace_digests(),
            cold.reads.workspace_digests()
        );
        assert_eq!(current.reads.workspace_map(), cold.reads.workspace_map());
        assert_eq!(
            current.reads.index_documents(),
            cold.reads.index_documents()
        );
        assert_eq!(
            current.reads.documentation_snapshot().index(),
            cold.reads.documentation_snapshot().index()
        );
        background.validate(&context).await?;
        assert_eq!(
            context.validation.observed_epoch(),
            current.epoch,
            "an unchanged healthy tick owes no recovery"
        );
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_timer_publishes_missing_events_past_a_retained_git_lock()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("rift.toml"),
            "[server]\nvalidation_interval = \"1s\"\nversion_control_timeout = \"1ms\"\n",
        )?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn old() {}\n")?;
        rift_history::fixture::init(directory.path());
        rift_history::fixture::commit_all(directory.path(), "add source");
        let repository = rift_history::Repository::open(directory.path())?;
        let lock = repository.index_lock_path();
        std::fs::write(&lock, "retained lock")?;
        let limits = rift_index::WorkspaceIndexLimits::default();
        let super::super::WorkspaceCandidate::Stable {
            published: current, ..
        } = super::super::build_workspace_candidate(
            directory.path(),
            limits,
            &super::super::RebuildRequest::initial(0),
        )?
        else {
            return Err("stable fixture required".into());
        };
        let (validation, invalidations) = super::super::IndexValidation::new(limits.files_max());
        let published = Arc::new(tokio::sync::RwLock::new(super::super::IndexState {
            current,
            failure: None,
        }));
        let context = IndexSupervisorContext {
            root: directory.path().to_path_buf(),
            limits,
            published: Arc::clone(&published),
            validation: Arc::clone(&validation),
            blocking: crate::server::BlockingExecutor::isolated(1, 60_000),
            population: None,
            lexical: None,
        };
        let watcher = super::super::unwatched(directory.path(), &validation)?;
        let supervisor = tokio::spawn(super::super::run_index_supervisor(
            watcher,
            invalidations,
            context,
        ));
        std::fs::write(directory.path().join("added.rs"), "pub fn added() {}\n")?;
        let settled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let changed = validation.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let current = published.read().await.snapshot().0;
                if current
                    .reads
                    .workspace_digests()
                    .get(&rift_core::ProjectPath::new("added.rs")?)
                    .is_some()
                {
                    return Ok::<_, Box<dyn std::error::Error>>(current);
                }
                changed.await;
            }
        })
        .await;
        validation.cancellation.cancel();
        supervisor.await?;
        let current = settled??;
        assert!(
            lock.exists(),
            "publication passes an expired lock without removing Git state"
        );
        let super::super::WorkspaceCandidate::Stable {
            published: cold, ..
        } = super::super::build_workspace_candidate(
            directory.path(),
            limits,
            &super::super::RebuildRequest::initial(current.epoch),
        )?
        else {
            return Err("stable cold fixture required".into());
        };
        assert_eq!(
            current.fingerprint, cold.fingerprint,
            "timer recovery equals a cold build"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_raw_catalog_above_indexed_source_budget_matches_cold_publication()
    -> Result<(), Box<dyn std::error::Error>> {
        const RAW_FILES: usize = 9;
        const RAW_FILE_BYTES: usize = 2 << 20;
        const INDEXED_SOURCE_BYTES: usize = 16 << 20;
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(
            root.join("rift.toml"),
            "[source]\nworkspace_size = \"16mb\"\n\n[languages.rust]\nenabled = false\n\n[search.text]\ninclude = [\"**/*.txt\"]\n",
        )?;
        std::fs::write(root.join("notes.txt"), "kept text\n")?;
        let mut bytes = vec![b'r'; RAW_FILE_BYTES];
        for file in 0..RAW_FILES {
            let extension = match file % 3 {
                0 => "rs",
                1 => "bin",
                _ => "unknown",
            };
            bytes[0] = if extension == "bin" { 0 } else { b'r' };
            std::fs::write(root.join(format!("raw_{file}.{extension}")), &bytes)?;
        }
        assert!(RAW_FILES * bytes.len() > INDEXED_SOURCE_BYTES);
        let (context, mut invalidations) = stable_context(root)?;
        let current = context.published.read().await.snapshot().0;
        let policy = current.source_policy.as_ref().ok_or("source policy")?;
        assert_eq!(current.visible_digests.iter().count(), RAW_FILES + 2);
        assert_eq!(current.visible_digests.as_ref(), &policy.visible_digests()?);
        let mut background = BackgroundValidation::new(&ServerConfiguration::default());
        for _ in 0..2 {
            background.deadline = Instant::now();
            assert_eq!(
                background
                    .next(&mut invalidations, &context.validation.cancellation)
                    .await,
                Some(Trigger::Validation),
            );
            background.validate(&context).await?;
            assert_eq!(
                context.validation.observed_epoch(),
                0,
                "unchanged raw content above the indexed budget owes no rebuild"
            );
            assert!(Arc::ptr_eq(
                &context.published.read().await.snapshot().0,
                &current
            ));
        }
        assert_eq!(current.visible_digests.as_ref(), &policy.visible_digests()?);
        Ok(())
    }

    #[tokio::test]
    async fn supervisor_timer_recovers_raw_edits_creates_and_deletes_without_events()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(
            root.join("rift.toml"),
            "[server]\nvalidation_interval = \"1s\"\n\n[languages.rust]\nenabled = false\n\n[search.text]\ninclude = [\"**/*.txt\"]\n",
        )?;
        let entries: [(&str, &[u8]); 4] = [
            ("disabled.rs", b"pub fn disabled() {}\n"),
            ("binary.bin", b"source\0bytes"),
            ("invalid.bin", &[0xff, 0xfe]),
            ("opaque.unknown", b"unclassified bytes"),
        ];
        for (path, bytes) in entries {
            std::fs::write(root.join(path), bytes)?;
        }
        let (context, invalidations) = stable_context(root)?;
        let published = Arc::clone(&context.published);
        let validation = Arc::clone(&context.validation);
        let original = published.read().await.snapshot().0;
        let policy = Arc::clone(original.source_policy.as_ref().ok_or("source policy")?);
        let limits = context.limits;
        let watcher = super::super::unwatched(root, &validation)?;
        let supervisor = tokio::spawn(super::super::run_index_supervisor(
            watcher,
            invalidations,
            context,
        ));
        let recovered = async {
            for operation in ["edit", "create", "delete"] {
                for (path, bytes) in entries {
                    let path = if operation == "create" {
                        format!("added-{path}")
                    } else {
                        path.to_owned()
                    };
                    match operation {
                        "edit" => {
                            let mut changed = bytes.to_vec();
                            changed.extend_from_slice(b"changed");
                            std::fs::write(root.join(path), changed)?;
                        }
                        "create" => std::fs::write(root.join(path), bytes)?,
                        "delete" => std::fs::remove_file(root.join(path))?,
                        _ => unreachable!("three fixture operations"),
                    }
                }
                let expected = policy.visible_digests()?;
                let current = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let changed = validation.changed.notified();
                        tokio::pin!(changed);
                        changed.as_mut().enable();
                        let (current, failure) = published.read().await.snapshot();
                        if failure.is_none() && current.visible_digests.as_ref() == &expected {
                            return current;
                        }
                        changed.await;
                    }
                })
                .await?;
                assert!(
                    current.epoch > original.epoch,
                    "timer publishes missed raw observations"
                );
                assert_eq!(
                    current.reads.workspace_digests(),
                    original.reads.workspace_digests()
                );
                assert_eq!(
                    current.reads.index_documents(),
                    original.reads.index_documents()
                );
                let super::super::WorkspaceCandidate::Stable {
                    published: cold, ..
                } = super::super::build_workspace_candidate(
                    root,
                    limits,
                    &super::super::RebuildRequest::initial(current.epoch),
                )?
                else {
                    return Err("stable cold fixture required".into());
                };
                assert_eq!(
                    current.visible_digests, cold.visible_digests,
                    "raw timer recovery equals source bytes and a cold publication"
                );
                assert_eq!(current.visible_refused, cold.visible_refused);
            }
            assert_eq!(
                original.visible_digests.iter().count(),
                5,
                "raw recovery leaves the held publication unchanged",
            );
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        validation.cancellation.cancel();
        supervisor.await?;
        recovered
    }

    #[test]
    fn git_controls_outside_a_worktree_are_observed_and_repository_store_writes_are_ignored() {
        let root = std::path::Path::new("/worktree");
        let git_directory = PathBuf::from("/repository/.git/worktrees/linked");
        let mut roots = super::super::WatchRoots::at(root);
        roots.git_directory = Some(git_directory.clone());
        let (validation, _receiver) = super::super::IndexValidation::new(100);
        for control in ["HEAD", "index.lock"] {
            super::super::report_watch_outcome(
                &roots,
                &validation,
                Ok(
                    notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
                        .add_path(git_directory.join(control)),
                ),
            );
        }
        assert_eq!(validation.observed_epoch(), 2);
        super::super::report_watch_outcome(
            &roots,
            &validation,
            Ok(
                notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Any))
                    .add_path(git_directory.join(".rift/server.json")),
            ),
        );
        assert_eq!(
            validation.observed_epoch(),
            2,
            "repository store writes remain excluded"
        );
        assert!(validation.take_pending().work.covers_whole_workspace());
    }
}
