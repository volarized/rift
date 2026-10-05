//! The serving process's handles on the workspace's databases below `.rift`.
//!
//! The metrics database at `.rift/metrics` holds the server's own log records; the index
//! database at `.rift/index` holds the lexical and documentation rows; the vectors
//! database at `.rift/vectors` holds the vector ranking's rows and opens at the first
//! vector operation, never at start. Each opens on its own, so one that fails leaves the
//! others serving, and an open failure never deletes a file.

use std::path::Path;
use std::sync::Arc;

use rift_core::constants::{METRICS_DATABASE_FILE_NAME, RIFT_STATE_DIRECTORY};
use rift_error::causes;
use rift_index::{DatabaseName, DatabasePool, LazyDatabase, WorkspaceDatabase};
use rift_tracing::{LogReader, LogStore};

/// One serving process's storage handles for a workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    database: Option<Arc<WorkspaceDatabase>>,
    vectors: Option<Arc<LazyDatabase>>,
    logs: Option<Arc<LogStore>>,
    election: Option<Arc<crate::ElectionGuard>>,
}

impl WorkspaceStorage {
    /// Opens the metrics database, then the index database, and keeps a handle that opens
    /// the vectors database at its first use.
    ///
    /// A failure leaves that one handle absent: without the index database identifier
    /// search still serves, and without the metrics database the run records nothing.
    /// The existing file stays in place for inspection and recovery.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the state directory or database file created. A later open
    /// retries the idempotent schema migrations and never removes the existing file.
    pub async fn open(root: &Path) -> Self {
        Self::open_with_owner(root, None).await
    }

    /// Opens storage only while this process holds the workspace election.
    ///
    /// Each database's worker thread and the metrics writer thread keep the guard until
    /// they exit, including after cancellation or a shutdown timeout. The vectors
    /// database's worker receives the same guard when it opens.
    ///
    /// # Errors
    ///
    /// Returns an election storage error when `guard` belongs to another workspace
    /// or its state directory cannot be read. Neither failure opens the database.
    pub async fn open_elected(
        root: &Path,
        guard: Arc<crate::ElectionGuard>,
    ) -> Result<Self, rift_error::RiftError> {
        guard.validate_workspace(root)?;
        Ok(Self::open_with_owner(root, Some(guard)).await)
    }

    /// Opens workspace storage with an owner held by each database thread until it ends.
    ///
    /// The metrics database opens first, so the index database's open lands as a record.
    /// The vectors database opens at the first vector operation, under the same owner.
    pub(crate) async fn open_with_owner(
        root: &Path,
        election: Option<Arc<crate::ElectionGuard>>,
    ) -> Self {
        let owner = election.as_ref().map(|guard| {
            let owner: Arc<dyn Send + Sync> = Arc::<crate::ElectionGuard>::clone(guard);
            owner
        });
        let (logs, database, vectors) = match state_directory(root).await {
            Some(state_directory) => {
                let logs = open_log_store(&state_directory, owner.clone()).await;
                let database = open_index_database(root, &state_directory, owner.clone()).await;
                let vectors = LazyDatabase::new(
                    &DatabaseName::Vectors.path(&state_directory),
                    DatabaseName::Vectors,
                    owner,
                );
                (logs, database, Some(Arc::new(vectors)))
            }
            None => (None, None, None),
        };
        Self {
            database,
            vectors,
            logs,
            election,
        }
    }

    /// Whether storage opened under this held election.
    pub(crate) fn holds_election(&self, guard: &Arc<crate::ElectionGuard>) -> bool {
        self.election
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, guard))
    }

    /// The index database, when it opened.
    pub(crate) fn database(&self) -> Option<Arc<WorkspaceDatabase>> {
        self.database.as_ref().map(Arc::clone)
    }

    /// The handle that opens the vectors database at its first use, when the state
    /// directory exists.
    pub(crate) fn vectors(&self) -> Option<Arc<LazyDatabase>> {
        self.vectors.as_ref().map(Arc::clone)
    }

    /// The recorded diagnostics store, when the metrics database opened.
    #[must_use]
    pub fn logs(&self) -> Option<Arc<LogStore>> {
        self.logs.as_ref().map(Arc::clone)
    }

    /// A reader of this workspace's metrics database, when the file exists.
    ///
    /// Nothing is opened or created: no writer thread starts, no schema changes, and no
    /// state directory appears. The reader needs no server and no valid `rift.toml`.
    #[must_use]
    pub fn open_logs(root: &Path) -> Option<LogReader> {
        let path = root
            .join(RIFT_STATE_DIRECTORY)
            .join(METRICS_DATABASE_FILE_NAME);
        path.exists().then(|| LogReader::new(&path))
    }
}

/// The pool the workspace asks for, or the default pool while `rift.toml` is invalid.
fn configured_pool(root: &Path) -> DatabasePool {
    let search = crate::validation::ConfigurationState::accept(root).search_configuration();
    DatabasePool::new(
        u32::try_from(search.pool_slots).unwrap_or(u32::MAX),
        u32::try_from(search.busy_timeout.milliseconds()).unwrap_or(u32::MAX),
    )
    .memory_mapped(search.lexical.mmap_size.bytes())
    .journal_size_limited(search.journal_size_limit.bytes())
}

/// The workspace state directory, created when absent; `None` when it cannot be.
async fn state_directory(root: &Path) -> Option<std::path::PathBuf> {
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    match tokio::fs::create_dir(&state_directory).await {
        Ok(()) => Some(state_directory),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Some(state_directory),
        Err(error) => {
            rift_tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %state_directory.display(),
                error = %error,
                "could not create the workspace state directory; the server starts without its \
                 databases"
            );
            None
        }
    }
}

/// Opens the metrics database without replacing a failed file.
async fn open_log_store(
    state_directory: &Path,
    owner: Option<Arc<dyn Send + Sync>>,
) -> Option<Arc<LogStore>> {
    let metrics_path = state_directory.join(METRICS_DATABASE_FILE_NAME);
    match LogStore::open(&metrics_path, owner).await {
        Ok(logs) => Some(Arc::new(logs)),
        Err(error) => {
            let causes = causes(&error).join(": ");
            rift_tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %metrics_path.display(),
                error = %error,
                causes,
                "the metrics database failed to open; the server starts without recorded logs"
            );
            None
        }
    }
}

/// Opens the index database without replacing a failed file.
async fn open_index_database(
    root: &Path,
    state_directory: &Path,
    owner: Option<Arc<dyn Send + Sync>>,
) -> Option<Arc<WorkspaceDatabase>> {
    let database_path = DatabaseName::Index.path(state_directory);
    match WorkspaceDatabase::open_with_owner(
        &database_path,
        DatabaseName::Index,
        configured_pool(root),
        owner,
    )
    .await
    {
        Ok(database) => Some(database),
        Err(error) => {
            let causes = causes(&error).join(": ");
            rift_tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %database_path.display(),
                error = %error,
                causes,
                database = %DatabaseName::Index,
                "the index database failed to open; the server starts without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_error::errors;
    use std::sync::Arc;

    use rift_core::constants::{INDEX_DATABASE_FILE_NAME, RIFT_STATE_DIRECTORY};

    use super::WorkspaceStorage;

    #[tokio::test]
    async fn one_storage_owner_clones_the_same_database_handle() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let storage = WorkspaceStorage::open(directory.path()).await;
        let cloned = storage.clone();
        let first = storage.database.expect("the database opens");
        let second = cloned.database.expect("the clone keeps the database");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn an_open_failure_keeps_the_existing_database_path() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database_path = directory
            .path()
            .join(RIFT_STATE_DIRECTORY)
            .join(INDEX_DATABASE_FILE_NAME);
        std::fs::create_dir_all(&database_path).expect("a directory occupies the database path");

        let storage = WorkspaceStorage::open(directory.path()).await;

        assert!(storage.database.is_none());
        assert!(
            database_path.is_dir(),
            "the failed path must not be deleted"
        );
    }

    /// Logs record and answer while the index database is refused: the metrics database
    /// opens on its own.
    #[tokio::test]
    async fn a_refused_index_database_leaves_the_logs_recording() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database_path = directory
            .path()
            .join(RIFT_STATE_DIRECTORY)
            .join(INDEX_DATABASE_FILE_NAME);
        std::fs::create_dir_all(&database_path).expect("a directory occupies the database path");

        let storage = WorkspaceStorage::open(directory.path()).await;

        assert!(storage.database.is_none());
        let logs = storage
            .logs()
            .expect("the metrics database opens on its own");
        let record = rift_tracing::LogRecord::new(
            1,
            "warn",
            "rift",
            "storage",
            "database.open",
            "refused",
            "{}",
        );
        logs.append(&[record], 10).await.expect("the record lands");
        let reader =
            WorkspaceStorage::open_logs(directory.path()).expect("the metrics file exists");
        let read = reader
            .connect()
            .and_then(|reads| reads.recent(&rift_tracing::LogQuery::newest(10)))
            .expect("the record reads back");
        assert_eq!(read.len(), 1);
    }

    /// An elected start beside the single database of an earlier release creates the
    /// metrics and index databases, opens no vectors database, and leaves the four old
    /// files byte-identical: nothing reads, migrates, or removes them.
    #[tokio::test]
    async fn an_elected_start_leaves_an_earlier_database_untouched() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let state = directory.path().join(RIFT_STATE_DIRECTORY);
        std::fs::create_dir_all(&state).expect("the state directory");
        let earlier = [
            ("db", b"earlier pages".as_slice()),
            ("db-wal", b"earlier log".as_slice()),
            ("db-shm", b"earlier index".as_slice()),
            ("db.lock", b"".as_slice()),
        ];
        for (name, bytes) in earlier {
            std::fs::write(state.join(name), bytes).expect("an earlier file");
        }
        let guard = Arc::new(crate::claim(directory.path()).expect("the election"));

        let storage = WorkspaceStorage::open_elected(directory.path(), Arc::clone(&guard))
            .await
            .expect("elected storage opens");

        assert!(storage.database.is_some(), "the index database opens");
        assert!(storage.logs().is_some(), "the metrics database opens");
        assert!(state.join(INDEX_DATABASE_FILE_NAME).is_file());
        assert!(state.join("metrics").is_file());
        assert!(
            !state.join("vectors").exists(),
            "the vectors database waits for its first vector operation"
        );
        for (name, bytes) in earlier {
            assert_eq!(
                std::fs::read(state.join(name)).expect("the earlier file stays"),
                bytes,
                "{name} is byte-identical"
            );
        }
        let vectors = storage.vectors().expect("the vectors handle");
        vectors
            .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
            .expect("an unopened handle closes");
    }

    /// The election outlasts every database thread of elected storage: with the index and
    /// vectors databases closed and every guard handle dropped, the metrics writer held in
    /// its close checkpoint keeps the election, and a claim succeeds once it is released.
    #[tokio::test]
    async fn the_election_outlasts_a_held_metrics_writer() {
        const STEP_MAX: std::time::Duration = std::time::Duration::from_secs(10);
        let directory = tempfile::tempdir().expect("a temporary directory");
        let guard = Arc::new(crate::claim(directory.path()).expect("the election"));
        let storage = WorkspaceStorage::open_elected(directory.path(), Arc::clone(&guard))
            .await
            .expect("elected storage opens");
        let index = storage.database().expect("the index database opens");
        let vectors = storage.vectors().expect("the vectors handle");
        vectors
            .resolve(index.pool())
            .await
            .expect("the vectors database opens");
        let logs = storage.logs().expect("the metrics database opens");
        drop(storage);
        drop(guard);

        let closed = tokio::time::Instant::now() + STEP_MAX;
        index.shutdown(closed).await.expect("the index closes");
        vectors.shutdown(closed).await.expect("the vectors close");
        let (holding, release) = logs.hold_next_checkpoint();
        let short = tokio::time::Instant::now() + std::time::Duration::from_millis(50);
        let (close, holding) = tokio::join!(logs.close(short), holding);
        holding.expect("the writer holds its checkpoint");
        let close = close.expect("a held checkpoint ends the close at its deadline");
        assert!(
            matches!(close, rift_tracing::StoreClose::Timeout { .. }),
            "the held writer outlasts its close: {close:?}"
        );
        let refused =
            crate::claim(directory.path()).expect_err("the held metrics writer keeps the election");
        assert_eq!(refused.slug(), errors::mcp::election_already_serving::SLUG);

        release.send(()).expect("the held writer resumes");
        tokio::time::timeout(STEP_MAX, async {
            while crate::claim(directory.path()).is_err() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the released writer frees the election");
    }

    /// A refused vectors database leaves the index and metrics databases serving: each
    /// database opens on its own.
    #[tokio::test]
    async fn a_refused_vectors_database_leaves_the_others_open() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let state = directory.path().join(RIFT_STATE_DIRECTORY);
        std::fs::create_dir_all(state.join("vectors")).expect("a directory occupies vectors");

        let storage = WorkspaceStorage::open(directory.path()).await;

        let index = storage.database().expect("the index database opens");
        assert!(storage.logs().is_some(), "the metrics database opens");
        let vectors = storage.vectors().expect("the vectors handle");
        let refused = vectors
            .resolve(index.pool())
            .await
            .expect_err("a directory at the vectors path refuses the open");
        assert_eq!(refused.slug(), errors::index::database_failed::SLUG);
        assert!(state.join("vectors").is_dir(), "the failed path stays");
    }

    #[test]
    fn a_workspace_without_a_metrics_database_has_no_log_reader() {
        let directory = tempfile::tempdir().expect("a temporary directory");

        assert!(WorkspaceStorage::open_logs(directory.path()).is_none());
        assert!(!directory.path().join(RIFT_STATE_DIRECTORY).exists());
    }

    #[tokio::test]
    async fn elected_storage_refuses_another_workspaces_guard_before_opening() {
        let held = tempfile::tempdir().expect("held workspace");
        let requested = tempfile::tempdir().expect("requested workspace");
        let guard = Arc::new(crate::claim(held.path()).expect("held election"));
        let _requested_guard = crate::claim(requested.path()).expect("requested election");
        let error = WorkspaceStorage::open_elected(requested.path(), guard)
            .await
            .expect_err("a mismatched guard cannot open the database");
        assert_eq!(error.slug(), errors::mcp::election_storage_failed::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "validate workspace election")
        );
        for name in ["index", "metrics", "vectors"] {
            assert!(
                !requested.path().join(".rift").join(name).exists(),
                "{name}"
            );
        }
    }

    #[test]
    fn the_pool_maps_what_search_lexical_mmap_size_names() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        assert_eq!(
            super::configured_pool(directory.path()).mmap_bytes(),
            rift_protocol::configuration::LEXICAL_MMAP_BYTES_DEFAULT,
            "an absent key maps the default"
        );
        std::fs::write(
            directory.path().join("rift.toml"),
            "[search.lexical]\nmmap_size = \"8mb\"\n",
        )
        .expect("the workspace configuration writes");
        assert_eq!(
            super::configured_pool(directory.path()).mmap_bytes(),
            8 << 20
        );
    }

    #[test]
    fn the_pool_limits_the_write_ahead_log_to_search_journal_size_limit() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let default = rift_protocol::configuration::SearchConfiguration::default()
            .journal_size_limit
            .bytes();
        assert_eq!(
            super::configured_pool(directory.path()).journal_size_limit_bytes(),
            Some(default),
            "an absent key limits the log to the default"
        );
        std::fs::write(
            directory.path().join("rift.toml"),
            "[search]\njournal_size_limit = \"8mb\"\n",
        )
        .expect("the workspace configuration writes");
        assert_eq!(
            super::configured_pool(directory.path()).journal_size_limit_bytes(),
            Some(8 << 20)
        );
    }

    #[tokio::test]
    async fn opening_storage_does_not_create_a_missing_workspace_root() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let missing = directory.path().join("missing");

        let storage = WorkspaceStorage::open(&missing).await;

        assert!(storage.database.is_none());
        assert!(
            !missing.exists(),
            "storage must not fabricate the workspace root"
        );
    }
}
