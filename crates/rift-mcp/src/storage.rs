//! The serving process's one handle on the workspace database at `.rift/db`.
//!
//! Every store in that file attaches to this owner. Opening a store never opens the file,
//! and an open failure never deletes it: the database contains recorded diagnostics as well
//! as derived search rows.

use std::path::Path;
use std::sync::Arc;

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_index::{DatabasePool, LogStore, WorkspaceDatabase};

/// One serving process's storage handles for a workspace.
#[derive(Clone, Debug)]
pub struct WorkspaceStorage {
    database: Option<Arc<WorkspaceDatabase>>,
    logs: Option<Arc<LogStore>>,
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
        let database = open_workspace_database(root).await;
        let logs = database
            .as_ref()
            .map(|database| Arc::new(LogStore::attached(Arc::clone(database))));
        Self { database, logs }
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
}

/// Opens the file without replacing a failed database.
async fn open_workspace_database(root: &Path) -> Option<Arc<WorkspaceDatabase>> {
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
    match WorkspaceDatabase::open(&database_path, configured_pool(root)).await {
        Ok(database) => Some(database),
        Err(error) => {
            tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %database_path.display(),
                error = %error,
                "the workspace database failed to open; the server starts without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
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
