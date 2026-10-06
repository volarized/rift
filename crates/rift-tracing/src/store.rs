//! The metrics database's writer: one thread, one connection, one command at a time.
//!
//! [`LogStore::open`] spawns the thread `rift-db-metrics`. The thread opens the one write
//! connection, keeps it for its life, and holds the serving owner it was handed until it
//! exits. `rusqlite` is synchronous, so the thread runs no async runtime: it reads its
//! commands with `blocking_recv`, and an async caller awaits each reply. One thread owns
//! the only write connection, so writers in this process never compete for the file.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread;
use std::time::Duration;

use rift_error::{RiftError, errors};
use rusqlite::config::DbConfig;
use rusqlite::{Connection, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, timeout_at};

use crate::metrics::{
    Counter, Histogram, ObservableUpDownCounter, Observation, ObservationGuard, SCOPE,
};
use crate::pages::{PAGE_STATE_FREE, PAGE_STATE_USED, PageCounts};
use crate::reads::LogReader;
use crate::record::{LOG_BATCH_RECORDS_MAX, LOG_KIND, LogRecord};

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
/// The stages one close runs through, in order: the command waits in the writer's queue,
/// then the thread reads the log's frames and closes the connection without a checkpoint.
/// The thread stage carries the operation its failure names.
const CLOSE_STAGES: [&str; 2] = ["queued", "close"];
/// [`CLOSE_STAGES`] index of the command waiting in the queue.
const CLOSE_QUEUED: usize = 0;
/// [`CLOSE_STAGES`] index of the connection's close: the read of the log's frames, then
/// the close itself.
const CLOSE_CONNECTION: usize = 1;

/// The `db.namespace` every signal of the metrics database carries.
const DB_NAMESPACE: &str = "metrics";
/// The `db.operation.name` of an append's queue wait.
const APPEND_OPERATION: &str = "append";
/// `sqlite.queue.wait.duration`: one append from its send to the writer's dequeue.
static QUEUE_WAIT: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.queue.wait.duration",
    &["db.namespace", "db.operation.name"],
);
/// `sqlite.queue.length`: commands sent to the writer and not yet received, read when the
/// meter collects.
static QUEUE_LENGTH: ObservableUpDownCounter<1> =
    ObservableUpDownCounter::declare(SCOPE, "sqlite.queue.length", "{command}", &["db.namespace"]);
/// `sqlite.write_lock.wait.duration`: one `BEGIN IMMEDIATE` of an append, with the result
/// code as `error.type` when it failed.
static WRITE_LOCK_WAIT: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.write_lock.wait.duration",
    &["db.namespace", "error.type"],
);
/// `sqlite.transaction.duration`: one append transaction from its begin to its commit or
/// rollback, by `sqlite.transaction.result`.
static TRANSACTION_DURATION: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.transaction.duration",
    &["db.namespace", "sqlite.transaction.result"],
);
/// `sqlite.commit.duration`: one append's `COMMIT`, a checkpoint it runs included.
static COMMIT_DURATION: Histogram<1> =
    Histogram::declare(SCOPE, "sqlite.commit.duration", &["db.namespace"]);
/// `sqlite.file.size`: the size of the metrics database file and of its write-ahead log,
/// read when the meter collects.
static FILE_SIZE: ObservableUpDownCounter<2> = ObservableUpDownCounter::declare(
    SCOPE,
    "sqlite.file.size",
    "By",
    &["db.namespace", "sqlite.file.type"],
);
/// `sqlite.page.count`: the pages of the metrics database file in use and on its freelist,
/// read from the file's header when the meter collects.
static PAGE_COUNT: ObservableUpDownCounter<2> = ObservableUpDownCounter::declare(
    SCOPE,
    "sqlite.page.count",
    "{page}",
    &["db.namespace", "sqlite.page.state"],
);

/// `sqlite.transaction.statement.count`: the statements one append transaction ran, its
/// begin and its end left out.
static TRANSACTION_STATEMENTS: Histogram<1, u64> = Histogram::declare_count(
    SCOPE,
    "sqlite.transaction.statement.count",
    "{statement}",
    &["db.namespace"],
    &STATEMENT_BOUNDARIES,
);
/// Upper bucket bounds of `sqlite.transaction.statement.count`, in statements, for every
/// database that records it: the metrics database, the index and vectors workers, and the
/// history store. One metrics append runs at most [`LOG_BATCH_RECORDS_MAX`] inserts and two
/// statements more.
pub const STATEMENT_BOUNDARIES: [f64; 13] = [
    1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0,
];

/// `db.client.operation.duration`: one operation on a metrics database connection, by
/// `db.operation.name`; a failed one adds `error.type` `_OTHER`, and the busy or locked
/// result code as `db.response.status_code`.
static OPERATION_DURATION: Histogram<5> = Histogram::declare(
    SCOPE,
    "db.client.operation.duration",
    &[
        "db.system.name",
        "db.namespace",
        "db.operation.name",
        "db.response.status_code",
        "error.type",
    ],
);
/// `sqlite.busy.retries`: calls of the writer connection's busy handler, each one a lock
/// another connection held when the writer asked for it.
static BUSY_RETRIES: Counter<1> =
    Counter::declare(SCOPE, "sqlite.busy.retries", "{retry}", &["db.namespace"]);
/// The sleep before each retry of one busy wait, in milliseconds, by the count of earlier
/// calls for the same lock: `sqliteDefaultBusyCallback`'s `delays` table in the bundled
/// `SQLite` 3.53.2, the handler `sqlite3_busy_timeout` installs. A call past the table
/// sleeps its last entry.
const BUSY_DELAYS_MS: [u64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
/// The sleep before each entry of [`BUSY_DELAYS_MS`], summed: the same function's `totals`.
const BUSY_TOTALS_MS: [u64; 12] = [0, 1, 3, 8, 18, 33, 53, 78, 103, 128, 178, 228];

/// How long the busy handler sleeps before retrying on its `count`-th call for one lock,
/// with `timeout_ms` the whole wait; `None` once the wait is spent, and the statement then
/// reports busy.
///
/// It is `sqliteDefaultBusyCallback`'s schedule: a registered busy handler replaces the
/// one `sqlite3_busy_timeout` installs, so the writer keeps the same total wait only by
/// sleeping the same steps, the last one cut so the sum equals `timeout_ms`.
fn busy_delay(count: i32, timeout_ms: u64) -> Option<Duration> {
    let count = usize::try_from(count).ok()?;
    let last = BUSY_DELAYS_MS.len() - 1;
    let (delay, prior) = if let (Some(delay), Some(prior)) =
        (BUSY_DELAYS_MS.get(count), BUSY_TOTALS_MS.get(count))
    {
        (*delay, *prior)
    } else {
        let past = u64::try_from(count - last).unwrap_or(u64::MAX);
        let prior = BUSY_DELAYS_MS[last]
            .saturating_mul(past)
            .saturating_add(BUSY_TOTALS_MS[last]);
        (BUSY_DELAYS_MS[last], prior)
    };
    let delay = if prior.saturating_add(delay) > timeout_ms {
        timeout_ms.checked_sub(prior).filter(|left| *left > 0)?
    } else {
        delay
    };
    Some(Duration::from_millis(delay))
}

/// The writer connection's busy handler: counts each call in `sqlite.busy.retries`, then
/// sleeps the step [`busy_delay`] gives within [`METRICS_BUSY_TIMEOUT_MS`] and asks
/// `SQLite` to retry, or, once the wait is spent, to report the database busy.
fn retry_busy(count: i32) -> bool {
    BUSY_RETRIES.labeled([DB_NAMESPACE]).add(1);
    match busy_delay(count, METRICS_BUSY_TIMEOUT_MS) {
        Some(delay) => {
            thread::sleep(delay);
            true
        }
        None => false,
    }
}

/// The `db.system.name` of the metrics database.
const DB_SYSTEM: &str = "sqlite";
/// The close statement that reads the frames the log holds: it takes no lock, moves
/// nothing, and syncs no file.
const CHECKPOINT_NOOP: &str = "PRAGMA wal_checkpoint(NOOP)";
/// The checkpoint an open runs, which moves every frame and empties the log file.
const CHECKPOINT_TRUNCATE: &str = "PRAGMA wal_checkpoint(TRUNCATE)";
/// The `db.operation.name` of the connection's close. The close runs no checkpoint: the
/// connection sets `SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE` first, so the write-ahead log stays
/// for the next open.
const CONNECTION_CLOSE: &str = "close";
/// The `db.operation.name` of an append's inserts: one point covers every insert of the batch.
const INSERT_OPERATION: &str = "insert";
/// The `db.operation.name` of an append's retention trim.
const TRIM_OPERATION: &str = "trim";

/// Runs one operation named `operation` and records its duration in
/// `db.client.operation.duration`, labeled by the `SQLite` failure `cause` finds in its
/// error. A regressed clock records nothing and keeps the operation's result.
pub(crate) fn timed<Answer, Failure>(
    operation: &'static str,
    run: impl FnOnce() -> Result<Answer, Failure>,
    cause: impl FnOnce(&Failure) -> &rusqlite::Error,
) -> Result<Answer, Failure> {
    let result;
    let took = crate::measure_elapsed!("db.client.operation", {
        result = run();
    })
    .ok()
    .map(|((), measurement)| measurement.elapsed());
    if let Some(took) = took {
        let (status, failed) = match &result {
            Ok(_) => ("", ""),
            Err(failure) => match sqlite_error_type(cause(failure)) {
                "_OTHER" => ("", "_OTHER"),
                code => (code, "_OTHER"),
            },
        };
        OPERATION_DURATION
            .labeled([DB_SYSTEM, DB_NAMESPACE, operation, status, failed])
            .record(took);
    }
    result
}

/// The `error.type` of a failed `SQLite` statement: the result code for busy, `5`, and for
/// locked, `6`, the two a writer waits on, and `_OTHER` for every other failure.
#[must_use]
pub fn sqlite_error_type(failure: &rusqlite::Error) -> &'static str {
    match failure.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy) => "5",
        Some(rusqlite::ErrorCode::DatabaseLocked) => "6",
        _ => "_OTHER",
    }
}

/// Reports the sizes of the database file at `path` and of its write-ahead log: two
/// `fs::metadata` calls, a few microseconds each. A file that does not exist reports
/// nothing.
fn observe_file_sizes(path: &Path, observation: &Observation<'_, 2>) {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    for (kind, file) in [("database", path), ("wal", Path::new(&wal))] {
        if let Ok(metadata) = std::fs::metadata(file) {
            observation.observe([DB_NAMESPACE, kind], metadata.len());
        }
    }
}

/// Reports the pages in use and on the freelist of the database file at `path`, as
/// [`PageCounts::read`] answers them; a file it answers nothing for reports nothing.
fn observe_page_counts(path: &Path, observation: &Observation<'_, 2>) {
    if let Some(counts) = PageCounts::read(path) {
        observation.observe([DB_NAMESPACE, PAGE_STATE_USED], u64::from(counts.used()));
        observation.observe([DB_NAMESPACE, PAGE_STATE_FREE], u64::from(counts.free()));
    }
}

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

/// What a close left in the write-ahead log, read with `PRAGMA wal_checkpoint(NOOP)` before
/// the connection closed: the frames the log held, the frames an earlier checkpoint had
/// already moved into the database, and how long the close ran.
///
/// The close runs no checkpoint, so the frames past `checkpointed` stay in the log and the
/// next open's checkpoint moves them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalCheckpoint {
    log: i64,
    checkpointed: i64,
    elapsed: Duration,
}

/// The three integers one `PRAGMA wal_checkpoint` row carries: `busy`, `log`, and
/// `checkpointed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CheckpointRow {
    busy: bool,
    log: i64,
    checkpointed: i64,
}

impl CheckpointRow {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            busy: row.get::<_, i64>(0)? != 0,
            log: row.get(1)?,
            checkpointed: row.get(2)?,
        })
    }
}

impl WalCheckpoint {
    /// Frames the write-ahead log held at the close.
    #[must_use]
    pub const fn log(&self) -> i64 {
        self.log
    }

    /// Frames an earlier checkpoint had already moved into the database.
    #[must_use]
    pub const fn checkpointed(&self) -> i64 {
        self.checkpointed
    }

    /// How long the close ran on the writer thread.
    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// How one close of the metrics database ended without a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreClose {
    /// The writer thread closed the connection without a checkpoint, and released its owner.
    Closed(WalCheckpoint),
    /// The deadline passed before the writer thread finished the close: while the command
    /// waited in the queue behind an earlier command, or while the thread closed the
    /// connection. The thread keeps running, and keeps its owner, until it finishes or the
    /// process exits; the log stays for the next open, which recovers every committed
    /// record from it.
    Timeout {
        /// The close stage the close was in: `queued` or `close`.
        stage: &'static str,
        /// How long the thread had been in that stage when the deadline passed.
        elapsed: Duration,
    },
}

/// Where one close stands, shared by the closer and the writer thread.
///
/// The closer starts the queued stage before it sends the command; the thread starts each
/// later stage before running it and marks the close ended after the connection closed. A
/// close that misses its deadline reads this, so its timeout names the stage the close was
/// in and how long that stage ran.
#[derive(Debug, Default)]
struct CloseProgress {
    stages: Mutex<CloseStages>,
    /// Holds the writer thread at the start of its next close: the thread answers on
    /// the sender once it holds, and resumes once the receiver fires or its sender drops.
    #[cfg(any(test, feature = "fixtures"))]
    close_hold: Mutex<Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

/// The instants one close reached, each read on the monotonic clock.
#[derive(Debug, Default)]
struct CloseStages {
    /// When each of [`CLOSE_STAGES`] started; `None` for a stage the close did not reach.
    started: [Option<std::time::Instant>; CLOSE_STAGES.len()],
    /// When the connection's close returned.
    ended: Option<std::time::Instant>,
}

impl CloseProgress {
    /// Starts a close: clears what an earlier close recorded and starts the queued stage.
    fn request(&self) {
        *self.lock() = CloseStages::default();
        self.start(CLOSE_QUEUED);
    }

    /// Records that `stage`, an index into [`CLOSE_STAGES`], started now.
    fn start(&self, stage: usize) {
        if let Some(started) = self.lock().started.get_mut(stage) {
            *started = Some(std::time::Instant::now());
        }
    }

    /// Records that the connection's close returned.
    fn end(&self) {
        self.lock().ended = Some(std::time::Instant::now());
    }

    /// The index into [`CLOSE_STAGES`] of the last stage the close started, and how long it
    /// ran by `now`, or until the connection's close returned when it returned before
    /// `now`; `None` before any stage started.
    fn running(&self, now: std::time::Instant) -> Option<(usize, Duration)> {
        let stages = self.lock();
        let until = stages.ended.unwrap_or(now);
        stages
            .started
            .iter()
            .enumerate()
            .rev()
            .find_map(|(stage, started)| started.map(|started| (stage, started)))
            .map(|(stage, started)| (stage, until.saturating_duration_since(started)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CloseStages> {
        self.stages.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Arms a hold of the next close: the receiver answers once the writer thread
    /// holds, and the thread resumes once the sender fires or drops.
    #[cfg(any(test, feature = "fixtures"))]
    fn hold_next_close(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (holding, held) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        *self
            .close_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((holding, released));
        (held, release)
    }

    /// Runs an armed hold on the writer thread; returns at once when none is armed.
    #[cfg(any(test, feature = "fixtures"))]
    fn close_held(&self) {
        let hold = self
            .close_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((holding, released)) = hold {
            let _ = holding.send(());
            let _ = released.recv();
        }
    }
}

/// One command the writer thread runs.
enum Command {
    /// Append one batch and trim back to `retention_records`.
    Append {
        records: Vec<LogRecord>,
        retention_records: u64,
        /// When the caller started to send it.
        queued: std::time::Instant,
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
    closed: OnceLock<StoreClose>,
    progress: Arc<CloseProgress>,
    /// Keeps the queue length, the file sizes, and the page counts reported while the
    /// store lives; absent where the process installed no meter.
    _readings: [Option<ObservationGuard>; 3],
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
        let progress = Arc::new(CloseProgress::default());
        let thread_progress = Arc::clone(&progress);
        thread::Builder::new()
            .name(WRITER_THREAD_NAME.to_owned())
            .spawn(move || {
                MetricsWriter::run(&thread_path, owner, receiver, ready, &thread_progress);
            })
            .map_err(|source| store_failure("start the writer thread", path, source))?;
        answer.await.map_err(|_| {
            store_failure(
                "open",
                path,
                std::io::Error::other("the writer thread stopped before it answered"),
            )
        })??;
        let queue = sender.downgrade();
        let sizes: PathBuf = database.to_path_buf();
        let pages: PathBuf = database.to_path_buf();
        // The reads hold the queue weakly and copy the path, so none keeps the writer.
        let readings = [
            QUEUE_LENGTH.observe(move |observation| {
                if let Some(sender) = queue.upgrade() {
                    let queued = sender.max_capacity().saturating_sub(sender.capacity());
                    observation.observe([DB_NAMESPACE], u64::try_from(queued).unwrap_or(u64::MAX));
                }
            }),
            FILE_SIZE.observe(move |observation| observe_file_sizes(&sizes, observation)),
            PAGE_COUNT.observe(move |observation| observe_page_counts(&pages, observation)),
        ];
        Ok(Self {
            path: database,
            sender,
            closed: OnceLock::new(),
            progress,
            _readings: readings,
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
            queued: std::time::Instant::now(),
            reply,
        };
        self.request(command, answer, "append").await
    }

    /// Closes the connection without a checkpoint and releases the owner, all by
    /// `deadline`.
    ///
    /// The write-ahead log stays with every committed record, and the next open's checkpoint
    /// moves it into the database. A received answer means the owner is released. A thread that misses `deadline` keeps running and keeps its
    /// owner until it finishes. A second close answers what the first close answered.
    ///
    /// A deadline that passes before the thread answers, whatever stage the close is in,
    /// answers [`StoreClose::Timeout`] naming that stage: every committed transaction is in the
    /// log, and the next open recovers whatever the log still holds.
    /// A close still waiting in the queue behind an earlier command runs after it; one the
    /// full queue never took leaves the thread to close once every handle drops.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when the thread already stopped, or `SQLite`
    /// refuses the close.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future after the command is queued leaves the thread to close.
    pub async fn close(&self, deadline: Instant) -> Result<StoreClose, RiftError> {
        if let Some(closed) = self.closed.get() {
            return Ok(*closed);
        }
        let (reply, answer) = oneshot::channel();
        self.progress.request();
        let answered = timeout_at(
            deadline,
            self.request(Command::Close { reply }, answer, "close"),
        )
        .await;
        let closed = match answered {
            Ok(checkpoint) => StoreClose::Closed(checkpoint?),
            Err(_elapsed) => {
                // `request` started the queued stage before the command was sent, so a
                // close always has a stage to name.
                let (stage, elapsed) = self
                    .progress
                    .running(std::time::Instant::now())
                    .unwrap_or((CLOSE_QUEUED, Duration::ZERO));
                StoreClose::Timeout {
                    stage: CLOSE_STAGES[stage],
                    elapsed,
                }
            }
        };
        Ok(*self.closed.get_or_init(|| closed))
    }

    /// Holds the writer thread at the start of its next close, as a test holds a close
    /// that outlasts its deadline: the receiver answers once the thread holds,
    /// and the thread resumes once the sender fires or drops.
    #[cfg(any(test, feature = "fixtures"))]
    #[doc(hidden)]
    #[must_use]
    pub fn hold_next_close(&self) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        self.progress.hold_next_close()
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
        progress: &CloseProgress,
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
                    queued,
                    reply,
                } => {
                    QUEUE_WAIT
                        .labeled([DB_NAMESPACE, APPEND_OPERATION])
                        .record(queued.elapsed());
                    let _ = reply.send(writer.append(&records, retention_records));
                }
                Command::Close { reply } => {
                    let closed = writer.close(progress);
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
            .busy_handler(Some(retry_busy))
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
        // A stop closes without a checkpoint, so the log the last process left is moved
        // into the database here, where no stop deadline runs.
        // The writer thread writes no record of its own, so the checkpoint is recorded as
        // its `db.client.operation.duration` point alone.
        timed(
            CHECKPOINT_TRUNCATE,
            || connection.query_row(CHECKPOINT_TRUNCATE, [], CheckpointRow::read),
            |source| source,
        )
        .map_err(|source| failure("checkpoint the write-ahead log", source))?;
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
        let started = std::time::Instant::now();
        let begun = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate);
        let failed = begun.as_ref().err().map_or("", sqlite_error_type);
        WRITE_LOCK_WAIT
            .labeled([DB_NAMESPACE, failed])
            .record(started.elapsed());
        let transaction = begun.map_err(|source| failure("begin append", source))?;
        // A transaction dropped on an early return rolls back.
        let mut ended = TransactionEnd {
            begun: std::time::Instant::now(),
            result: "rollback",
            statements: 0,
        };
        ended.statements += 1;
        let newest: i64 = transaction
            .query_row("SELECT COALESCE(MAX(id), 0) FROM log_records", [], |row| {
                row.get(0)
            })
            .map_err(|source| failure("read the newest identity", source))?;
        let last = timed(
            INSERT_OPERATION,
            || {
                let mut insert = transaction
                    .prepare_cached(INSERT_RECORD)
                    .map_err(|source| ("prepare append", source))?;
                let mut last = newest;
                for record in records {
                    last = last.saturating_add(1);
                    ended.statements += 1;
                    insert
                        .execute(params![
                            last,
                            LOG_KIND,
                            record.recorded_at_ms,
                            record.level,
                            record.target,
                            record.component,
                            record.operation,
                            record.message,
                            record.fields,
                        ])
                        .map_err(|source| ("insert record", source))?;
                }
                Ok(last)
            },
            |(_, source)| source,
        )
        .map_err(|(operation, source)| failure(operation, source))?;
        let retained = i64::try_from(retention_records).unwrap_or(i64::MAX);
        ended.statements += 1;
        let dropped = timed(
            TRIM_OPERATION,
            || {
                transaction
                    .prepare_cached(TRIM_RECORDS)
                    .and_then(|mut trim| trim.execute([last.saturating_sub(retained)]))
            },
            |source| source,
        )
        .map_err(|source| failure("trim records", source))?;
        let commit = std::time::Instant::now();
        let committed = transaction.commit();
        COMMIT_DURATION
            .labeled([DB_NAMESPACE])
            .record(commit.elapsed());
        committed.map_err(|source| failure("commit append", source))?;
        ended.result = "commit";
        Ok(dropped as u64)
    }

    /// Closes the connection without a checkpoint, then answers what the log held.
    ///
    /// `SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE` keeps the close from running the checkpoint the
    /// last connection's close runs otherwise (`sqlite3PagerClose` passes no buffer to
    /// `sqlite3WalClose` when the flag is set), so the close syncs no file and a stop never
    /// waits on a flush; the next open's checkpoint moves what the log holds. Starts each
    /// stage in `progress` before running it.
    fn close(self, progress: &CloseProgress) -> Result<WalCheckpoint, RiftError> {
        let Self {
            connection, path, ..
        } = self;
        let failure =
            |operation: &str, source: rusqlite::Error| store_failure(operation, &path, source);
        progress.start(CLOSE_CONNECTION);
        #[cfg(any(test, feature = "fixtures"))]
        progress.close_held();
        let started = std::time::Instant::now();
        let frames = timed(
            CHECKPOINT_NOOP,
            || connection.query_row(CHECKPOINT_NOOP, [], CheckpointRow::read),
            |source| source,
        )
        .map_err(|source| failure(CLOSE_STAGES[CLOSE_CONNECTION], source))?;
        connection
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .map_err(|source| failure(CLOSE_STAGES[CLOSE_CONNECTION], source))?;
        // A refused close hands the connection back; dropping it closes it again.
        timed(
            CONNECTION_CLOSE,
            || connection.close().map_err(|(_connection, source)| source),
            |source| source,
        )
        .map_err(|source| failure(CLOSE_STAGES[CLOSE_CONNECTION], source))?;
        progress.end();
        Ok(WalCheckpoint {
            log: frames.log,
            checkpointed: frames.checkpointed,
            elapsed: started.elapsed(),
        })
    }
}

/// Records `sqlite.transaction.duration` for one append transaction when it drops, under
/// the `sqlite.transaction.result` it ended with, and the statements it ran in
/// `sqlite.transaction.statement.count`.
struct TransactionEnd {
    begun: std::time::Instant,
    result: &'static str,
    /// Statements started inside the transaction, a refused one included.
    statements: u64,
}

impl Drop for TransactionEnd {
    fn drop(&mut self) {
        TRANSACTION_DURATION
            .labeled([DB_NAMESPACE, self.result])
            .record(self.begun.elapsed());
        TRANSACTION_STATEMENTS
            .labeled([DB_NAMESPACE])
            .record(self.statements);
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
