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

/// A deadline that filesystem events and requests never postpone.
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
                    tracing::debug!(
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
            tracing::warn!(component = "index", operation = "index.lock", path = %lock.display(), "Git index lock wait expired");
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
