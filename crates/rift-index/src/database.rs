//! The workspace database at `.rift/db`, and the one connection pool every
//! store in it shares.
//!
//! `SQLite` serializes writers per file, not per connection: two handles on one
//! file take the same write lock, and the loser is refused. Opening a handle
//! per store therefore bought nothing and cost correctness - a log write that
//! met an index rebuild came back `database is locked`. One pool, opened once
//! and shared, is what every store attaches to.
//!
//! The pool also owns what a connection has to be configured with, so the
//! journal mode and the busy timeout are set and proven in one place rather
//! than once per store.

use std::path::Path;
use std::sync::Arc;

use toasty::Db;
use toasty::db::Connection;
use toasty::stmt::{Type, Value};
use toasty_driver_sqlite::Sqlite;
use tokio::sync::{Mutex, MutexGuard};

use crate::lexical::{
    LexicalIndexError, MIGRATIONS, bound_as_usize, lexical_error_caused_by, require_pragma_row,
    storage_error,
};
use crate::lexical::{LexicalIndexStateRecord, LexicalUnitRecord};
use crate::log::LogRecordRow;
use crate::vector::SemanticVectorRecord;

/// What one pooled connection is configured with.
///
/// These are the file's bounds, not a store's: a store's own limits - how many
/// units it indexes, how many terms a query may carry - never reach the pool,
/// because the pool is shared and one store's bounds are not another's. The
/// v0.0.21 defect that taught this: a log drain that opened the file first
/// configured the pool with its own placeholder bounds, and the index commit
/// that followed refused at `unit_limit maximum 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabasePool {
    slots: u32,
    busy_timeout_ms: u32,
}

impl DatabasePool {
    /// Builds the pool's bounds: connection slots, and the busy-wait budget
    /// `SQLite` grants a connection before it refuses.
    #[must_use]
    pub const fn new(slots: u32, busy_timeout_ms: u32) -> Self {
        Self {
            slots,
            busy_timeout_ms,
        }
    }

    /// Pooled connection slots.
    #[must_use]
    pub const fn slots(self) -> u32 {
        self.slots
    }

    /// The busy-wait budget, in milliseconds.
    #[must_use]
    pub const fn busy_timeout_ms(self) -> u32 {
        self.busy_timeout_ms
    }
}

/// One open workspace database: the pool every store in the file shares.
///
/// Cloning the [`Arc`] shares the pool; opening the file twice does not.
#[derive(Debug)]
pub struct WorkspaceDatabase {
    database: Db,
    pool: DatabasePool,
    /// Serializes the file's writers.
    ///
    /// `SQLite` admits one writer per file and refuses the rest with `database
    /// is locked`; `busy_timeout` only decides how long a loser waits before it
    /// is told so. A shared pool does not change that - it removes the second
    /// pool, not the second writer. Writes therefore queue here, so a log
    /// append that arrives during an index commit waits for it and lands,
    /// rather than spending its busy budget and being dropped.
    writes: Mutex<()>,
}

impl WorkspaceDatabase {
    /// Opens (creating if absent) the workspace database at `database_path` and
    /// applies the schema every store in it declares.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the database cannot be opened or its
    /// schema migration fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the database file created without its schema
    /// applied. Reopening retries safely: schema migrations are idempotent.
    pub async fn open(
        database_path: &Path,
        pool: DatabasePool,
    ) -> Result<Arc<Self>, LexicalIndexError> {
        let mut builder = Db::builder();
        builder
            .models(toasty::models!(
                LexicalUnitRecord,
                LexicalIndexStateRecord,
                SemanticVectorRecord,
                LogRecordRow
            ))
            .max_pool_size(bound_as_usize(pool.slots()));
        let database = builder
            .build(Sqlite::open(database_path))
            .await
            .map_err(|source| {
                lexical_error_caused_by(
                    crate::lexical::LexicalIndexViolation::Storage,
                    Some(database_path),
                    source,
                )
            })?;
        let _migration_report = MIGRATIONS.apply(&database).await.map_err(|source| {
            lexical_error_caused_by(
                crate::lexical::LexicalIndexViolation::Storage,
                Some(database_path),
                source,
            )
        })?;
        Ok(Arc::new(Self {
            database,
            pool,
            writes: Mutex::new(()),
        }))
    }

    /// The bounds this database's connections carry.
    #[must_use]
    pub const fn pool(&self) -> DatabasePool {
        self.pool
    }

    /// Exclusive write access to the file: the file's write turn, and a
    /// connection to spend it on.
    ///
    /// Every store's write transaction opens through this. The guard holds the
    /// turn until it drops, so the transaction it carries is the file's only
    /// writer for its whole life.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when no connection can be configured.
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future releases the turn without writing.
    pub(crate) async fn writing(&self) -> Result<WriteAccess<'_>, LexicalIndexError> {
        let turn = self.writes.lock().await;
        let connection = self.connection().await?;
        Ok(WriteAccess {
            _turn: turn,
            connection,
        })
    }

    /// A pooled connection with the journal mode and busy timeout every store
    /// in this file requires.
    ///
    /// Foreign keys are not configured: no table here carries a foreign-key
    /// relationship to another.
    pub(crate) async fn connection(&self) -> Result<Connection, LexicalIndexError> {
        let mut connection = self.database.connection().await.map_err(storage_error)?;

        let journal_mode = toasty::sql::query("PRAGMA journal_mode = WAL")
            .column_types([Type::String])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        require_pragma_row(&journal_mode, &[Value::String("wal".to_owned())])?;

        let busy_timeout_ms = i64::from(self.pool.busy_timeout_ms());
        toasty::sql::query(format!("PRAGMA busy_timeout = {busy_timeout_ms}"))
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        let busy_timeout = toasty::sql::query("PRAGMA busy_timeout")
            .column_types([Type::I64])
            .exec(&mut connection)
            .await
            .map_err(storage_error)?;
        require_pragma_row(&busy_timeout, &[Value::I64(busy_timeout_ms)])?;

        Ok(connection)
    }
}

/// The file's write turn, held with the connection that spends it.
#[derive(Debug)]
pub(crate) struct WriteAccess<'database> {
    _turn: MutexGuard<'database, ()>,
    connection: Connection,
}

impl WriteAccess<'_> {
    /// The connection this turn writes through.
    pub(crate) const fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }
}

#[cfg(test)]
mod tests {
    use super::{DatabasePool, WorkspaceDatabase};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn pool() -> DatabasePool {
        DatabasePool::new(4, 1_000)
    }

    #[tokio::test]
    async fn one_open_serves_every_store_in_the_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;

        let mut connection = database.connection().await?;
        let tables =
            toasty::sql::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .column_types([toasty::stmt::Type::String])
                .exec(&mut connection)
                .await?;

        let names: Vec<String> = tables
            .iter()
            .filter_map(|row| match row {
                toasty::stmt::Value::Record(record) => match record.as_slice() {
                    [toasty::stmt::Value::String(name)] => Some(name.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        for expected in ["lexical_units", "semantic_vectors", "log_records"] {
            assert!(names.iter().any(|name| name == expected), "{names:?}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_reopened_database_applies_no_migration_twice() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let _first = WorkspaceDatabase::open(&path, pool()).await?;

        let reopened = WorkspaceDatabase::open(&path, pool()).await;

        assert!(reopened.is_ok(), "reopening must be idempotent");
        Ok(())
    }
}
