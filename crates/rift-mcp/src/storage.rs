//! The serving process's one handle on the workspace database at `.rift/db`.
//!
//! Every store in that file attaches to this owner. Opening a store never opens the file,
//! and an open failure never deletes it: the database contains recorded diagnostics as well
//! as derived search rows.

use std::path::Path;
use std::sync::Arc;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_error::causes;
use rift_index::{DatabasePool, LogStore, WorkspaceDatabase};

/// One serving process's storage handles for a workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    database: Option<Arc<WorkspaceDatabase>>,
    logs: Option<Arc<LogStore>>,
    election: Option<Arc<crate::ElectionGuard>>,
}

impl WorkspaceStorage {
    /// Opens the workspace database once and attaches every store to it.
    ///
    /// A database failure leaves both handles absent so identifier search can still serve.
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
    /// The SQLite worker keeps the guard until it exits, including after cancellation
    /// or a shutdown timeout.
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

    /// Opens workspace storage with an owner held by its SQLite worker until it ends.
    pub(crate) async fn open_with_owner(
        root: &Path,
        election: Option<Arc<crate::ElectionGuard>>,
    ) -> Self {
        let owner = election.as_ref().map(|guard| {
            let owner: Arc<dyn Send + Sync> = Arc::<crate::ElectionGuard>::clone(guard);
            owner
        });
        let database = open_workspace_database(root, owner).await;
        let logs = database
            .as_ref()
            .map(|database| Arc::new(LogStore::attached(Arc::clone(database))));
        Self {
            database,
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

    /// The workspace database, when it opened.
    pub(crate) fn database(&self) -> Option<Arc<WorkspaceDatabase>> {
        self.database.as_ref().map(Arc::clone)
    }

    /// The recorded diagnostics store, when the database opened.
    #[must_use]
    pub fn logs(&self) -> Option<Arc<LogStore>> {
        self.logs.as_ref().map(Arc::clone)
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
}

/// Opens the file without replacing a failed database.
async fn open_workspace_database(
    root: &Path,
    owner: Option<Arc<dyn Send + Sync>>,
) -> Option<Arc<WorkspaceDatabase>> {
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    match tokio::fs::create_dir(&state_directory).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %state_directory.display(),
                error = %error,
                "could not create the workspace state directory; the server starts without the \
                 workspace database"
            );
            return None;
        }
    }
    let database_path = state_directory.join(WORKSPACE_DATABASE_FILE_NAME);
    match WorkspaceDatabase::open_with_owner(&database_path, configured_pool(root), owner).await {
        Ok(database) => Some(database),
        Err(error) => {
            let causes = causes(&error).join(": ");
            tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %database_path.display(),
                error = %error,
                causes,
                "the workspace database failed to open; the server starts without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use rift_error::errors;
    use std::sync::Arc;

    use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};

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
            .join(WORKSPACE_DATABASE_FILE_NAME);
        std::fs::create_dir_all(&database_path).expect("a directory occupies the database path");

        let storage = WorkspaceStorage::open(directory.path()).await;

        assert!(storage.database.is_none());
        assert!(
            database_path.is_dir(),
            "the failed path must not be deleted"
        );
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
        assert!(!requested.path().join(".rift/db").exists());
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
