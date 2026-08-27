//! The workspace database at `.rift/db`, and the one connection pool every
//! store in it shares.
//!
//! `SQLite` serializes writers per file, not per connection: two handles on one
//! file take the same write lock, and the loser is refused. Opening a handle
//! per store therefore bought nothing and cost correctness - a log write that
//! met an index rebuild came back `database is locked`. One pool, opened once
//! and shared, is what every store attaches to.
//!
//! Read checkouts use WAL snapshots with `query_only` enabled. Write checkouts
//! wait for one process-wide turn and start with `BEGIN IMMEDIATE`.

use std::path::Path;
use std::sync::Arc;

use toasty::Db;
use toasty::db::{Connection, Transaction};
use toasty::stmt::{Type, Value};
use toasty_core::driver::operation::TransactionMode;
use toasty_driver_sqlite::Sqlite;
use tokio::sync::{Mutex, MutexGuard};

use crate::lexical::{
    LexicalIndexError, MIGRATIONS, bound_as_usize, lexical_error_caused_by, require_pragma_row,
    storage_error,
};
use crate::lexical::{LexicalIndexStateRecord, LexicalUnitRecord};
use crate::log::LogRecordRow;
use crate::vector::SemanticVectorRecord;

/// Connection count and lock-wait bounds for one database file.
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
    /// `SQLite` admits one writer per file. Writes queue here before taking a
    /// connection, so in-process writers never compete for `SQLite`'s file lock.
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
        let mut connection = database.connection().await.map_err(storage_error)?;
        configure_journal(&mut connection).await?;
        configure_connection(&mut connection, pool, ConnectionAccess::Write).await?;
        drop(connection);
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
        let connection = self.configured_connection(ConnectionAccess::Write).await?;
        Ok(WriteAccess {
            _turn: turn,
            connection,
        })
    }

    /// A read-only pooled connection with this file's connection-local pragmas.
    ///
    /// Foreign keys are not configured: no table here carries a foreign-key
    /// relationship to another.
    pub(crate) async fn connection(&self) -> Result<Connection, LexicalIndexError> {
        self.configured_connection(ConnectionAccess::Read).await
    }

    /// Checks out and configures one connection for its next operation.
    async fn configured_connection(
        &self,
        access: ConnectionAccess,
    ) -> Result<Connection, LexicalIndexError> {
        let mut connection = self.database.connection().await.map_err(storage_error)?;
        configure_connection(&mut connection, self.pool, access).await?;
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
    /// Starts this turn's transaction with `BEGIN IMMEDIATE`.
    ///
    /// The write lock is acquired before any read prerequisite, so a transaction never
    /// asks `SQLite` to upgrade a shared lock while another process writes.
    pub(crate) async fn transaction(&mut self) -> Result<Transaction<'_>, LexicalIndexError> {
        self.connection
            .transaction_builder()
            .mode(TransactionMode::Immediate)
            .begin()
            .await
            .map_err(storage_error)
    }
}

/// Whether one checkout may write.
#[derive(Clone, Copy, Debug)]
enum ConnectionAccess {
    Read,
    Write,
}

/// Selects WAL once for the database file.
async fn configure_journal(connection: &mut Connection) -> Result<(), LexicalIndexError> {
    let journal_mode = toasty::sql::query("PRAGMA journal_mode = WAL")
        .column_types([Type::String])
        .exec(connection)
        .await
        .map_err(storage_error)?;
    require_pragma_row(&journal_mode, &[Value::String("wal".to_owned())])
}

/// Applies connection-local durability, lock wait, and access policy.
async fn configure_connection(
    connection: &mut Connection,
    pool: DatabasePool,
    access: ConnectionAccess,
) -> Result<(), LexicalIndexError> {
    toasty::sql::query("PRAGMA synchronous = NORMAL")
        .exec(&mut *connection)
        .await
        .map_err(storage_error)?;
    let busy_timeout_ms = pool.busy_timeout_ms();
    toasty::sql::query(format!("PRAGMA busy_timeout = {busy_timeout_ms}"))
        .exec(&mut *connection)
        .await
        .map_err(storage_error)?;
    let query_only = match access {
        ConnectionAccess::Read => "ON",
        ConnectionAccess::Write => "OFF",
    };
    toasty::sql::query(format!("PRAGMA query_only = {query_only}"))
        .exec(connection)
        .await
        .map_err(storage_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use toasty::stmt::{Type, Value};

    use super::{DatabasePool, WorkspaceDatabase};
    use crate::log::LogRecordRow;

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

    #[tokio::test]
    async fn checkouts_carry_wal_normal_sync_busy_wait_and_access_policy() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;
        let mut reading = database.connection().await?;

        let journal = toasty::sql::query("PRAGMA journal_mode")
            .column_types([Type::String])
            .exec(&mut reading)
            .await?;
        let synchronous = toasty::sql::query("PRAGMA synchronous")
            .column_types([Type::I64])
            .exec(&mut reading)
            .await?;
        let busy_timeout = toasty::sql::query("PRAGMA busy_timeout")
            .column_types([Type::I64])
            .exec(&mut reading)
            .await?;
        let query_only = toasty::sql::query("PRAGMA query_only")
            .column_types([Type::I64])
            .exec(&mut reading)
            .await?;

        crate::lexical::require_pragma_row(&journal, &[Value::String("wal".to_owned())])?;
        crate::lexical::require_pragma_row(&synchronous, &[Value::I64(1)])?;
        crate::lexical::require_pragma_row(&busy_timeout, &[Value::I64(1_000)])?;
        crate::lexical::require_pragma_row(&query_only, &[Value::I64(1)])?;
        let refused = toasty::sql::statement("DELETE FROM log_records")
            .exec(&mut reading)
            .await;
        assert!(refused.is_err(), "a read checkout must refuse a write");
        drop(reading);

        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        let query_only = toasty::sql::query("PRAGMA query_only")
            .column_types([Type::I64])
            .exec(&mut transaction)
            .await?;
        crate::lexical::require_pragma_row(&query_only, &[Value::I64(0)])?;
        transaction.rollback().await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn write_checkouts_wait_for_the_process_write_turn() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;
        let first = database.writing().await?;
        let (waiting, reached_wait) = tokio::sync::oneshot::channel();
        let contender_database = Arc::clone(&database);
        let mut contender = tokio::spawn(async move {
            let _ = waiting.send(());
            contender_database.writing().await.map(drop)
        });
        reached_wait.await?;

        let blocked = tokio::time::timeout(Duration::from_millis(1), &mut contender).await;
        assert!(blocked.is_err(), "the second writer must remain queued");
        drop(first);

        tokio::time::timeout(Duration::from_secs(1), contender).await???;
        Ok(())
    }

    #[tokio::test]
    async fn wal_read_completes_while_an_immediate_write_is_uncommitted() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;
        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        LogRecordRow::create()
            .id(1)
            .recorded_at(1)
            .level("info".to_owned())
            .target("rift_index::database".to_owned())
            .component("storage".to_owned())
            .operation("database.test".to_owned())
            .message("uncommitted".to_owned())
            .fields("{}".to_owned())
            .exec(&mut transaction)
            .await?;
        let reader_database = Arc::clone(&database);
        let reader = tokio::spawn(async move {
            let mut connection = reader_database
                .connection()
                .await
                .expect("the read connection opens");
            LogRecordRow::all()
                .count()
                .exec(&mut connection)
                .await
                .expect("the snapshot count reads")
        });

        let visible = tokio::time::timeout(Duration::from_secs(1), reader).await??;
        assert_eq!(visible, 0, "a reader must see the last committed snapshot");
        transaction.commit().await?;
        Ok(())
    }
}
