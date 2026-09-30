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
//! wait for one process-wide turn and start with `BEGIN IMMEDIATE`. A checkout
//! waits for a free connection at most the pool's busy-wait budget, the same
//! budget a connection waits for a lock another process holds.
//!
//! Opening the file switches it to WAL and applies the schema migrations under
//! the file's migration lock, so processes opening one new file prepare it one
//! after the other.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use toasty::Db;
use toasty::db::{Connection, Transaction};
use toasty::stmt::{Type, Value};
use toasty_core::driver::operation::TransactionMode;
use toasty_driver_sqlite::Sqlite;
use tokio::sync::{Mutex, MutexGuard};
use tracing::Instrument as _;

use crate::documentation_store::{
    DocumentationManifestRecord, DocumentationReferenceRecord, DocumentationSourceRecord,
};
use crate::lexical::{LexicalDocumentRecord, LexicalFileRecord, LexicalIndexStateRecord};
use crate::lexical::{
    LexicalIndexError, MIGRATIONS, bound_as_usize, lexical_error_caused_by, require_pragma_row,
    storage_error,
};
use crate::log::LogRecordRow;
use crate::vector::VectorRecord;

/// Suffix the migration lock file appends to the database file's whole name: the
/// database `.rift/db` is prepared under `.rift/db.lock`.
const MIGRATION_LOCK_SUFFIX: &str = ".lock";

/// Wall-clock span between two attempts at a migration lock another process holds.
///
/// Preparing a new file takes a few milliseconds, so a process that meets the lock
/// takes it at most one span after the holder releases it.
const MIGRATION_LOCK_POLL: Duration = Duration::from_millis(10);

/// Connection count, wait bounds, and memory map size for one database file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabasePool {
    slots: u32,
    busy_timeout_ms: u32,
    mmap_bytes: u64,
}

impl DatabasePool {
    /// Builds the pool's bounds: connection slots, and the busy-wait budget
    /// that bounds a caller's wait for a free slot, the wait `SQLite` grants a
    /// connection for another process's lock before it refuses, and an open's
    /// wait for another process's migration lock. Connections read through
    /// `SQLite`'s page cache alone until [`Self::memory_mapped`] sets a map.
    #[must_use]
    pub const fn new(slots: u32, busy_timeout_ms: u32) -> Self {
        Self {
            slots,
            busy_timeout_ms,
            mmap_bytes: 0,
        }
    }

    /// The same pool with each connection mapping up to `mmap_bytes` of the file,
    /// the `[search.lexical] mmap_size` key. `SQLite` caps the map at its compiled
    /// `SQLITE_MAX_MMAP_SIZE`, the key's accepted maximum.
    #[must_use]
    pub const fn memory_mapped(self, mmap_bytes: u64) -> Self {
        Self { mmap_bytes, ..self }
    }

    /// The bytes of the file each connection maps; `0` maps nothing.
    #[must_use]
    pub const fn mmap_bytes(self) -> u64 {
        self.mmap_bytes
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
    /// The WAL switch and the migrations run under the file's migration lock, so a
    /// process that opens a new file while another prepares it waits, then finds WAL
    /// on and every migration recorded.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the database cannot be opened, another
    /// process holds its migration lock past the pool's busy-wait budget, or its
    /// schema migration fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation may leave the database file created without its schema
    /// applied. Reopening retries safely: schema migrations are idempotent, and
    /// the migration lock releases with the dropped future.
    pub async fn open(
        database_path: &Path,
        pool: DatabasePool,
    ) -> Result<Arc<Self>, LexicalIndexError> {
        let migration_lock = MigrationLock::acquire(database_path, pool).await?;
        let mut builder = Db::builder();
        builder
            .models(toasty::models!(
                LexicalDocumentRecord,
                LexicalFileRecord,
                LexicalIndexStateRecord,
                DocumentationManifestRecord,
                DocumentationSourceRecord,
                DocumentationReferenceRecord,
                VectorRecord,
                LogRecordRow
            ))
            .max_pool_size(bound_as_usize(pool.slots()))
            // The pool waits for a free slot without a bound unless told one ("Passing
            // `None` disables the timeout, which is the default"), and a read that met a
            // pool every other caller held would wait for as long as they did.
            .pool_wait_timeout(Some(Duration::from_millis(u64::from(
                pool.busy_timeout_ms(),
            ))));
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
        drop(migration_lock);
        Ok(Arc::new(Self {
            database,
            pool,
            writes: Mutex::new(()),
        }))
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

    /// Checks out one pooled connection and keeps its slot from every other
    /// caller until the returned value drops.
    ///
    /// Holding every slot is how a caller proves what it does against a pool
    /// with none free: its next checkout waits out the busy-wait budget and
    /// refuses.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when no slot frees within the pool's
    /// busy-wait budget.
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future gives up the wait; no slot is held.
    pub async fn hold_connection(&self) -> Result<HeldConnection, LexicalIndexError> {
        Ok(HeldConnection {
            _connection: self.database.connection().await.map_err(storage_error)?,
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
        // Every store operation checks a connection out, so the span sits at debug: an info
        // filter would print one closing line per checkout.
        let mut connection = self
            .database
            .connection()
            .instrument(tracing::debug_span!(
                "database.checkout",
                component = "database",
                operation = "database.checkout"
            ))
            .await
            .map_err(storage_error)?;
        configure_connection(&mut connection, self.pool, access).await?;
        Ok(connection)
    }
}

/// One pooled connection [`WorkspaceDatabase::hold_connection`] checked out; its
/// slot returns to the pool when this drops.
#[derive(Debug)]
#[must_use = "dropping the value returns the slot at once"]
pub struct HeldConnection {
    _connection: Connection,
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

/// The database file's migration lock: an exclusive lock on a companion file, held
/// while one process switches the file to WAL and applies the schema migrations.
///
/// `SQLite` cannot make either step wait for another process running it. On a new
/// file, `PRAGMA journal_mode = WAL` rewrites the file header in a read transaction it
/// then upgrades to a write transaction, and `SQLite` refuses that upgrade with
/// `SQLITE_BUSY` at once, without calling the busy handler, while another connection
/// holds the write lock. Toasty reads the applied migrations once and applies each
/// missing one in a transaction of its own, so two processes that both read an empty
/// set both apply the first migration.
///
/// The lock lives on a companion file because an exclusive lock on Windows blocks
/// other processes' reads of the locked file itself. It releases when the value drops,
/// and with the process when the process exits.
#[derive(Debug)]
struct MigrationLock {
    _file: File,
}

impl MigrationLock {
    /// Takes the migration lock of the database at `database_path`, waiting at most the
    /// pool's busy-wait budget for another process to release it.
    ///
    /// The wait tries the lock once every [`MIGRATION_LOCK_POLL`], so it makes at most
    /// the budget divided by that span, plus one, attempts.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the lock file cannot be opened or locked, or
    /// when another process still holds the lock once the budget has passed.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future gives up the wait; the lock is not held.
    async fn acquire(database_path: &Path, pool: DatabasePool) -> Result<Self, LexicalIndexError> {
        let lock_path = migration_lock_path(database_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| migration_lock_error(&lock_path, source))?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(u64::from(pool.busy_timeout_ms()));
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::Error(source)) => {
                    return Err(migration_lock_error(&lock_path, source));
                }
                Err(TryLockError::WouldBlock) if tokio::time::Instant::now() >= deadline => {
                    let held = migration_lock_held(pool.busy_timeout_ms());
                    return Err(migration_lock_error(&lock_path, held));
                }
                Err(TryLockError::WouldBlock) => tokio::time::sleep(MIGRATION_LOCK_POLL).await,
            }
        }
    }
}

/// The migration lock file of the database at `database_path`: the suffix appended to
/// the whole file name, beside the database.
fn migration_lock_path(database_path: &Path) -> PathBuf {
    let mut lock_path = OsString::from(database_path);
    lock_path.push(MIGRATION_LOCK_SUFFIX);
    PathBuf::from(lock_path)
}

/// A migration lock failure, naming the lock file.
fn migration_lock_error(
    lock_path: &Path,
    source: impl std::error::Error + Send + Sync + 'static,
) -> LexicalIndexError {
    lexical_error_caused_by(
        crate::lexical::LexicalIndexViolation::Storage,
        Some(lock_path),
        source,
    )
}

/// The cause an open reports when another process kept the migration lock for the
/// whole budget.
fn migration_lock_held(busy_timeout_ms: u32) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "another process held the migration lock for the whole busy-wait budget of \
             {busy_timeout_ms}ms; retry once that process has opened the database"
        ),
    )
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

/// Applies connection-local durability, lock wait, memory map, and access policy.
///
/// Every checkout sets each pragma again, one statement each, so a pooled connection
/// answers under this pool's policy whichever checkout opened it.
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
    let mmap_bytes = pool.mmap_bytes();
    toasty::sql::query(format!("PRAGMA mmap_size = {mmap_bytes}"))
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
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use toasty::stmt::{Type, Value};
    use toasty_driver_sqlite::Sqlite;

    use super::{DatabasePool, MIGRATION_LOCK_POLL, MigrationLock, WorkspaceDatabase};
    use crate::log::LogRecordRow;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn pool() -> DatabasePool {
        DatabasePool::new(4, 1_000)
    }

    /// The busy-wait budget of the one-slot pool a held-slot case reads from: the least
    /// `[search] busy_timeout` accepts.
    const HELD_POOL_BUSY_TIMEOUT_MS: u32 = 100;
    /// Bound on the whole read in a held-slot case, well past its budget: a read still
    /// waiting here has no bound of its own.
    const HELD_POOL_READ_MAX: Duration = Duration::from_secs(10);

    /// A read that meets a pool whose every slot is held refuses once the busy-wait budget
    /// passes, naming the wait, instead of waiting for as long as the holder keeps the slot.
    #[tokio::test]
    async fn a_read_that_meets_a_held_pool_refuses_within_the_busy_wait_budget() -> TestResult {
        let directory = tempfile::tempdir()?;
        let one_slot = DatabasePool::new(1, HELD_POOL_BUSY_TIMEOUT_MS);
        let database = WorkspaceDatabase::open(&directory.path().join("db"), one_slot).await?;
        let logs = crate::log::LogStore::attached(Arc::clone(&database));
        let held = database.connection().await?;

        let started = std::time::Instant::now();
        let newest = crate::log::LogQuery::newest(1);
        let refused = tokio::time::timeout(HELD_POOL_READ_MAX, logs.recent(&newest))
            .await
            .map_err(|_elapsed| "the read kept waiting for the held slot past its budget")?;
        let waited = started.elapsed();
        let error = refused.expect_err("a read that meets a held pool refuses");

        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            error.fault().is_connection_unavailable(),
            "the refusal names the missing connection: {error}"
        );
        let causes = rift_core::causes(&error).join(": ");
        assert!(causes.contains("waiting for a slot"), "{causes}");
        assert!(
            waited >= Duration::from_millis(u64::from(HELD_POOL_BUSY_TIMEOUT_MS)),
            "the read waits out the budget first: waited={waited:?}"
        );
        drop(held);
        let released = logs.recent(&crate::log::LogQuery::newest(1)).await?;
        assert!(released.is_empty(), "a freed slot serves the read again");
        Ok(())
    }

    /// A slot held through `hold_connection` blocks the next checkout the same way, and a
    /// refusal the store itself raised names no missing connection.
    #[tokio::test]
    async fn a_held_connection_keeps_its_slot_until_it_drops() -> TestResult {
        let directory = tempfile::tempdir()?;
        let one_slot = DatabasePool::new(1, HELD_POOL_BUSY_TIMEOUT_MS);
        let database = WorkspaceDatabase::open(&directory.path().join("db"), one_slot).await?;
        let held = database.hold_connection().await?;
        let refused = database
            .hold_connection()
            .await
            .expect_err("a second hold meets no free slot");
        assert!(refused.fault().is_connection_unavailable(), "{refused}");
        drop(held);
        let _again = database.hold_connection().await?;

        let unrelated = super::lexical_error_caused_by(
            crate::lexical::LexicalIndexViolation::Storage,
            None,
            std::io::Error::other("disk gone"),
        );
        assert!(
            !unrelated.fault().is_connection_unavailable(),
            "a store refusal is not a missing connection"
        );
        Ok(())
    }

    /// Starts a write transaction on the file at `path` through a pool of its own, the
    /// state another process is in while it rewrites a new file's header.
    async fn hold_write_lock(
        path: &Path,
    ) -> Result<(toasty::Db, toasty::db::Connection), Box<dyn std::error::Error>> {
        let mut builder = toasty::Db::builder();
        builder.models(toasty::models!(LogRecordRow));
        let writer = builder.build(Sqlite::open(path)).await?;
        let mut writing = writer.connection().await?;
        toasty::sql::statement("BEGIN IMMEDIATE")
            .exec(&mut writing)
            .await?;
        Ok((writer, writing))
    }

    /// A first open that meets another process preparing the same new file waits for it
    /// and then finds the file prepared.
    ///
    /// The test stands in for that process: it holds the migration lock, and `SQLite`'s
    /// write lock on the file, the state the process is in while its WAL switch rewrites
    /// the header. A second open whose own WAL switch met that write lock is refused
    /// `database is locked` at once, because `SQLite` calls no busy handler for the
    /// upgrade the switch makes. The runtime's clock is paused, so the sleep below returns
    /// only once the contender is parked.
    #[tokio::test(start_paused = true)]
    async fn a_first_open_waits_while_another_process_prepares_the_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let preparing = MigrationLock::acquire(&path, pool()).await?;
        let (writer, mut writing) = hold_write_lock(&path).await?;
        let contender_path = path.clone();
        let contender =
            tokio::spawn(async move { WorkspaceDatabase::open(&contender_path, pool()).await });

        tokio::time::sleep(MIGRATION_LOCK_POLL / 2).await;
        assert!(
            !contender.is_finished(),
            "the second open must wait while another process prepares the file"
        );
        toasty::sql::statement("ROLLBACK")
            .exec(&mut writing)
            .await?;
        drop(writing);
        drop(writer);
        drop(preparing);

        let database = contender.await??;
        let mut connection = database.connection().await?;
        let tables = toasty::sql::query(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' \
             AND name IN ('lexical_documents', 'semantic_vectors', 'log_records')",
        )
        .column_types([Type::I64])
        .exec(&mut connection)
        .await?;
        crate::lexical::require_pragma_row(&tables, &[Value::I64(3)])?;
        let released = std::fs::File::open(super::migration_lock_path(&path))?;
        released.try_lock()?;
        Ok(())
    }

    /// A migration lock another process keeps past the busy-wait budget refuses the open,
    /// naming the lock file and the budget, and leaves no database file behind.
    #[tokio::test(start_paused = true)]
    async fn a_migration_lock_held_past_the_budget_refuses_the_open() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let held = MigrationLock::acquire(&path, pool()).await?;
        let started = tokio::time::Instant::now();

        let refused = WorkspaceDatabase::open(&path, pool())
            .await
            .expect_err("a held migration lock refuses the open once the budget passes");

        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(u64::from(pool().busy_timeout_ms())),
            "the open waits out the budget first: waited={waited:?}"
        );
        assert_eq!(refused.descriptor().code(), "storage_failure");
        let rendered = refused.to_string();
        assert!(rendered.contains("db.lock"), "{rendered}");
        let causes = rift_core::causes(&refused).join(": ");
        assert!(causes.contains("busy-wait budget"), "{causes}");
        assert!(!path.exists(), "a refused open creates no database file");
        drop(held);
        Ok(())
    }

    /// The lock file for a database under a directory that does not exist cannot be
    /// created, and the open refuses naming it.
    #[tokio::test]
    async fn a_missing_database_directory_refuses_at_the_migration_lock() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("missing").join("db");

        let refused = WorkspaceDatabase::open(&path, pool())
            .await
            .expect_err("no lock file can be created under a missing directory");

        assert_eq!(refused.descriptor().code(), "storage_failure");
        assert!(refused.to_string().contains("db.lock"), "{refused}");
        Ok(())
    }

    #[test]
    fn the_migration_lock_path_appends_to_the_whole_file_name() {
        assert_eq!(
            super::migration_lock_path(Path::new("/workspace/.rift/db")),
            Path::new("/workspace/.rift/db.lock")
        );
        assert_eq!(
            super::migration_lock_path(Path::new("state/db.sqlite")),
            Path::new("state/db.sqlite.lock")
        );
    }

    #[tokio::test]
    async fn one_open_serves_every_store_in_the_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database = WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;

        let mut connection = database.connection().await?;
        let tables = toasty::sql::query(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' \
             AND name IN ('lexical_documents', 'semantic_vectors', 'log_records')",
        )
        .column_types([toasty::stmt::Type::I64])
        .exec(&mut connection)
        .await?;
        crate::lexical::require_pragma_row(&tables, &[toasty::stmt::Value::I64(3)])?;
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

    #[tokio::test]
    async fn every_checkout_maps_the_pool_memory_map_size() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let unmapped = WorkspaceDatabase::open(&path, pool()).await?;
        let mut reading = unmapped.connection().await?;
        crate::lexical::require_pragma_row(
            &toasty::sql::query("PRAGMA mmap_size")
                .column_types([Type::I64])
                .exec(&mut reading)
                .await?,
            &[Value::I64(0)],
        )?;
        drop(reading);
        drop(unmapped);

        let mapped_pool = pool().memory_mapped(1 << 20);
        assert_eq!(mapped_pool.mmap_bytes(), 1 << 20);
        let database = WorkspaceDatabase::open(&path, mapped_pool).await?;
        let mut reading = database.connection().await?;
        crate::lexical::require_pragma_row(
            &toasty::sql::query("PRAGMA mmap_size")
                .column_types([Type::I64])
                .exec(&mut reading)
                .await?,
            &[Value::I64(1 << 20)],
        )?;
        drop(reading);
        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        crate::lexical::require_pragma_row(
            &toasty::sql::query("PRAGMA mmap_size")
                .column_types([Type::I64])
                .exec(&mut transaction)
                .await?,
            &[Value::I64(1 << 20)],
        )?;
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
