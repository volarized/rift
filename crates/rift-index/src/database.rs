//! The two workspace databases `rift-index` owns, `.rift/index` and
//! `.rift/vectors`, and the one connection pool each opens.
//!
//! Each database is one [`WorkspaceDatabase`]: its own worker thread, its own
//! pool, its own write turn, and its own busy timeout, so a write to one never
//! queues behind a transaction on the other. `SQLite` serializes writers per
//! file, not per connection: two handles on one file take the same write lock,
//! and the loser is refused. Within one file, one pool, opened once and shared,
//! is what every store of that file attaches to.
//!
//! Read checkouts use WAL snapshots with `query_only` enabled. Write checkouts
//! wait for the file's write turn and start with `BEGIN IMMEDIATE`. A checkout
//! waits for a free connection at most the pool's busy-wait budget, the same
//! budget a connection waits for a lock another process holds.
//!
//! Opening a file switches it to WAL and applies its database's migration set
//! under the file's migration lock, so processes opening one new file prepare
//! it one after the other. Toasty reads and records applied migrations in the
//! `__toasty_migrations` table of the file it opened, so each database carries
//! a migration set of its own, numbered from 1.

use std::ffi::OsString;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rift_core::constants::{
    INDEX_DATABASE_FILE_NAME, VECTORS_DATABASE_FILE_NAME, WRITE_AHEAD_LOG_SUFFIX,
};
use rift_error::{RiftError, errors};
use toasty::db::{Connection, Transaction};
use toasty::migration::MigrationSet;
use toasty::stmt::{Type, Value};
use toasty::{Db, ModelSet};
use toasty_core::driver::operation::TransactionMode;
use toasty_driver_sqlite::Sqlite;
use tokio::sync::{Mutex, MutexGuard};

use crate::database_thread::{DatabaseThread, SqliteThreadDriver};
use crate::documentation_store::{
    DocumentationManifestRecord, DocumentationReferenceRecord, DocumentationSourceRecord,
};
use crate::lexical::{INDEX_MIGRATIONS, bound_as_usize, require_pragma_row};
use crate::lexical::{LexicalDocumentRecord, LexicalFileRecord, LexicalIndexStateRecord};
use crate::vector::{VECTORS_MIGRATIONS, VectorRecord};

/// Suffix the migration lock file appends to the database file's whole name: the
/// database `.rift/index` is prepared under `.rift/index.lock`.
const MIGRATION_LOCK_SUFFIX: &str = ".lock";

/// One of the two databases `rift-index` owns below the workspace state directory.
///
/// The name decides the file, the models a pool registers, the migration set it
/// applies, and the worker thread's name. Its label equals its file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DatabaseName {
    /// `.rift/index`: lexical, trigram, and documentation rows.
    Index,
    /// `.rift/vectors`: the vector ranking's stored vectors.
    Vectors,
}

impl DatabaseName {
    /// The label a record or an error names this database by, equal to its file name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Index => INDEX_DATABASE_FILE_NAME,
            Self::Vectors => VECTORS_DATABASE_FILE_NAME,
        }
    }

    /// The database file below `state_directory`.
    #[must_use]
    pub fn path(self, state_directory: &Path) -> PathBuf {
        state_directory.join(self.label())
    }

    /// The write-ahead log `SQLite` keeps beside the database file below
    /// `state_directory`.
    #[must_use]
    pub fn write_ahead_log_path(self, state_directory: &Path) -> PathBuf {
        appended(&self.path(state_directory), WRITE_AHEAD_LOG_SUFFIX)
    }

    /// The migration lock file beside the database file below `state_directory`.
    #[must_use]
    pub fn migration_lock_path(self, state_directory: &Path) -> PathBuf {
        migration_lock_path(&self.path(state_directory))
    }

    /// The name the database's write turn is recorded under, waits and holds alike.
    #[must_use]
    pub const fn write_lock_name(self) -> &'static str {
        match self {
            Self::Index => "index.write",
            Self::Vectors => "vectors.write",
        }
    }

    /// The name of this database's worker thread. Linux keeps 15 bytes of a thread
    /// name, and both names fit.
    #[must_use]
    pub const fn thread_name(self) -> &'static str {
        match self {
            Self::Index => "rift-db-index",
            Self::Vectors => "rift-db-vectors",
        }
    }

    /// Asserts that a store attaching here attaches to `expected`.
    ///
    /// # Panics
    ///
    /// Panics when this is another database: a store attached to the wrong file
    /// finds none of its tables.
    #[track_caller]
    pub(crate) fn assert_is(self, expected: Self) {
        assert_eq!(
            self, expected,
            "a store must attach to its own database: attached={self:?}, expected={expected:?}"
        );
    }

    /// The models a pool on this database registers.
    fn models(self) -> ModelSet {
        match self {
            Self::Index => toasty::models!(
                LexicalDocumentRecord,
                LexicalFileRecord,
                LexicalIndexStateRecord,
                DocumentationManifestRecord,
                DocumentationSourceRecord,
                DocumentationReferenceRecord
            ),
            Self::Vectors => toasty::models!(VectorRecord),
        }
    }

    /// The migration set this database applies at open.
    const fn migrations(self) -> MigrationSet {
        match self {
            Self::Index => INDEX_MIGRATIONS,
            Self::Vectors => VECTORS_MIGRATIONS,
        }
    }

    /// The pool this database opens under, from the configured one:
    /// `[search.lexical] mmap_size` maps the index database alone.
    const fn pool(self, configured: DatabasePool) -> DatabasePool {
        match self {
            Self::Index => configured,
            Self::Vectors => configured.memory_mapped(0),
        }
    }

    /// The failure of an open of this database at `path`.
    pub(crate) fn failed(
        self,
        path: &Path,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> RiftError {
        errors::index::database_failed()
            .database(self.label())
            .path(path)
            .source(source)
            .error()
    }
}

impl std::fmt::Display for DatabaseName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// Wall-clock span between two attempts at a migration lock another process holds.
///
/// Preparing a new file takes a few milliseconds, so a process that meets the lock
/// takes it at most one span after the holder releases it.
const MIGRATION_LOCK_POLL: Duration = Duration::from_millis(10);

/// Connection count, wait bounds, memory map size, and write-ahead log size limit for one
/// database file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabasePool {
    slots: u32,
    busy_timeout_ms: u32,
    mmap_bytes: u64,
    journal_size_limit_bytes: Option<u64>,
}

impl DatabasePool {
    /// Builds the pool's bounds: connection slots, and the busy-wait budget
    /// that bounds a caller's wait for a free slot, the wait `SQLite` grants a
    /// connection for another process's lock before it refuses, and an open's
    /// wait for another process's migration lock. Connections read through
    /// `SQLite`'s page cache alone until [`Self::memory_mapped`] sets a map, and
    /// keep `SQLite`'s unlimited write-ahead log until [`Self::journal_size_limited`]
    /// sets a limit.
    #[must_use]
    pub const fn new(slots: u32, busy_timeout_ms: u32) -> Self {
        Self {
            slots,
            busy_timeout_ms,
            mmap_bytes: 0,
            journal_size_limit_bytes: None,
        }
    }

    /// The same pool with the write-ahead log cut back to `bytes` at the commit that
    /// restarts it, the `[search] journal_size_limit` key.
    ///
    /// `SQLite` truncates the log to the limit only when a commit completes the first
    /// transaction of a restarted log, so a larger transaction still grows the file past
    /// it until then. The limit is a setting of one connection, so every checkout sets it.
    #[must_use]
    pub const fn journal_size_limited(self, bytes: u64) -> Self {
        Self {
            journal_size_limit_bytes: Some(bytes),
            ..self
        }
    }

    /// The write-ahead log size limit in bytes; `None` keeps `SQLite`'s default of no limit.
    #[must_use]
    pub const fn journal_size_limit_bytes(self) -> Option<u64> {
        self.journal_size_limit_bytes
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
    name: DatabaseName,
    database: Db,
    pool: DatabasePool,
    thread: Arc<DatabaseThread>,
    /// Serializes the file's writers.
    ///
    /// `SQLite` admits one writer per file. Writes queue here before taking a
    /// connection, so in-process writers never compete for `SQLite`'s file lock.
    writes: Mutex<()>,
    /// Whether a shutdown has already run the close checkpoint.
    checkpointed: AtomicBool,
}

/// The row `PRAGMA wal_checkpoint(TRUNCATE)` answers: whether it met another connection's
/// lock, the frames the write-ahead log held, and the frames it moved into the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WalCheckpoint {
    busy: bool,
    log: i64,
    checkpointed: i64,
}

impl WalCheckpoint {
    /// Reads the one row of three integers the pragma answers.
    fn from_row(rows: &[Value]) -> Result<Self, RiftError> {
        if let [Value::Record(record)] = rows
            && let [Value::I64(busy), Value::I64(log), Value::I64(checkpointed)] = record.as_slice()
        {
            return Ok(Self {
                busy: *busy != 0,
                log: *log,
                checkpointed: *checkpointed,
            });
        }
        errors::index::lexical_storage()
            .with(rift_error::ErrorContext::new(
                "pragma",
                format!("unexpected wal_checkpoint row: rows={rows:?}"),
            ))
            .fail()
    }
}

impl WorkspaceDatabase {
    /// Opens (creating if absent) the database `name` at `database_path` and applies
    /// that database's migration set.
    ///
    /// The WAL switch and the migrations run under the file's migration lock, so a
    /// process that opens a new file while another prepares it waits, then finds WAL
    /// on and every migration recorded.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the database cannot be opened, another
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
        name: DatabaseName,
        pool: DatabasePool,
    ) -> Result<Arc<Self>, RiftError> {
        Self::open_with_owner(database_path, name, pool, None).await
    }

    /// Opens the database while retaining its serving owner until the SQLite worker ends.
    ///
    /// Once the worker starts, its owner remains held after a shutdown deadline or a cancelled opening future.
    ///
    /// # Errors
    ///
    /// Returns the same storage failures as [`Self::open`].
    pub async fn open_with_owner(
        database_path: &Path,
        name: DatabaseName,
        pool: DatabasePool,
        owner: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Arc<Self>, RiftError> {
        let pool = name.pool(pool);
        let migration_lock = MigrationLock::acquire(database_path, name, pool).await?;
        let mut builder = Db::builder();
        builder
            .models(name.models())
            .max_pool_size(bound_as_usize(pool.slots()))
            // The pool waits for a free slot without a bound unless told one ("Passing
            // `None` disables the timeout, which is the default"), and a read that met a
            // pool every other caller held would wait for as long as they did.
            .pool_wait_timeout(Some(Duration::from_millis(u64::from(
                pool.busy_timeout_ms(),
            ))));
        let sqlite = Arc::new(Sqlite::open(database_path));
        let thread = DatabaseThread::spawn(
            name,
            Arc::clone(&sqlite),
            bound_as_usize(pool.slots()),
            Duration::from_millis(u64::from(pool.busy_timeout_ms())),
            owner,
        )
        .await
        .map_err(|source| name.failed(database_path, source))?;
        let database = builder
            .build(SqliteThreadDriver::new(sqlite, Arc::clone(&thread)))
            .await
            .map_err(|source| name.failed(database_path, source))?;
        let mut connection = database
            .connection()
            .await
            .map_err(|source| name.failed(database_path, source))?;
        configure_journal(&mut connection).await?;
        configure_connection(&mut connection, pool, ConnectionAccess::Write).await?;
        drop(connection);
        let _migration_report = name
            .migrations()
            .apply(&database)
            .await
            .map_err(|source| name.failed(database_path, source))?;
        drop(migration_lock);
        Ok(Arc::new(Self {
            name,
            database,
            pool,
            thread,
            writes: Mutex::new(()),
            checkpointed: AtomicBool::new(false),
        }))
    }

    /// Which database this is.
    #[must_use]
    pub const fn name(&self) -> DatabaseName {
        self.name
    }

    /// The pool bounds this database opened under.
    #[must_use]
    pub const fn pool(&self) -> DatabasePool {
        self.pool
    }

    /// Checkpoints the write-ahead log, then stops the SQLite worker, both by `deadline`.
    ///
    /// The checkpoint waits for the file's write turn, sets the busy timeout of its write
    /// connection to zero, and runs `PRAGMA wal_checkpoint(TRUNCATE)`, which empties the log
    /// file unless another connection holds it busy. It records the `busy`, `log`, and
    /// `checkpointed` row as a `database.close` event. A turn not free by `deadline`, a busy
    /// answer, or a refused checkpoint does not fail the close: the worker still stops,
    /// and `SQLite` recovers whatever the log holds at the next open. The worker drops every
    /// connection it holds, and the last connection's close removes the log file.
    ///
    /// Only the first call checkpoints; a later one awaits the worker's stop alone.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the worker stops responding or panics.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future during the checkpoint releases the write turn; a worker stop
    /// already queued still completes.
    pub async fn shutdown(&self, deadline: tokio::time::Instant) -> Result<(), RiftError> {
        if !self.checkpointed.swap(true, Ordering::AcqRel) {
            self.checkpoint_before_close(deadline).await;
        }
        self.thread
            .shutdown(deadline)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())
    }

    /// The truncate checkpoint of [`Self::shutdown`], recorded and never failing the close.
    async fn checkpoint_before_close(&self, deadline: tokio::time::Instant) {
        let database = self.name.label();
        match tokio::time::timeout_at(deadline, self.truncate_write_ahead_log()).await {
            Ok(Ok(checkpoint)) => rift_tracing::info!(
                component = "storage",
                operation = "database.close",
                database,
                busy = checkpoint.busy,
                log = checkpoint.log,
                checkpointed = checkpoint.checkpointed,
                "database checkpointed its write-ahead log"
            ),
            Ok(Err(error)) => rift_tracing::warn!(
                component = "storage",
                operation = "database.close",
                database,
                %error,
                "database checkpoint failed; the write-ahead log stays for the next open"
            ),
            Err(_elapsed) => rift_tracing::warn!(
                component = "storage",
                operation = "database.close",
                database,
                "database checkpoint outlasted the shutdown deadline; the write-ahead log \
                 stays for the next open"
            ),
        }
    }

    /// Takes the write turn and truncates the write-ahead log without waiting on another
    /// connection's lock.
    async fn truncate_write_ahead_log(&self) -> Result<WalCheckpoint, RiftError> {
        let mut access = self.writing().await?;
        toasty::sql::query("PRAGMA busy_timeout = 0")
            .exec(&mut access.connection)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        let rows = toasty::sql::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .column_types([Type::I64, Type::I64, Type::I64])
            .exec(&mut access.connection)
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())?;
        WalCheckpoint::from_row(&rows)
    }

    /// Exclusive write access to the file: the file's write turn, and a
    /// connection to spend it on.
    ///
    /// Every store's write transaction opens through this. The guard holds the
    /// turn until it drops, so the transaction it carries is the file's only
    /// writer for its whole life. The turn is recorded as the lock
    /// [`DatabaseName::write_lock_name`]: a writer that waits records the wait, its
    /// waiting operation, and the operation that holds the turn, and the held turn
    /// stays in the table of operations in flight until the guard drops.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when no connection can be configured.
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future releases the turn without writing.
    pub(crate) async fn writing(&self) -> Result<WriteAccess<'_>, RiftError> {
        let turn = rift_tracing::lock(self.name.write_lock_name())
            .acquire(self.writes.lock())
            .await;
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
    /// Returns [`RiftError`] when no slot frees within the pool's
    /// busy-wait budget.
    ///
    /// # Cancel safety
    ///
    /// Dropping the returned future gives up the wait; no slot is held.
    pub async fn hold_connection(&self) -> Result<HeldConnection, RiftError> {
        Ok(HeldConnection {
            _connection: self
                .database
                .connection()
                .await
                .map_err(|source| errors::index::lexical_storage().source(source).error())?,
        })
    }

    /// A read-only pooled connection with this file's connection-local pragmas.
    ///
    /// Foreign keys are not configured: no table here carries a foreign-key
    /// relationship to another.
    pub(crate) async fn connection(&self) -> Result<Connection, RiftError> {
        self.configured_connection(ConnectionAccess::Read).await
    }

    /// Checks out and configures one connection for its next operation.
    async fn configured_connection(
        &self,
        access: ConnectionAccess,
    ) -> Result<Connection, RiftError> {
        // Every store operation checks a connection out, so the span sits at debug: an info
        // filter would print one closing line per checkout.
        let mut connection = rift_tracing::debug_span!(
            "database.checkout",
            component = "database",
            operation = "database.checkout"
        )
        .instrument(self.database.connection())
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
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
    _turn: rift_tracing::Held<MutexGuard<'database, ()>>,
    connection: Connection,
}

impl WriteAccess<'_> {
    /// Starts this turn's transaction with `BEGIN IMMEDIATE`.
    ///
    /// The write lock is acquired before any read prerequisite, so a transaction never
    /// asks `SQLite` to upgrade a shared lock while another process writes.
    pub(crate) async fn transaction(&mut self) -> Result<Transaction<'_>, RiftError> {
        self.connection
            .transaction_builder()
            .mode(TransactionMode::Immediate)
            .begin()
            .await
            .map_err(|source| errors::index::lexical_storage().source(source).error())
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
    /// Returns [`RiftError`] when the lock file cannot be opened or locked, or
    /// when another process still holds the lock once the budget has passed.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future gives up the wait; the lock is not held.
    async fn acquire(
        database_path: &Path,
        name: DatabaseName,
        pool: DatabasePool,
    ) -> Result<Self, RiftError> {
        let lock_path = migration_lock_path(database_path);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| name.failed(&lock_path, source))?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(u64::from(pool.busy_timeout_ms()));
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::Error(source)) => {
                    return Err(name.failed(&lock_path, source));
                }
                Err(TryLockError::WouldBlock) if tokio::time::Instant::now() >= deadline => {
                    let held = migration_lock_held(pool.busy_timeout_ms());
                    return Err(name.failed(&lock_path, held));
                }
                Err(TryLockError::WouldBlock) => tokio::time::sleep(MIGRATION_LOCK_POLL).await,
            }
        }
    }
}

/// The migration lock file of the database at `database_path`: the suffix appended to
/// the whole file name, beside the database.
fn migration_lock_path(database_path: &Path) -> PathBuf {
    appended(database_path, MIGRATION_LOCK_SUFFIX)
}

/// `path` with `suffix` appended to its whole file name, as `SQLite` names its
/// sidecar files.
fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut appended = OsString::from(path);
    appended.push(suffix);
    PathBuf::from(appended)
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
async fn configure_journal(connection: &mut Connection) -> Result<(), RiftError> {
    let journal_mode = toasty::sql::query("PRAGMA journal_mode = WAL")
        .column_types([Type::String])
        .exec(connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    require_pragma_row(&journal_mode, &[Value::String("wal".to_owned())])
}

/// Applies connection-local durability, lock wait, memory map, write-ahead log limit, and
/// access policy.
///
/// Every checkout sets each pragma again, one statement each, so a pooled connection
/// answers under this pool's policy whichever checkout opened it.
async fn configure_connection(
    connection: &mut Connection,
    pool: DatabasePool,
    access: ConnectionAccess,
) -> Result<(), RiftError> {
    toasty::sql::query("PRAGMA synchronous = NORMAL")
        .exec(&mut *connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    let busy_timeout_ms = pool.busy_timeout_ms();
    toasty::sql::query(format!("PRAGMA busy_timeout = {busy_timeout_ms}"))
        .exec(&mut *connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    let mmap_bytes = pool.mmap_bytes();
    toasty::sql::query(format!("PRAGMA mmap_size = {mmap_bytes}"))
        .exec(&mut *connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    // `-1` is `SQLite`'s own default, no limit; a value past `i64::MAX` cannot be stored, and
    // no configuration accepts one.
    let journal_size_limit = pool
        .journal_size_limit_bytes()
        .map_or(-1, |bytes| i64::try_from(bytes).unwrap_or(i64::MAX));
    toasty::sql::query(format!("PRAGMA journal_size_limit = {journal_size_limit}"))
        .exec(&mut *connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    let query_only = match access {
        ConnectionAccess::Read => "ON",
        ConnectionAccess::Write => "OFF",
    };
    toasty::sql::query(format!("PRAGMA query_only = {query_only}"))
        .exec(connection)
        .await
        .map_err(|source| errors::index::lexical_storage().source(source).error())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    use rift_core::ProjectPath;
    use rift_ranking::{
        DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
        SearchableField,
    };
    use rusqlite::OptionalExtension as _;
    use toasty::stmt::{Type, Value};
    use toasty_driver_sqlite::Sqlite;

    use super::{
        DatabaseName, DatabasePool, MIGRATION_LOCK_POLL, MigrationLock, WorkspaceDatabase, errors,
    };
    use crate::lexical::{LexicalIndexLimits, LexicalSearchIndex};
    use crate::vector::VectorRecord;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn pool() -> DatabasePool {
        DatabasePool::new(4, 1_000)
    }

    const CRASH_CHILD: &str = "RIFT_INDEX_SQLITE_CRASH_CHILD";
    const CRASH_DATABASE: &str = "RIFT_INDEX_SQLITE_CRASH_DATABASE";
    const CRASH_MARKER: &str = "RIFT_INDEX_SQLITE_CRASH_MARKER";

    fn crash_document(
        identity: &str,
        content: &str,
    ) -> Result<IndexDocument, Box<dyn std::error::Error>> {
        let fields = DocumentFields::empty()
            .with(SearchableField::Name, identity)
            .with(SearchableField::FileContent, content);
        Ok(IndexDocument::new(
            DocumentIdentity::new(identity)?,
            DocumentLocation::Project(ProjectPath::new(format!("{identity}.md"))?),
            DocumentKind::TextFile,
            fields.digest(),
            fields,
        )?)
    }

    fn crash_documents(old: bool) -> Result<Vec<IndexDocument>, Box<dyn std::error::Error>> {
        if old {
            Ok(vec![
                crash_document("old-a", "previous committed text alpha")?,
                crash_document("old-b", "previous committed text beta")?,
            ])
        } else {
            Ok(vec![
                crash_document("new-a", "uncommitted replacement text gamma")?,
                crash_document("new-b", "uncommitted replacement text delta")?,
            ])
        }
    }

    struct ChildGuard(Child);

    impl ChildGuard {
        fn terminate(&mut self) -> std::io::Result<std::process::ExitStatus> {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            self.0.kill()?;
            self.0.wait()
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }

    async fn crash_child() -> TestResult {
        let database_path =
            std::env::var_os(CRASH_DATABASE).ok_or("crash child has no database path")?;
        let marker_path = std::env::var_os(CRASH_MARKER).ok_or("crash child has no marker path")?;
        let database =
            WorkspaceDatabase::open(Path::new(&database_path), DatabaseName::Index, pool()).await?;
        let store =
            LexicalSearchIndex::attached(Arc::clone(&database), LexicalIndexLimits::default());
        let previous = crash_documents(true)?;
        store.replace_all(&previous, "before-crash").await?;

        let (started, _release) = database.thread.hold_next_commit_for_test().await?;
        let replacement = crash_documents(false)?;
        let _replacement_task =
            tokio::spawn(async move { store.replace_all(&replacement, "during-crash").await });
        tokio::time::timeout(Duration::from_secs(10), started).await??;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker_path)?;
        marker.write_all(b"lexical commit reached actor hold")?;
        marker.sync_all()?;
        std::future::pending::<TestResult>().await
    }

    fn assert_sqlite_and_fts_integrity(path: &Path) -> TestResult {
        let connection = rusqlite::Connection::open(path)?;
        let integrity: String =
            connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        assert_eq!(integrity, "ok", "SQLite integrity check");
        for index in ["lexical_documents_fts", "lexical_documents_trigram"] {
            connection.execute(
                &format!("INSERT INTO {index}({index}, rank) VALUES('integrity-check', 1)"),
                [],
            )?;
        }
        Ok(())
    }

    async fn assert_documents_equal(
        actual: &LexicalSearchIndex,
        expected: &[IndexDocument],
    ) -> TestResult {
        for document in expected {
            assert_eq!(
                actual.document(document.identity()).await?,
                Some(document.clone()),
                "reopened store must match cold store at {}",
                document.identity().as_str()
            );
        }
        Ok(())
    }

    async fn catch_up_crash_trigrams(store: &LexicalSearchIndex) -> TestResult {
        for _ in 0..64 {
            if store.index_trigrams().await?.pending() == 0 {
                return Ok(());
            }
        }
        Err("trigram index stayed pending past bounded batches".into())
    }

    #[derive(Debug, PartialEq)]
    struct LexicalSnapshot {
        documents: Vec<Vec<rusqlite::types::Value>>,
        terms: Vec<(String, String, i64, i64)>,
        tree_revision: Option<String>,
        trigram_pending: i64,
    }

    fn lexical_snapshot(path: &Path) -> Result<LexicalSnapshot, Box<dyn std::error::Error>> {
        let connection = rusqlite::Connection::open(path)?;
        let mut statement = connection.prepare(
            "SELECT identity, path, kind, digest, byte_length, byte_offset, name, \
             qualified_name, identifier_terms, signature, documentation, file_content \
             FROM lexical_documents ORDER BY identity",
        )?;
        let mut rows = statement.query([])?;
        let mut documents = Vec::new();
        while let Some(row) = rows.next()? {
            documents.push(
                (0..12)
                    .map(|column| row.get(column))
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        let terms = connection
            .prepare(
                "SELECT term, col, doc, cnt FROM lexical_documents_vocabulary \
             ORDER BY term, col",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tree_revision = connection
            .query_row(
                "SELECT tree_revision FROM lexical_index_state WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let trigram_pending =
            connection.query_row("SELECT COUNT(*) FROM lexical_trigram_pending", [], |row| {
                row.get(0)
            })?;
        Ok(LexicalSnapshot {
            documents,
            terms,
            tree_revision,
            trigram_pending,
        })
    }

    async fn kill_child_at_lexical_commit(database_path: &Path, marker_path: &Path) -> TestResult {
        let executable = std::env::current_exe()?;
        let child = Command::new(executable)
            .args([
                "--exact",
                "database::tests::abrupt_process_exit_before_lexical_commit_recovers_previous_publication",
                "--nocapture",
            ])
            .env(CRASH_CHILD, "1")
            .env(CRASH_DATABASE, database_path)
            .env(CRASH_MARKER, marker_path)
            .stdin(Stdio::null())
            .spawn()?;
        let mut child = ChildGuard(child);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if marker_path.exists() {
                break;
            }
            if let Some(status) = child.0.try_wait()? {
                return Err(format!("crash child exited before commit witness: {status}").into());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("crash child missed bounded commit witness deadline".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = child.terminate()?;
        assert!(
            !status.success(),
            "forced child exit must not report success"
        );
        Ok(())
    }

    async fn assert_crash_recovery_matches_cold(database_path: &Path) -> TestResult {
        let previous = crash_documents(true)?;
        let reopened = WorkspaceDatabase::open(database_path, DatabaseName::Index, pool()).await?;
        let recovered =
            LexicalSearchIndex::attached(Arc::clone(&reopened), LexicalIndexLimits::default());
        assert_eq!(
            recovered.tree_revision().await?,
            Some("before-crash".to_owned())
        );
        assert_documents_equal(&recovered, &previous).await?;
        catch_up_crash_trigrams(&recovered).await?;
        for replacement in crash_documents(false)? {
            assert_eq!(recovered.document(replacement.identity()).await?, None);
        }

        let cold_directory = tempfile::tempdir()?;
        let cold_path = cold_directory.path().join("db");
        let cold_database =
            WorkspaceDatabase::open(&cold_path, DatabaseName::Index, pool()).await?;
        let cold =
            LexicalSearchIndex::attached(Arc::clone(&cold_database), LexicalIndexLimits::default());
        cold.replace_all(&previous, "before-crash").await?;
        catch_up_crash_trigrams(&cold).await?;
        assert_documents_equal(&cold, &previous).await?;

        let shutdown_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        reopened.shutdown(shutdown_deadline).await?;
        cold_database.shutdown(shutdown_deadline).await?;
        drop(recovered);
        drop(cold);
        drop(reopened);
        drop(cold_database);

        let recovered_snapshot = lexical_snapshot(database_path)?;
        let cold_snapshot = lexical_snapshot(&cold_path)?;
        assert_eq!(recovered_snapshot.trigram_pending, 0);
        assert_eq!(cold_snapshot.trigram_pending, 0);
        assert_eq!(recovered_snapshot, cold_snapshot);
        assert_sqlite_and_fts_integrity(database_path)?;
        assert_sqlite_and_fts_integrity(&cold_path)?;
        Ok(())
    }

    /// Kills child process after lexical replacement reaches actor commit, before SQLite
    /// receives commit. Reopen must expose previous publication, matching cold write.
    #[tokio::test(flavor = "current_thread")]
    async fn abrupt_process_exit_before_lexical_commit_recovers_previous_publication() -> TestResult
    {
        if std::env::var_os(CRASH_CHILD).is_some() {
            return crash_child().await;
        }

        let directory = tempfile::tempdir()?;
        let database_path = directory.path().join("db");
        let marker_path = directory.path().join("commit-reached");
        kill_child_at_lexical_commit(&database_path, &marker_path).await?;
        assert_crash_recovery_matches_cold(&database_path).await
    }

    /// The `sqlite_schema` rows a new index database holds, one block per row in name
    /// order: `type`, `name`, `tbl_name`, then `sql`, bare where `SQLite` stores none.
    /// Compared through [`fixture_text`], whatever line endings the checkout wrote.
    const INDEX_SCHEMA: &str = include_str!("../tests/fixtures/index_schema.txt");
    /// The `sqlite_schema` rows a new vectors database holds, spelled as [`INDEX_SCHEMA`].
    const VECTORS_SCHEMA: &str = include_str!("../tests/fixtures/vectors_schema.txt");

    /// The schema rows of the file at `path`, rendered as [`INDEX_SCHEMA`] spells them.
    fn rendered_schema(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
        let connection = rusqlite::Connection::open(path)?;
        let mut statement = connection
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY name")?;
        let rows = statement.query_map([], |row| {
            let sql = row
                .get::<_, Option<String>>(3)?
                .map_or_else(String::new, |sql| format!(" {sql}"));
            Ok(format!(
                "type: {}\nname: {}\ntbl_name: {}\nsql:{sql}\n",
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let blocks = rows.collect::<Result<Vec<String>, _>>()?;
        Ok(blocks.join("\n"))
    }

    /// `fixture` with every line ending `\n`, as `SQLite` stores the schema text.
    ///
    /// A checkout's line endings follow its host's git configuration: a Windows runner
    /// converts every fixture line to `\r\n`, and `include_str!` keeps those bytes, while
    /// the schema text comes from string literals `rustc` reads with `\n` endings.
    fn fixture_text(fixture: &str) -> String {
        fixture.replace("\r\n", "\n")
    }

    /// The migrations the file at `path` recorded, as `(id, name)` in id order.
    fn recorded_migrations(path: &Path) -> Result<Vec<(i64, String)>, Box<dyn std::error::Error>> {
        let connection = rusqlite::Connection::open(path)?;
        let mut statement =
            connection.prepare("SELECT id, name FROM __toasty_migrations ORDER BY id")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Each new database holds exactly its recorded tables, indexes, and triggers: the
    /// index database every lexical and documentation table, the vectors database
    /// `semantic_vectors` alone, and neither a log table.
    #[tokio::test]
    async fn each_new_database_holds_its_recorded_schema() -> TestResult {
        let directory = tempfile::tempdir()?;
        for (name, expected) in [
            (DatabaseName::Index, INDEX_SCHEMA),
            (DatabaseName::Vectors, VECTORS_SCHEMA),
        ] {
            let path = name.path(directory.path());
            let database = WorkspaceDatabase::open(&path, name, pool()).await?;
            database
                .shutdown(tokio::time::Instant::now() + HELD_POOL_READ_MAX)
                .await?;
            assert_eq!(
                rendered_schema(&path)?,
                fixture_text(expected),
                "schema of {name}"
            );
        }
        Ok(())
    }

    /// A fixture a Windows checkout rewrote to `\r\n` reads as the one with `\n` endings.
    #[test]
    fn a_crlf_checkout_of_a_schema_fixture_reads_as_written() {
        let crlf = INDEX_SCHEMA.replace("\r\n", "\n").replace('\n', "\r\n");
        assert_eq!(fixture_text(&crlf), fixture_text(INDEX_SCHEMA));
        assert!(!fixture_text(&crlf).contains('\r'));
    }

    /// Toasty records a migration set per file: two databases opened in one directory
    /// each apply their own migration 1 and record it in their own table.
    #[tokio::test]
    async fn each_database_records_its_own_migration_set() -> TestResult {
        let directory = tempfile::tempdir()?;
        let index_path = DatabaseName::Index.path(directory.path());
        let vectors_path = DatabaseName::Vectors.path(directory.path());
        let index = WorkspaceDatabase::open(&index_path, DatabaseName::Index, pool()).await?;
        let vectors = WorkspaceDatabase::open(&vectors_path, DatabaseName::Vectors, pool()).await?;
        let reopened = WorkspaceDatabase::open(&index_path, DatabaseName::Index, pool()).await?;

        assert_eq!(index.name(), DatabaseName::Index);
        assert_eq!(vectors.name(), DatabaseName::Vectors);
        assert_eq!(
            recorded_migrations(&index_path)?,
            [(1, "index_schema".to_owned())]
        );
        assert_eq!(
            recorded_migrations(&vectors_path)?,
            [(1, "semantic_vectors".to_owned())]
        );
        drop(reopened);
        Ok(())
    }

    /// A store attached to the other database's file panics before it reads.
    #[tokio::test]
    #[should_panic(expected = "a store must attach to its own database")]
    async fn a_vector_store_refuses_the_index_database() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database = WorkspaceDatabase::open(
            &DatabaseName::Index.path(directory.path()),
            DatabaseName::Index,
            pool(),
        )
        .await
        .expect("the index database opens");
        let _store = crate::VectorStore::attached(database);
    }

    /// A lexical store attached to the vectors database panics before it reads.
    #[tokio::test]
    #[should_panic(expected = "a store must attach to its own database")]
    async fn a_lexical_store_refuses_the_vectors_database() {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database = WorkspaceDatabase::open(
            &DatabaseName::Vectors.path(directory.path()),
            DatabaseName::Vectors,
            pool(),
        )
        .await
        .expect("the vectors database opens");
        let _store = LexicalSearchIndex::attached(database, LexicalIndexLimits::default());
    }

    /// Each name answers its file, write-ahead log, and migration lock below the state
    /// directory, labelled as its file is named.
    #[test]
    fn each_database_name_answers_its_own_files() {
        let state = Path::new("/workspace/.rift");
        assert_eq!(DatabaseName::Index.label(), "index");
        assert_eq!(DatabaseName::Vectors.to_string(), "vectors");
        assert_eq!(
            DatabaseName::Index.path(state),
            Path::new("/workspace/.rift/index")
        );
        assert_eq!(
            DatabaseName::Vectors.write_ahead_log_path(state),
            Path::new("/workspace/.rift/vectors-wal")
        );
        assert_eq!(
            DatabaseName::Index.migration_lock_path(state),
            Path::new("/workspace/.rift/index.lock")
        );
        assert_eq!(
            DatabaseName::Vectors
                .pool(pool().memory_mapped(1 << 20))
                .mmap_bytes(),
            0
        );
        assert_eq!(
            DatabaseName::Index
                .pool(pool().memory_mapped(1 << 20))
                .mmap_bytes(),
            1 << 20
        );
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
        let database = WorkspaceDatabase::open(
            &directory.path().join("db"),
            DatabaseName::Vectors,
            one_slot,
        )
        .await?;
        let held = database.connection().await?;

        let started = std::time::Instant::now();
        let refused = tokio::time::timeout(HELD_POOL_READ_MAX, database.connection())
            .await
            .map_err(|_elapsed| "the read kept waiting for the held slot past its budget")?;
        let waited = started.elapsed();
        let error = refused.expect_err("a read that meets a held pool refuses");

        assert_eq!(error.slug().as_str(), "rift.index.lexical_storage");
        let causes = rift_error::causes(&error).join(": ");
        assert!(causes.contains("waiting for a slot"), "{causes}");
        assert!(
            waited >= Duration::from_millis(u64::from(HELD_POOL_BUSY_TIMEOUT_MS)),
            "the read waits out the budget first: waited={waited:?}"
        );
        drop(held);
        let mut released = database.connection().await?;
        let vectors = VectorRecord::all().count().exec(&mut released).await?;
        assert_eq!(vectors, 0, "a freed slot serves the read again");
        Ok(())
    }

    /// A slot held through `hold_connection` blocks the next checkout the same way, and a
    /// refusal the store itself raised names no missing connection.
    #[tokio::test]
    async fn a_held_connection_keeps_its_slot_until_it_drops() -> TestResult {
        let directory = tempfile::tempdir()?;
        let one_slot = DatabasePool::new(1, HELD_POOL_BUSY_TIMEOUT_MS);
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, one_slot)
                .await?;
        let held = database.hold_connection().await?;
        let refused = database
            .hold_connection()
            .await
            .expect_err("a second hold meets no free slot");
        assert_eq!(refused.slug().as_str(), "rift.index.lexical_storage");
        drop(held);
        let _again = database.hold_connection().await?;

        let unrelated = errors::index::lexical_storage()
            .source(std::io::Error::other("disk gone"))
            .error();
        assert_eq!(unrelated.slug().as_str(), "rift.index.lexical_storage");
        assert_eq!(
            std::error::Error::source(&unrelated)
                .map(ToString::to_string)
                .as_deref(),
            Some("disk gone")
        );
        Ok(())
    }

    /// Starts a write transaction on the file at `path` through a pool of its own, the
    /// state another process is in while it rewrites a new file's header.
    async fn hold_write_lock(
        path: &Path,
    ) -> Result<(toasty::Db, toasty::db::Connection), Box<dyn std::error::Error>> {
        let mut builder = toasty::Db::builder();
        builder.models(toasty::models!(VectorRecord));
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
    /// upgrade the switch makes. Poll the open once before waiting half a migration-lock
    /// interval, so the first attempt reaches the held lock before the test releases it.
    #[tokio::test]
    async fn a_first_open_waits_while_another_process_prepares_the_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let preparing = MigrationLock::acquire(&path, DatabaseName::Index, pool()).await?;
        let (writer, mut writing) = hold_write_lock(&path).await?;
        let contender = WorkspaceDatabase::open(&path, DatabaseName::Index, pool());
        tokio::pin!(contender);
        tokio::select! {
            biased;
            result = &mut contender => panic!(
                "the second open completed while another process prepares the file: {result:?}"
            ),
            () = tokio::time::sleep(MIGRATION_LOCK_POLL / 2) => {}
        }
        toasty::sql::statement("ROLLBACK")
            .exec(&mut writing)
            .await?;
        drop(writing);
        drop(writer);
        drop(preparing);

        let database = contender.await?;
        let mut connection = database.connection().await?;
        let tables = toasty::sql::query(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'table' \
             AND name IN ('lexical_documents', 'lexical_files', 'documentation_sources')",
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
        let held = MigrationLock::acquire(&path, DatabaseName::Index, pool()).await?;
        let started = tokio::time::Instant::now();

        let refused = WorkspaceDatabase::open(&path, DatabaseName::Index, pool())
            .await
            .expect_err("a held migration lock refuses the open once the budget passes");

        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(u64::from(pool().busy_timeout_ms())),
            "the open waits out the budget first: waited={waited:?}"
        );
        assert_eq!(refused.slug().as_str(), "rift.index.database_failed");
        let rendered = refused.to_string();
        assert!(rendered.contains("index database"), "{rendered}");
        assert!(rendered.contains("db.lock"), "{rendered}");
        let causes = rift_error::causes(&refused).join(": ");
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

        let refused = WorkspaceDatabase::open(&path, DatabaseName::Index, pool())
            .await
            .expect_err("no lock file can be created under a missing directory");

        assert_eq!(refused.slug().as_str(), "rift.index.database_failed");
        assert!(refused.to_string().contains("db.lock"), "{refused}");
        Ok(())
    }

    #[test]
    fn the_migration_lock_path_appends_to_the_whole_file_name() {
        assert_eq!(
            super::migration_lock_path(Path::new("/workspace/.rift/index")),
            Path::new("/workspace/.rift/index.lock")
        );
        assert_eq!(
            super::migration_lock_path(Path::new("state/db.sqlite")),
            Path::new("state/db.sqlite.lock")
        );
    }

    #[tokio::test]
    async fn a_reopened_database_applies_no_migration_twice() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let _first = WorkspaceDatabase::open(&path, DatabaseName::Index, pool()).await?;

        let reopened = WorkspaceDatabase::open(&path, DatabaseName::Index, pool()).await;

        assert!(reopened.is_ok(), "reopening must be idempotent");
        Ok(())
    }

    #[tokio::test]
    async fn checkouts_carry_wal_normal_sync_busy_wait_and_access_policy() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, pool())
                .await?;
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
        let refused = toasty::sql::statement("DELETE FROM lexical_files")
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
    async fn dropping_write_transaction_rolls_back_before_connection_reuse() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, pool())
                .await?;
        let mut writing = database.writing().await?;
        let transaction = writing.transaction().await?;
        drop(transaction);

        let transaction =
            tokio::time::timeout(Duration::from_secs(1), writing.transaction()).await??;
        transaction.rollback().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sqlite_lock_wait_does_not_block_async_runtime() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let database = WorkspaceDatabase::open(&path, DatabaseName::Index, pool()).await?;
        let mut writing = database.writing().await?;

        let lock_path = path.clone();
        let (locked, is_locked) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(0);
        let blocker = std::thread::spawn(move || -> Result<(), String> {
            let connection =
                rusqlite::Connection::open(lock_path).map_err(|error| error.to_string())?;
            connection
                .execute_batch("BEGIN IMMEDIATE")
                .map_err(|error| error.to_string())?;
            locked.send(()).map_err(|error| error.to_string())?;
            released.recv().map_err(|error| error.to_string())?;
            connection
                .execute_batch("ROLLBACK")
                .map_err(|error| error.to_string())
        });
        is_locked.recv_timeout(Duration::from_secs(1))?;

        let mut begin = Box::pin(writing.transaction());
        let (async_done, async_completed) = tokio::sync::oneshot::channel();
        let async_work = tokio::spawn(async move {
            tokio::task::yield_now().await;
            async_done
                .send(())
                .expect("async witness must still be listening");
        });
        tokio::select! {
            biased;
            result = &mut begin => match result {
                Ok(transaction) => {
                    drop(transaction);
                    panic!("SQLite begin succeeded while external lock remained held");
                }
                Err(error) => panic!("SQLite begin failed before external lock release: {error}"),
            },
            () = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        tokio::time::timeout(Duration::from_millis(100), async_completed).await??;
        async_work.await?;

        release.send(()).map_err(|error| error.to_string())?;
        blocker
            .join()
            .map_err(|_| "SQLite lock holder panicked")??;
        let transaction = tokio::time::timeout(Duration::from_secs(2), &mut begin).await??;
        transaction.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn every_checkout_maps_the_pool_memory_map_size() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("db");
        let unmapped = WorkspaceDatabase::open(&path, DatabaseName::Index, pool()).await?;
        let mut reading = unmapped.connection().await?;
        let unmapped_size = toasty::sql::query("PRAGMA mmap_size")
            .column_types([Type::I64])
            .exec(&mut reading)
            .await?;
        crate::lexical::require_pragma_row(&unmapped_size, &[Value::I64(0)])?;
        drop(reading);
        drop(unmapped);

        let mapped_pool = pool().memory_mapped(1 << 20);
        assert_eq!(mapped_pool.mmap_bytes(), 1 << 20);
        let database = WorkspaceDatabase::open(&path, DatabaseName::Index, mapped_pool).await?;
        let mut reading = database.connection().await?;
        let read_size = toasty::sql::query("PRAGMA mmap_size")
            .column_types([Type::I64])
            .exec(&mut reading)
            .await?;
        crate::lexical::require_pragma_row(&read_size, &[Value::I64(1 << 20)])?;
        drop(reading);
        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        let written_size = toasty::sql::query("PRAGMA mmap_size")
            .column_types([Type::I64])
            .exec(&mut transaction)
            .await?;
        crate::lexical::require_pragma_row(&written_size, &[Value::I64(1 << 20)])?;
        transaction.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn write_checkouts_wait_for_the_process_write_turn() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, pool())
                .await?;
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

    /// The entries of the `operations` list a table record carries whose `field` is `value`.
    fn listed_with(
        record: &rift_tracing::LogRecord,
        field: &str,
        value: &str,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error>> {
        let fields: serde_json::Value = serde_json::from_str(record.fields())?;
        let listed: serde_json::Value =
            serde_json::from_str(fields["operations"].as_str().ok_or("operations")?)?;
        Ok(listed
            .as_array()
            .into_iter()
            .flatten()
            .filter(|entry| entry[field] == value)
            .cloned()
            .collect())
    }

    #[tokio::test]
    async fn a_writer_waiting_for_the_write_turn_names_the_operation_that_holds_it() -> TestResult {
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, pool())
                .await?;
        let holder =
            rift_tracing::traced!("documentation.store", async { database.writing().await })
                .await?;
        let mut waiter = std::pin::pin!(rift_tracing::traced!(
            component = "lexical",
            operation = "lexical.commit",
            async { database.writing().await.map(drop) }
        ));
        let pending = tokio::select! {
            biased;
            _ = waiter.as_mut() => false,
            () = std::future::ready(()) => true,
        };
        assert!(pending, "the holder keeps the write turn");
        rift_tracing::publish_in_flight("write turn");
        drop(holder);
        waiter.await?;
        drop(recorder);

        let records = drain.queued_records();
        let table = records
            .iter()
            .find(|record| record.message() == "operations in flight")
            .ok_or("the table was published")?;
        let waits = listed_with(table, "kind", "wait")?;
        assert_eq!(waits.len(), 1, "{}", table.fields());
        assert_eq!(waits[0]["lock.name"], "index.write");
        assert_eq!(waits[0]["parent"], "lexical.commit");
        let held = listed_with(table, "kind", "held")?;
        assert_eq!(held.len(), 1, "{}", table.fields());
        assert_eq!(held[0]["parent"], "documentation.store");
        let wait = records
            .iter()
            .find(|record| record.message() == "lock.wait")
            .ok_or("the wait closed with a record")?;
        let wait: serde_json::Value = serde_json::from_str(wait.fields())?;
        assert_eq!(wait["waiter"], "lexical.commit");
        assert_eq!(wait["holder"], "documentation.store");
        assert_eq!(wait["outcome"], "acquired");
        Ok(())
    }

    #[tokio::test]
    async fn wal_read_completes_while_an_immediate_write_is_uncommitted() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Vectors, pool())
                .await?;
        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        VectorRecord::create()
            .identity("uncommitted".to_owned())
            .model("model".to_owned())
            .digest("digest".to_owned())
            .dimension(1)
            .vector(vec![0_u8; 4])
            .exec(&mut transaction)
            .await?;
        let reader_database = Arc::clone(&database);
        let reader = tokio::spawn(async move {
            let mut connection = reader_database
                .connection()
                .await
                .expect("the read connection opens");
            VectorRecord::all()
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

    /// Failure bound on one log write or read beside a held index transaction; never a
    /// way to order two events.
    const LOG_BESIDE_INDEX_MAX: Duration = Duration::from_secs(10);

    fn log_record(message: &str) -> rift_tracing::LogRecord {
        rift_tracing::LogRecord::new(
            1,
            "info",
            "rift_index::tests",
            "index",
            "index.commit",
            message,
            "{}",
        )
    }

    /// A log append and a log read on the metrics database complete while an index commit
    /// is held open on the index database's worker, after its SQL ran and before `COMMIT`.
    #[tokio::test]
    async fn a_held_index_commit_delays_no_log_write_or_read() -> TestResult {
        let directory = tempfile::tempdir()?;
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabaseName::Index, pool())
                .await?;
        let logs = rift_tracing::LogStore::open(&directory.path().join("metrics"), None).await?;
        let index =
            LexicalSearchIndex::attached(Arc::clone(&database), LexicalIndexLimits::default());
        let (started, release) = database.thread.hold_next_commit_for_test().await?;
        let documents = crash_documents(true)?;
        let commit = tokio::spawn(async move { index.replace_all(&documents, "held").await });
        tokio::time::timeout(LOG_BESIDE_INDEX_MAX, started).await??;

        tokio::time::timeout(
            LOG_BESIDE_INDEX_MAX,
            logs.append(&[log_record("beside")], 100),
        )
        .await
        .map_err(|_elapsed| "the log append waited on the held index commit")??;
        let read = logs
            .reader()
            .connect()?
            .recent(&rift_tracing::LogQuery::newest(10))?;
        assert_eq!(read.len(), 1);
        assert!(!commit.is_finished(), "the index commit is still held");

        release
            .send(())
            .map_err(|()| "the index worker dropped the hold")?;
        tokio::time::timeout(LOG_BESIDE_INDEX_MAX, commit).await???;
        Ok(())
    }

    /// Another connection's write lock on the index database delays no log write or read,
    /// while an index write waits out its own busy timeout and refuses.
    #[tokio::test]
    async fn another_write_lock_on_the_index_delays_no_log_write_or_read() -> TestResult {
        let directory = tempfile::tempdir()?;
        let index_path = directory.path().join("db");
        let database = WorkspaceDatabase::open(
            &index_path,
            DatabaseName::Index,
            DatabasePool::new(4, HELD_POOL_BUSY_TIMEOUT_MS),
        )
        .await?;
        let logs = rift_tracing::LogStore::open(&directory.path().join("metrics"), None).await?;
        let index =
            LexicalSearchIndex::attached(Arc::clone(&database), LexicalIndexLimits::default());
        let (_writer, _writing) = hold_write_lock(&index_path).await?;

        tokio::time::timeout(
            LOG_BESIDE_INDEX_MAX,
            logs.append(&[log_record("beside")], 100),
        )
        .await
        .map_err(|_elapsed| "the log append waited on the index write lock")??;
        let read = logs
            .reader()
            .connect()?
            .recent(&rift_tracing::LogQuery::newest(10))?;
        assert_eq!(read.len(), 1);
        let refused = tokio::time::timeout(
            LOG_BESIDE_INDEX_MAX,
            index.replace_all(&crash_documents(true)?, "locked"),
        )
        .await
        .map_err(|_elapsed| "the index write kept waiting past its own busy timeout")?;
        assert!(
            refused.is_err(),
            "the index write meets the held lock and refuses"
        );
        Ok(())
    }

    /// The write-ahead log limit the WAL tests set: small enough that one test write
    /// passes it many times over.
    const WAL_TEST_LIMIT_BYTES: u64 = 64 << 10;
    /// Blobs of [`WAL_TEST_LIMIT_BYTES`] one test write commits.
    const WAL_TEST_ROWS: i64 = 16;

    async fn limited_database(
        path: &Path,
        name: DatabaseName,
    ) -> Result<Arc<WorkspaceDatabase>, Box<dyn std::error::Error>> {
        let pool = DatabasePool::new(4, 1_000).journal_size_limited(WAL_TEST_LIMIT_BYTES);
        Ok(WorkspaceDatabase::open(path, name, pool).await?)
    }

    /// Commits [`WAL_TEST_ROWS`] blobs of the limit's size into a scratch table through the
    /// write turn.
    async fn write_past_the_limit(database: &WorkspaceDatabase) -> TestResult {
        let mut writing = database.writing().await?;
        let mut transaction = writing.transaction().await?;
        toasty::sql::statement("CREATE TABLE IF NOT EXISTS wal_scratch(payload BLOB)")
            .exec(&mut transaction)
            .await?;
        toasty::sql::statement(format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {WAL_TEST_ROWS}) \
             INSERT INTO wal_scratch SELECT randomblob({WAL_TEST_LIMIT_BYTES}) FROM n"
        ))
        .exec(&mut transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    fn wal_bytes(path: &Path) -> std::io::Result<Option<u64>> {
        match std::fs::metadata(super::appended(
            path,
            rift_core::constants::WRITE_AHEAD_LOG_SUFFIX,
        )) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// A close checkpoints and drops every connection, so a log written past the limit
    /// leaves no `-wal` file, for each database.
    #[tokio::test]
    async fn a_closed_database_leaves_no_write_ahead_log() -> TestResult {
        for name in [DatabaseName::Index, DatabaseName::Vectors] {
            let directory = tempfile::tempdir()?;
            let path = name.path(directory.path());
            let database = limited_database(&path, name).await?;
            write_past_the_limit(&database).await?;
            let written = wal_bytes(&path)?.ok_or("the write leaves a write-ahead log")?;
            assert!(
                written > WAL_TEST_LIMIT_BYTES,
                "the {name} write must pass the limit before the close: written={written}"
            );

            database
                .shutdown(tokio::time::Instant::now() + Duration::from_secs(5))
                .await?;

            assert_eq!(
                wal_bytes(&path)?,
                None,
                "a closed {name} database must leave no write-ahead log"
            );
        }
        Ok(())
    }

    /// With another connection still open on the file, the log outlives the close but the
    /// truncate checkpoint leaves it empty.
    #[tokio::test]
    async fn a_close_beside_another_connection_leaves_an_empty_write_ahead_log() -> TestResult {
        for name in [DatabaseName::Index, DatabaseName::Vectors] {
            let directory = tempfile::tempdir()?;
            let path = name.path(directory.path());
            let database = limited_database(&path, name).await?;
            write_past_the_limit(&database).await?;
            let other = rusqlite::Connection::open(&path)?;
            let rows: i64 =
                other.query_row("SELECT COUNT(*) FROM wal_scratch", [], |row| row.get(0))?;
            assert_eq!(
                rows, WAL_TEST_ROWS,
                "the other connection reads the committed rows"
            );

            database
                .shutdown(tokio::time::Instant::now() + Duration::from_secs(5))
                .await?;

            assert_eq!(
                wal_bytes(&path)?,
                Some(0),
                "the checkpoint must empty the {name} write-ahead log the other connection keeps"
            );
            drop(other);
        }
        Ok(())
    }

    /// Once a checkpoint restarts the log, the next commit cuts the file to the limit.
    #[tokio::test]
    async fn the_commit_that_restarts_the_log_cuts_it_to_the_limit() -> TestResult {
        for name in [DatabaseName::Index, DatabaseName::Vectors] {
            let directory = tempfile::tempdir()?;
            let path = name.path(directory.path());
            let database = limited_database(&path, name).await?;
            write_past_the_limit(&database).await?;
            let grown = wal_bytes(&path)?.ok_or("the write leaves a write-ahead log")?;
            assert!(
                grown > WAL_TEST_LIMIT_BYTES,
                "the {name} write must grow the log past the limit: grown={grown}"
            );
            let other = rusqlite::Connection::open(&path)?;
            let busy: i64 =
                other.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| row.get(0))?;
            assert_eq!(
                busy, 0,
                "the passive checkpoint must copy every {name} frame"
            );
            drop(other);

            let mut writing = database.writing().await?;
            let mut transaction = writing.transaction().await?;
            toasty::sql::statement("INSERT INTO wal_scratch VALUES (x'00')")
                .exec(&mut transaction)
                .await?;
            transaction.commit().await?;
            drop(writing);

            let restarted = wal_bytes(&path)?.ok_or("the commit keeps a write-ahead log")?;
            assert!(
                restarted <= WAL_TEST_LIMIT_BYTES,
                "the commit restarting the {name} log must cut it to the limit: \
                 restarted={restarted}, limit={WAL_TEST_LIMIT_BYTES}"
            );
            database
                .shutdown(tokio::time::Instant::now() + Duration::from_secs(5))
                .await?;
        }
        Ok(())
    }

    /// A second shutdown runs no second checkpoint and answers as the first did.
    #[tokio::test]
    async fn a_second_shutdown_answers_without_a_second_checkpoint() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = DatabaseName::Index.path(directory.path());
        let database = limited_database(&path, DatabaseName::Index).await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        database.shutdown(deadline).await?;
        database.shutdown(deadline).await?;
        Ok(())
    }

    /// Every checkout sets the pool's write-ahead log limit, and a pool without one keeps
    /// `SQLite`'s default of no limit.
    #[tokio::test]
    async fn every_checkout_sets_the_pool_journal_size_limit() -> TestResult {
        let directory = tempfile::tempdir()?;
        let limited =
            limited_database(&directory.path().join("limited"), DatabaseName::Index).await?;
        let unlimited = WorkspaceDatabase::open(
            &directory.path().join("unlimited"),
            DatabaseName::Index,
            pool(),
        )
        .await?;
        for (database, expected) in [
            (&limited, i64::try_from(WAL_TEST_LIMIT_BYTES)?),
            (&unlimited, -1),
        ] {
            let mut reading = database.connection().await?;
            let limit = toasty::sql::query("PRAGMA journal_size_limit")
                .column_types([Type::I64])
                .exec(&mut reading)
                .await?;
            crate::lexical::require_pragma_row(&limit, &[Value::I64(expected)])?;
        }
        Ok(())
    }

    /// A checkpoint row of another shape is a storage failure, not a guess.
    #[test]
    fn a_checkpoint_row_of_another_shape_is_refused() {
        let row = Value::Record(toasty_core::stmt::ValueRecord::from_vec(vec![
            Value::I64(1),
            Value::I64(4),
            Value::I64(3),
        ]));
        let checkpoint = super::WalCheckpoint::from_row(std::slice::from_ref(&row));
        assert_eq!(
            checkpoint.ok(),
            Some(super::WalCheckpoint {
                busy: true,
                log: 4,
                checkpointed: 3,
            })
        );
        assert!(super::WalCheckpoint::from_row(&[]).is_err());
    }
}
