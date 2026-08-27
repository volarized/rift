//! The process's one handle on the workspace database at `.rift/db`.
//!
//! Every store in that file - the lexical index, the vectors, the log records -
//! attaches to one pool, because `SQLite` serializes writers per file: a second
//! pool contributes connections that lose the same write lock, never
//! throughput. The serving process opens the file once here, and the search
//! index and the log drain both attach to what this returns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use rift_core::constants::{RIFT_STATE_DIRECTORY, WORKSPACE_DATABASE_FILE_NAME};
use rift_index::{DatabasePool, WorkspaceDatabase};
use tokio::sync::Mutex;

/// The databases this process holds open, one per workspace root.
///
/// The handles are weak: a workspace whose server is gone releases its pool,
/// and the next open for that root creates a fresh one. A single process-wide
/// handle would be wrong even though a server serves one workspace - the test
/// binary opens many, and handing the first root's database to the second is
/// how this cache first failed.
static WORKSPACE_DATABASES: Mutex<BTreeMap<PathBuf, Weak<WorkspaceDatabase>>> =
    Mutex::const_new(BTreeMap::new());

/// The process's workspace database, opening it on the first call.
///
/// The pool's bounds come from the workspace's own accepted `[search]` table,
/// never from the caller: the log drain and the search index both reach this,
/// and a caller-supplied bound let whichever arrived first configure the file
/// for the other. That is the v0.0.21 defect where a drain opening first left
/// the index committing against `units_max = 1`.
///
/// The database is a derived store, rebuildable from the workspace tree at any
/// time: an open failure deletes the file and retries exactly once before this
/// run gives up on it, rather than refusing to start over a file Rift can
/// always regenerate. A run that gives up serves identifier search alone and
/// records no logs, and says so in both places.
pub async fn workspace_database(root: &Path) -> Option<Arc<WorkspaceDatabase>> {
    let mut open = WORKSPACE_DATABASES.lock().await;
    if let Some(held) = open.get(root).and_then(Weak::upgrade) {
        return Some(held);
    }
    let database = open_workspace_database(root).await?;
    open.insert(root.to_path_buf(), Arc::downgrade(&database));
    Some(database)
}

/// The pool the workspace asks for, or the default pool while `rift.toml` is
/// invalid.
fn configured_pool(root: &Path) -> DatabasePool {
    let search = crate::validation::ConfigurationState::accept(root).search_configuration();
    DatabasePool::new(
        u32::try_from(search.pool_slots).unwrap_or(u32::MAX),
        u32::try_from(search.busy_timeout.milliseconds()).unwrap_or(u32::MAX),
    )
}

/// Opens the file, recreating it once when the first open fails.
async fn open_workspace_database(root: &Path) -> Option<Arc<WorkspaceDatabase>> {
    let pool = configured_pool(root);
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    if let Err(error) = tokio::fs::create_dir_all(&state_directory).await {
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
    let database_path = state_directory.join(WORKSPACE_DATABASE_FILE_NAME);
    match WorkspaceDatabase::open(&database_path, pool).await {
        Ok(database) => return Some(database),
        Err(error) => tracing::warn!(
            component = "storage",
            operation = "database.open",
            path = %database_path.display(),
            error = %error,
            "the workspace database failed to open; deleting and recreating it once"
        ),
    }
    let _ = tokio::fs::remove_file(&database_path).await;
    match WorkspaceDatabase::open(&database_path, pool).await {
        Ok(database) => Some(database),
        Err(error) => {
            tracing::warn!(
                component = "storage",
                operation = "database.open",
                path = %database_path.display(),
                error = %error,
                "the workspace database failed to open after recreation; the server starts \
                 without it"
            );
            None
        }
    }
}
