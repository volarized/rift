//! The metrics database's writer: one thread, one connection, one command at a time.
//!
//! [`LogStore::open`] spawns the thread `rift-db-metrics`. The thread opens the one write
//! connection, keeps it for its life, and holds the serving owner it was handed until it
//! exits. `rusqlite` is synchronous, so the thread runs no async runtime: it reads its
//! commands with `blocking_recv`, and an async caller awaits each reply. One thread owns
//! the only write connection, so writers in this process never compete for the file.

use std::error::Error;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use rift_error::{RiftError, errors};
use rusqlite::{Connection, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout_at};

use crate::reads::LogReader;
use crate::record::{LOG_BATCH_RECORDS_MAX, LogRecord};

/// The schema version `.rift/metrics` carries in `PRAGMA user_version`.
///
/// The file keeps one name across releases, so the version lives in the file. A writer
/// that finds another version drops `log_records` and creates it again; a reader that
/// finds one refuses.
pub const METRICS_SCHEMA_VERSION: i64 = 1;
/// How long one metrics connection waits for another's lock before `SQLite` reports the
/// database busy. One wait fits inside the settle wait of a `rift://logs` read, and it
/// is compiled rather than configured, so a log read never inherits a search wait.
pub const METRICS_BUSY_TIMEOUT_MS: u64 = 1_000;
/// [`METRICS_BUSY_TIMEOUT_MS`] as the duration `rusqlite` takes.
pub(crate) const METRICS_BUSY_TIMEOUT: Duration = Duration::from_millis(METRICS_BUSY_TIMEOUT_MS);
/// Bytes the write-ahead log keeps after the commit that restarts it. Rift owns the size
/// budget of the metrics database, so the bound is compiled.
const METRICS_JOURNAL_SIZE_LIMIT_BYTES: i64 = 4 << 20;
/// The writer thread's name. Linux keeps 15 bytes of a thread name; this one fits.
const WRITER_THREAD_NAME: &str = "rift-db-metrics";
/// Commands the writer's queue holds while the thread runs one.
const WRITER_QUEUE_COMMANDS: usize = 1;
/// The `kind` every row this store writes carries.
const LOG_RECORD_KIND: &str = "log";

/// The metrics database's tables, created when the file holds no schema version.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS log_records(
         id INTEGER PRIMARY KEY,
         kind TEXT NOT NULL,
         recorded_at INTEGER NOT NULL,
         level TEXT NOT NULL,
         target TEXT NOT NULL,
         component TEXT NOT NULL,
         operation TEXT NOT NULL,
         message TEXT NOT NULL,
         fields TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS log_records_level ON log_records(level);
     CREATE INDEX IF NOT EXISTS log_records_component ON log_records(component);";

/// One append's insert, every value bound.
const INSERT_RECORD: &str = "INSERT INTO log_records
     (id, kind, recorded_at, level, target, component, operation, message, fields)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";

/// The retention trim: every row at or below the identity the newest kept row follows.
/// Identities are minted in order and trimmed from the oldest, so the newest
/// `retention_records` rows are exactly those above it.
const TRIM_RECORDS: &str = "DELETE FROM log_records WHERE id <= ?1";

/// The row a truncate checkpoint answers: whether it met a busy lock, the frames the
/// write-ahead log held, and the frames it moved into the database.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalCheckpoint {
    busy: bool,
    log: i64,
    checkpointed: i64,
}

impl WalCheckpoint {
    /// Whether the checkpoint met another connection's lock and stopped short.
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.busy
    }

    /// Frames the write-ahead log held when the checkpoint ran.
    #[must_use]
    pub const fn log(&self) -> i64 {
        self.log
    }

    /// Frames the checkpoint moved into the database.
    #[must_use]
    pub const fn checkpointed(&self) -> i64 {
        self.checkpointed
    }
}

/// One command the writer thread runs.
enum Command {
    /// Append one batch and trim back to `retention_records`.
    Append {
        records: Vec<LogRecord>,
        retention_records: u64,
        reply: oneshot::Sender<Result<u64, RiftError>>,
    },
    /// Checkpoint, close the connection, release the owner, then answer.
    Close {
        reply: oneshot::Sender<Result<WalCheckpoint, RiftError>>,
    },
}

/// The writing end of the metrics database: a handle on its writer thread.
///
/// Dropping every handle closes the queue, and the thread then closes its connection and
/// releases its owner on its own.
#[derive(Debug)]
pub struct LogStore {
    path: Arc<Path>,
    sender: mpsc::Sender<Command>,
    closed: OnceLock<WalCheckpoint>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Append { records, .. } => formatter
                .debug_struct("Append")
                .field("records", &records.len())
                .finish_non_exhaustive(),
            Self::Close { .. } => formatter.debug_struct("Close").finish_non_exhaustive(),
        }
    }
}

impl LogStore {
    /// Opens the metrics database at `path` on a writer thread of its own.
    ///
    /// The thread creates the file when it is absent, switches it to WAL, and prepares its
    /// schema before it answers. `owner` stays with the thread from its entry until it
    /// exits, so whatever it guards is released only after the last write.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when the thread cannot start or `SQLite` refuses
    /// the file, the WAL switch, or the schema.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the thread started leaves the thread to finish its open
    /// and then end on the closed queue, releasing `owner`.
    pub async fn open(path: &Path, owner: Option<Arc<dyn Send + Sync>>) -> Result<Self, RiftError> {
        let database: Arc<Path> = Arc::from(path);
        let (sender, receiver) = mpsc::channel(WRITER_QUEUE_COMMANDS);
        let (ready, answer) = oneshot::channel();
        let thread_path = Arc::clone(&database);
        thread::Builder::new()
            .name(WRITER_THREAD_NAME.to_owned())
            .spawn(move || MetricsWriter::run(&thread_path, owner, receiver, ready))
            .map_err(|source| store_failure("start the writer thread", path, source))?;
        answer.await.map_err(|_| {
            store_failure(
                "open",
                path,
                std::io::Error::other("the writer thread stopped before it answered"),
            )
        })??;
        Ok(Self {
            path: database,
            sender,
            closed: OnceLock::new(),
        })
    }

    /// The metrics database file this store writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A reader of the same file, opening a connection of its own per read.
    #[must_use]
    pub fn reader(&self) -> LogReader {
        LogReader::new(&self.path)
    }

    /// Appends one batch and trims the store back to `retention_records`.
    ///
    /// Identities ascend from the highest the store already holds, assigned inside the
    /// transaction that inserts. A batch longer than [`LOG_BATCH_RECORDS_MAX`] is refused
    /// whole rather than half written. A retention of zero leaves the store empty.
    ///
    /// Returns the number of rows the trim dropped.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_batch_limit` for an oversized batch, and
    /// `tracing.log_store_failed` when the writer thread has stopped or `SQLite` refuses
    /// the write, which then writes nothing.
    ///
    /// # Cancel safety
    ///
    /// The append and its trim run in one transaction on the writer thread. Dropping the
    /// future before the batch is queued writes nothing; once queued, the thread commits
    /// it whether or not the caller still waits.
    pub async fn append(
        &self,
        records: &[LogRecord],
        retention_records: u64,
    ) -> Result<u64, RiftError> {
        if records.is_empty() {
            return Ok(0);
        }
        if records.len() > LOG_BATCH_RECORDS_MAX {
            return errors::tracing::log_batch_limit()
                .observed(records.len() as u64)
                .maximum(LOG_BATCH_RECORDS_MAX as u64)
                .fail();
        }
        let (reply, answer) = oneshot::channel();
        let command = Command::Append {
            records: records.to_vec(),
            retention_records,
            reply,
        };
        self.request(command, answer, "append").await
    }

    /// Checkpoints the write-ahead log, closes the connection, and releases the owner,
    /// all by `deadline`.
    ///
    /// The checkpoint truncates the WAL file to zero bytes unless another connection, such
    /// as a `rift server logs --follow` reader, holds it busy. A received answer means the
    /// owner is released. A thread that misses `deadline` keeps running and keeps its
    /// owner until it finishes. A second close answers the first close's checkpoint.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when the deadline passes, the thread already
    /// stopped, or `SQLite` refuses the checkpoint or the close.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the command is queued leaves the thread to close.
    pub async fn close(&self, deadline: Instant) -> Result<WalCheckpoint, RiftError> {
        if let Some(checkpoint) = self.closed.get() {
            return Ok(*checkpoint);
        }
        let (reply, answer) = oneshot::channel();
        let checkpoint = timeout_at(
            deadline,
            self.request(Command::Close { reply }, answer, "close"),
        )
        .await
        .map_err(|_| {
            store_failure(
                "close",
                &self.path,
                std::io::Error::other("the writer thread outlasted the close deadline"),
            )
        })??;
        Ok(*self.closed.get_or_init(|| checkpoint))
    }

    /// Queues `command` and awaits the thread's answer to it.
    async fn request<Answer>(
        &self,
        command: Command,
        answer: oneshot::Receiver<Result<Answer, RiftError>>,
        operation: &str,
    ) -> Result<Answer, RiftError> {
        let stopped = || {
            store_failure(
                operation,
                &self.path,
                std::io::Error::other("the writer thread has stopped"),
            )
        };
        self.sender.send(command).await.map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }
}

/// The writer thread's state: the one write connection and the file it writes.
struct MetricsWriter {
    connection: Connection,
    path: Arc<Path>,
}

impl MetricsWriter {
    /// The thread's whole life: open, answer `ready`, run commands until the queue closes
    /// or a close arrives. `owner` is released last, after the connection closed.
    fn run(
        path: &Arc<Path>,
        owner: Option<Arc<dyn Send + Sync>>,
        mut receiver: mpsc::Receiver<Command>,
        ready: oneshot::Sender<Result<(), RiftError>>,
    ) {
        let mut writer = match Self::open(path) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        if ready.send(Ok(())).is_err() {
            return;
        }
        while let Some(command) = receiver.blocking_recv() {
            match command {
                Command::Append {
                    records,
                    retention_records,
                    reply,
                } => {
                    let _ = reply.send(writer.append(&records, retention_records));
                }
                Command::Close { reply } => {
                    let closed = writer.close();
                    drop(owner);
                    let _ = reply.send(closed);
                    return;
                }
            }
        }
    }

    /// Opens the write connection, sets its PRAGMAs once, and prepares the schema.
    fn open(path: &Arc<Path>) -> Result<Self, RiftError> {
        let failure =
            |operation: &str, source: rusqlite::Error| store_failure(operation, path, source);
        let mut connection = Connection::open(path).map_err(|source| failure("open", source))?;
        connection
            .busy_timeout(METRICS_BUSY_TIMEOUT)
            .map_err(|source| failure("set the busy timeout", source))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|source| failure("enter WAL mode", source))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|source| failure("set synchronous mode", source))?;
        connection
            .pragma_update(None, "journal_size_limit", METRICS_JOURNAL_SIZE_LIMIT_BYTES)
            .map_err(|source| failure("set the WAL size limit", source))?;
        prepare_schema(&mut connection, path)?;
        Ok(Self {
            connection,
            path: Arc::clone(path),
        })
    }

    /// Inserts `records` above the highest identity held, trims, and commits, all in one
    /// immediate transaction. Answers the count the trim dropped.
    fn append(&mut self, records: &[LogRecord], retention_records: u64) -> Result<u64, RiftError> {
        let path = Arc::clone(&self.path);
        let failure =
            |operation: &str, source: rusqlite::Error| store_failure(operation, &path, source);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| failure("begin append", source))?;
        let newest: i64 = transaction
            .query_row("SELECT COALESCE(MAX(id), 0) FROM log_records", [], |row| {
                row.get(0)
            })
            .map_err(|source| failure("read the newest identity", source))?;
        let mut last = newest;
        {
            let mut insert = transaction
                .prepare_cached(INSERT_RECORD)
                .map_err(|source| failure("prepare append", source))?;
            for record in records {
                last = last.saturating_add(1);
                insert
                    .execute(params![
                        last,
                        LOG_RECORD_KIND,
                        record.recorded_at_ms,
                        record.level,
                        record.target,
                        record.component,
                        record.operation,
                        record.message,
                        record.fields,
                    ])
                    .map_err(|source| failure("insert record", source))?;
            }
        }
        let retained = i64::try_from(retention_records).unwrap_or(i64::MAX);
        let dropped = transaction
            .prepare_cached(TRIM_RECORDS)
            .and_then(|mut trim| trim.execute([last.saturating_sub(retained)]))
            .map_err(|source| failure("trim records", source))?;
        transaction
            .commit()
            .map_err(|source| failure("commit append", source))?;
        Ok(dropped as u64)
    }

    /// Truncates the write-ahead log without waiting on another connection's lock, then
    /// closes the connection. No transaction is open: the thread runs one command at a time.
    fn close(self) -> Result<WalCheckpoint, RiftError> {
        let Self { connection, path } = self;
        let failure =
            |operation: &str, source: rusqlite::Error| store_failure(operation, &path, source);
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(|source| failure("clear the busy timeout", source))?;
        let checkpoint = connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok(WalCheckpoint {
                    busy: row.get::<_, i64>(0)? != 0,
                    log: row.get(1)?,
                    checkpointed: row.get(2)?,
                })
            })
            .map_err(|source| failure("checkpoint", source))?;
        connection
            .close()
            .map_err(|(_connection, source)| failure("close", source))?;
        Ok(checkpoint)
    }
}

/// Brings the file's schema to [`METRICS_SCHEMA_VERSION`] in one immediate transaction.
///
/// Version zero is a file with no schema yet: the tables are created. Any other version
/// is a schema this build does not write: `log_records` is dropped and created again,
/// discarding what it held.
fn prepare_schema(connection: &mut Connection, path: &Path) -> Result<(), RiftError> {
    let failure = |operation: &str, source: rusqlite::Error| store_failure(operation, path, source);
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| failure("begin schema", source))?;
    let found: i64 = transaction
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|source| failure("read the schema version", source))?;
    if found != METRICS_SCHEMA_VERSION {
        if found != 0 {
            transaction
                .execute_batch("DROP TABLE IF EXISTS log_records")
                .map_err(|source| failure("drop a schema of another version", source))?;
        }
        transaction
            .execute_batch(SCHEMA)
            .map_err(|source| failure("create the schema", source))?;
        transaction
            .pragma_update(None, "user_version", METRICS_SCHEMA_VERSION)
            .map_err(|source| failure("set the schema version", source))?;
    }
    transaction
        .commit()
        .map_err(|source| failure("commit schema", source))
}

/// One metrics database failure, naming the operation, the file, and `SQLite`'s own text.
pub(crate) fn store_failure(
    operation: &str,
    path: &Path,
    source: impl Into<Box<dyn Error + Send + Sync + 'static>>,
) -> RiftError {
    errors::tracing::log_store_failed()
        .operation(operation)
        .path(path)
        .detail(source)
        .error()
}

#[cfg(test)]
mod tests;
