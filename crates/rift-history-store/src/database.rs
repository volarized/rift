//! The store's `SQLite` tables: the filler's writes and the readers' queries.
//!
//! Every statement is prepared once per connection (`prepare_cached`), and
//! each write batch commits in one `BEGIN IMMEDIATE` transaction, so the
//! write lock is held for one batch at most and parsing never runs inside it.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rift_error::{RiftError, errors};
use rift_protocol::read::{CommitAuthor, SymbolVersionKind};
use rift_tracing::{Held, Histogram, ObservableUpDownCounter, Observation, ObservationGuard};
use rusqlite::{
    CachedStatement, Connection, OptionalExtension as _, Transaction, TransactionBehavior, params,
};

use crate::record::CommitRecord;

/// How long one connection waits for another's write lock before `SQLite`
/// reports the database busy.
const BUSY_TIMEOUT: Duration = Duration::from_millis(1_000);

/// The store's tables. A schema change ships in a new binary, whose
/// derivation revision names a new store file, so no migration ever runs.
fn schema() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS commits(
             row INTEGER PRIMARY KEY,
             id TEXT NOT NULL UNIQUE,
             base TEXT,
             boundary INTEGER NOT NULL,
             author_name TEXT NOT NULL,
             author_email TEXT NOT NULL,
             committed_at TEXT NOT NULL,
             time INTEGER NOT NULL,
             message TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS changed_paths(
             commit_row INTEGER NOT NULL,
             path TEXT NOT NULL,
             old_blob TEXT,
             new_blob TEXT
         );
         CREATE INDEX IF NOT EXISTS changed_paths_commit ON changed_paths(commit_row);
         CREATE INDEX IF NOT EXISTS changed_paths_path ON changed_paths(path);
         CREATE TABLE IF NOT EXISTS renamed_paths(
             commit_row INTEGER NOT NULL,
             old_path TEXT NOT NULL,
             new_path TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS renamed_paths_commit ON renamed_paths(commit_row, new_path);
         CREATE TABLE IF NOT EXISTS moved_declarations(
             commit_row INTEGER NOT NULL,
             new_path TEXT NOT NULL,
             qualified_name TEXT NOT NULL,
             old_path TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS moved_declarations_commit
             ON moved_declarations(commit_row, new_path, qualified_name);
         CREATE TABLE IF NOT EXISTS changed_declarations(
             commit_row INTEGER NOT NULL,
             path TEXT NOT NULL,
             qualified_name TEXT NOT NULL,
             change TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS changed_declarations_commit
             ON changed_declarations(commit_row, path, qualified_name);
         CREATE INDEX IF NOT EXISTS changed_declarations_name
             ON changed_declarations(qualified_name);
         CREATE VIRTUAL TABLE IF NOT EXISTS commit_text USING fts5(
             message, content='commits', content_rowid='row', tokenize='{tokenizer}'
         );",
        tokenizer = rift_ranking::CORPUS_TOKENIZER,
    )
}

/// The tables whose rows belong to one commit row.
const COMMIT_CHILD_TABLES: [&str; 4] = [
    "changed_paths",
    "renamed_paths",
    "moved_declarations",
    "changed_declarations",
];

/// Opens one connection to `database` in WAL mode, where a writer never
/// blocks a reader. The open is one `db.client.operation.duration` point.
fn connect(database: &Path) -> Result<Connection, RiftError> {
    timed(CONNECT_OPERATION, || open_connection(database))
}

/// The open [`connect`] times: the file, its busy timeout, WAL mode, and synchronous mode.
fn open_connection(database: &Path) -> Result<Connection, RiftError> {
    let connection = Connection::open(database).map_err(|source| {
        errors::history_store::database()
            .operation("open store")
            .detail(source)
            .error()
    })?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(|source| {
        errors::history_store::database()
            .operation("set busy timeout")
            .detail(source)
            .error()
    })?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|source| {
            errors::history_store::database()
                .operation("enter WAL mode")
                .detail(source)
                .error()
        })?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|source| {
            errors::history_store::database()
                .operation("set synchronous mode")
                .detail(source)
                .error()
        })?;
    Ok(connection)
}

/// Creates the store's tables when the file lacks them.
pub(crate) fn create_schema(database: &Path) -> Result<(), RiftError> {
    connect(database)?
        .execute_batch(&schema())
        .map_err(|source| {
            errors::history_store::database()
                .operation("create store tables")
                .detail(source)
                .error()
        })
}

/// What the store holds for one commit: what it was compared with, and
/// whether history past it is out of reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldCommit {
    /// The commit it was compared with.
    pub base: Option<String>,
    /// Whether history past it is out of the store's reach.
    pub boundary: bool,
}

/// The one writer of a store: the fill lock and a write connection.
#[derive(Debug)]
pub struct StoreFiller {
    connection: Connection,
    _fill: Held<File>,
    /// Keeps the store's file sizes reported while the filler lives; absent where the
    /// process installed no meter.
    _sizes: Option<ObservationGuard>,
}

impl StoreFiller {
    /// Opens the write connection while `fill` holds the fill lock, and reports the sizes
    /// of the store's database file and write-ahead log in `sqlite.file.size` until the
    /// filler drops.
    pub(crate) fn open(database: &Path, fill: Held<File>) -> Result<Self, RiftError> {
        let connection = connect(database)?;
        let sizes = database.to_path_buf();
        Ok(Self {
            connection,
            _fill: fill,
            _sizes: FILE_SIZE.observe(move |observation| observe_file_sizes(&sizes, observation)),
        })
    }

    /// The write connection, so a test reaches the tables directly.
    #[cfg(test)]
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Every commit the store holds, keyed by commit id.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn held(&self) -> Result<HashMap<String, HeldCommit>, RiftError> {
        held_commits(&self.connection)
    }

    /// Writes one batch in one `BEGIN IMMEDIATE` transaction. A commit the
    /// store already holds under another base is replaced whole.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses a statement; the batch then
    /// writes nothing.
    pub fn write_batch(&mut self, batch: &[CommitRecord]) -> Result<(), RiftError> {
        in_write_transaction(
            &mut self.connection,
            INSERT_OPERATION,
            ["begin batch", "commit batch"],
            |transaction| {
                for commit in batch {
                    let held: Option<i64> = transaction
                        .prepare("SELECT row FROM commits WHERE id = ?1")
                        .and_then(|mut statement| {
                            statement
                                .query_row([&commit.id], |row| row.get(0))
                                .optional()
                        })
                        .map_err(|source| {
                            errors::history_store::database()
                                .operation("read held commit")
                                .detail(source)
                                .error()
                        })?;
                    if let Some(row) = held {
                        delete_commit(transaction, row)?;
                    }
                    write_commit(transaction, commit)?;
                }
                Ok(())
            },
        )
    }

    /// Deletes every commit outside `keep`, index entries first. Returns how
    /// many commits went.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses a statement; the trim then
    /// deletes nothing.
    pub fn trim(&mut self, keep: &BTreeSet<String>) -> Result<usize, RiftError> {
        in_write_transaction(
            &mut self.connection,
            TRIM_OPERATION,
            ["begin trim", "commit trim"],
            |transaction| {
                let held: Vec<(i64, String)> = transaction
                    .prepare("SELECT row, id FROM commits")
                    .and_then(|mut statement| {
                        statement
                            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                            .collect()
                    })
                    .map_err(|source| {
                        errors::history_store::database()
                            .operation("read held commits")
                            .detail(source)
                            .error()
                    })?;
                let mut deleted = 0_usize;
                for (row, _) in held.iter().filter(|(_, id)| !keep.contains(id)) {
                    delete_commit(transaction, *row)?;
                    deleted += 1;
                }
                Ok(deleted)
            },
        )
    }

    /// Checks the message index against the commit rows it names: `SQLite`'s
    /// full-text `integrity-check` with `rank` 1 compares the two.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the index and the commit rows disagree.
    pub fn check_message_index(&self) -> Result<(), RiftError> {
        self.connection
            .execute(
                "INSERT INTO commit_text(commit_text, rank) VALUES('integrity-check', 1)",
                [],
            )
            .map(|_| ())
            .map_err(|source| {
                errors::history_store::database()
                    .operation("check message index")
                    .detail(source)
                    .error()
            })
    }
}

/// The store's name as every signal of its database carries it, `db.namespace`.
const DB_NAMESPACE: &str = "history";
/// `sqlite.write_lock.wait.duration`: how long `BEGIN IMMEDIATE` waited for the write
/// lock, with the result code as `error.type` when it failed.
static WRITE_LOCK_WAIT: Histogram<2> = Histogram::declare(
    "sqlite.write_lock.wait.duration",
    &["db.namespace", "error.type"],
);
/// `sqlite.transaction.duration`: one write transaction from its begin to its commit or
/// rollback, by `sqlite.transaction.result`.
static TRANSACTION_DURATION: Histogram<2> = Histogram::declare(
    "sqlite.transaction.duration",
    &["db.namespace", "sqlite.transaction.result"],
);
/// `sqlite.commit.duration`: one `COMMIT`, a checkpoint it runs included.
static COMMIT_DURATION: Histogram<1> =
    Histogram::declare("sqlite.commit.duration", &["db.namespace"]);
/// `sqlite.transaction.statement.count`: the statements one write transaction ran, its
/// begin and its end left out.
static TRANSACTION_STATEMENTS: Histogram<1, u64> = Histogram::declare_count(
    "sqlite.transaction.statement.count",
    "{statement}",
    &["db.namespace"],
    &STATEMENT_BOUNDARIES,
);
/// Upper bucket bounds of [`TRANSACTION_STATEMENTS`], in statements: the bounds the index
/// and vectors workers use.
const STATEMENT_BOUNDARIES: [f64; 13] = [
    1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0,
];
/// `db.client.operation.duration`: one operation on a store connection, by
/// `db.operation.name`; a failed one adds `error.type` `_OTHER`, and the busy or locked
/// result code as `db.response.status_code`.
static OPERATION_DURATION: Histogram<5> = Histogram::declare(
    "db.client.operation.duration",
    &[
        "db.system.name",
        "db.namespace",
        "db.operation.name",
        "db.response.status_code",
        "error.type",
    ],
);
/// The `db.system.name` of the store.
const DB_SYSTEM: &str = "sqlite";
/// The `db.operation.name` of one connection's open.
const CONNECT_OPERATION: &str = "connect";
/// The `db.operation.name` of one batch's writes, inside its transaction.
const INSERT_OPERATION: &str = "insert";
/// The `db.operation.name` of one trim's deletes, inside its transaction.
const TRIM_OPERATION: &str = "trim";
/// The `db.operation.name` of one read connection's query.
const QUERY_OPERATION: &str = "query";
/// `sqlite.file.size`: the size of the store's database file and of its write-ahead log,
/// read when the meter collects.
static FILE_SIZE: ObservableUpDownCounter<2> = ObservableUpDownCounter::declare(
    "sqlite.file.size",
    "By",
    &["db.namespace", "sqlite.file.type"],
);

/// Runs one operation named `operation` and records its duration in
/// `db.client.operation.duration`, labeled by the `SQLite` failure the refusal's source
/// chain carries. A regressed clock records nothing and keeps the operation's result.
fn timed<Answer>(
    operation: &'static str,
    run: impl FnOnce() -> Result<Answer, RiftError>,
) -> Result<Answer, RiftError> {
    let result;
    let took = rift_tracing::measure_elapsed!("db.client.operation", {
        result = run();
    })
    .ok()
    .map(|((), measurement)| measurement.elapsed());
    if let Some(took) = took {
        let (status, failed) = match &result {
            Ok(_) => ("", ""),
            Err(refusal) => match sqlite_cause(refusal).map_or("_OTHER", error_type) {
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

/// The `SQLite` failure in `refusal`'s source chain, when it carries one.
fn sqlite_cause(refusal: &RiftError) -> Option<&rusqlite::Error> {
    std::iter::successors(std::error::Error::source(refusal), |cause| cause.source())
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>())
}

/// Reports the sizes of the database file at `path` and of its write-ahead log: two
/// `fs::metadata` calls. A file that does not exist reports nothing.
fn observe_file_sizes(path: &Path, observation: &Observation<'_, 2>) {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    for (kind, file) in [("database", path), ("wal", Path::new(&wal))] {
        if let Ok(metadata) = std::fs::metadata(file) {
            observation.observe([DB_NAMESPACE, kind], metadata.len());
        }
    }
}

/// One write transaction and the statements it started, counted as each is prepared:
/// every write prepares the one statement it runs.
struct CountedTransaction<'connection> {
    transaction: Transaction<'connection>,
    statements: Cell<u64>,
}

impl CountedTransaction<'_> {
    /// Prepares `sql` through the connection's statement cache and counts it.
    fn prepare(&self, sql: &str) -> rusqlite::Result<CachedStatement<'_>> {
        self.statements.set(self.statements.get().saturating_add(1));
        self.transaction.prepare_cached(sql)
    }
}

/// The `error.type` of a failed statement: `SQLite`'s result code for busy, `5`, and for
/// locked, `6`, the two a writer waits on, and `_OTHER` for every other failure.
fn error_type(failure: &rusqlite::Error) -> &'static str {
    match failure.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy) => "5",
        Some(rusqlite::ErrorCode::DatabaseLocked) => "6",
        _ => "_OTHER",
    }
}

/// Runs `body` inside one `BEGIN IMMEDIATE` transaction on `connection` and commits
/// what it wrote; a failed `body` rolls back. `operations` name the begin and the commit
/// in a refusal.
///
/// It records the wait for the write lock, `body` as the `db.client.operation.duration`
/// point of `operation`, the statements `body` started, the commit, and the transaction
/// from its begin to its commit or rollback, all under `db.namespace` = `history`.
fn in_write_transaction<Answer>(
    connection: &mut Connection,
    operation: &'static str,
    operations: [&'static str; 2],
    body: impl FnOnce(&CountedTransaction<'_>) -> Result<Answer, RiftError>,
) -> Result<Answer, RiftError> {
    let [begin_operation, commit_operation] = operations;
    let begun;
    let waited = rift_tracing::measure_elapsed!("sqlite.write_lock.wait", {
        begun = connection.transaction_with_behavior(TransactionBehavior::Immediate);
    })
    .ok()
    .map(|((), waited)| waited);
    if let Some(waited) = waited {
        let failed = begun.as_ref().err().map_or("", error_type);
        WRITE_LOCK_WAIT
            .labeled([DB_NAMESPACE, failed])
            .record(waited.elapsed());
    }
    let transaction = begun.map_err(|source| {
        errors::history_store::database()
            .operation(begin_operation)
            .detail(source)
            .error()
    })?;
    let ended;
    let lasted = rift_tracing::measure_elapsed!("sqlite.transaction", {
        ended = finish_transaction(transaction, operation, body, commit_operation);
    })
    .ok()
    .map(|((), lasted)| lasted);
    let (answer, result) = ended;
    if let Some(lasted) = lasted {
        TRANSACTION_DURATION
            .labeled([DB_NAMESPACE, result])
            .record(lasted.elapsed());
    }
    answer
}

/// Runs `body` in `transaction` as `operation`, then commits it, or rolls it back when
/// `body` fails; answers `body`'s answer, or the failure, with the
/// `sqlite.transaction.result` it ended with. `commit` names the commit in a refusal.
fn finish_transaction<Answer>(
    transaction: Transaction<'_>,
    operation: &'static str,
    body: impl FnOnce(&CountedTransaction<'_>) -> Result<Answer, RiftError>,
    commit: &'static str,
) -> (Result<Answer, RiftError>, &'static str) {
    let counted = CountedTransaction {
        transaction,
        statements: Cell::new(0),
    };
    let answered = timed(operation, || body(&counted));
    TRANSACTION_STATEMENTS
        .labeled([DB_NAMESPACE])
        .record(counted.statements.get());
    let CountedTransaction { transaction, .. } = counted;
    let answer = match answered {
        Ok(answer) => answer,
        Err(failure) => {
            drop(transaction);
            return (Err(failure), "rollback");
        }
    };
    let committed;
    let measured = rift_tracing::measure_elapsed!("sqlite.commit", {
        committed = transaction.commit();
    });
    if let Ok(((), measured)) = measured {
        COMMIT_DURATION
            .labeled([DB_NAMESPACE])
            .record(measured.elapsed());
    }
    match committed {
        Ok(()) => (Ok(answer), "commit"),
        Err(source) => (
            errors::history_store::database()
                .operation(commit)
                .detail(source)
                .fail(),
            "rollback",
        ),
    }
}

/// Every commit the store `connection` reads holds, keyed by commit id. The read is one
/// `db.client.operation.duration` point.
fn held_commits(connection: &Connection) -> Result<HashMap<String, HeldCommit>, RiftError> {
    timed(QUERY_OPERATION, || {
        connection
            .prepare_cached("SELECT id, base, boundary FROM commits")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            HeldCommit {
                                base: row.get(1)?,
                                boundary: row.get(2)?,
                            },
                        ))
                    })?
                    .collect::<Result<_, _>>()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read held commits")
                    .detail(source)
                    .error()
            })
    })
}

/// Writes one commit's rows, the message index entry after the commit row.
fn write_commit(
    transaction: &CountedTransaction<'_>,
    commit: &CommitRecord,
) -> Result<(), RiftError> {
    transaction
        .prepare(
            "INSERT INTO commits(id, base, boundary, author_name, author_email, committed_at, \
             time, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                commit.id,
                commit.base,
                commit.boundary,
                commit.author.name,
                commit.author.email,
                commit.committed_at,
                commit.time,
                commit.message
            ])
        })
        .map_err(|source| {
            errors::history_store::database()
                .operation("write commit")
                .detail(source)
                .error()
        })?;
    let row = transaction.transaction.last_insert_rowid();
    transaction
        .prepare("INSERT INTO commit_text(rowid, message) VALUES (?1, ?2)")
        .and_then(|mut statement| statement.execute(params![row, commit.message]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("index commit message")
                .detail(source)
                .error()
        })?;
    write_commit_paths_and_declarations(transaction, row, commit)?;
    Ok(())
}

/// Writes path and declaration rows that belong to one commit row.
fn write_commit_paths_and_declarations(
    transaction: &CountedTransaction<'_>,
    row: i64,
    commit: &CommitRecord,
) -> Result<(), RiftError> {
    for changed in &commit.paths {
        transaction
            .prepare("INSERT INTO changed_paths VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    changed.path,
                    changed.old_blob,
                    changed.new_blob
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write changed path")
                    .detail(source)
                    .error()
            })?;
    }
    for renamed in &commit.renames {
        transaction
            .prepare("INSERT INTO renamed_paths VALUES (?1, ?2, ?3)")
            .and_then(|mut statement| {
                statement.execute(params![row, renamed.old_path, renamed.new_path])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write renamed path")
                    .detail(source)
                    .error()
            })?;
    }
    for moved in &commit.moves {
        transaction
            .prepare("INSERT INTO moved_declarations VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    moved.new_path,
                    moved.qualified_name,
                    moved.old_path
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write moved declaration")
                    .detail(source)
                    .error()
            })?;
    }
    for declaration in &commit.declarations {
        let change = change_code(declaration.change);
        transaction
            .prepare("INSERT INTO changed_declarations VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    declaration.path,
                    declaration.qualified_name,
                    change
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write changed declaration")
                    .detail(source)
                    .error()
            })?;
    }
    Ok(())
}

/// Deletes one commit row and every row that belongs to it. The message
/// index entry goes first, while the commit row still holds the message it
/// was indexed from.
fn delete_commit(transaction: &CountedTransaction<'_>, row: i64) -> Result<(), RiftError> {
    transaction
        .prepare(
            "INSERT INTO commit_text(commit_text, rowid, message) \
             SELECT 'delete', row, message FROM commits WHERE row = ?1",
        )
        .and_then(|mut statement| statement.execute([row]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("delete message index entry")
                .detail(source)
                .error()
        })?;
    for table in COMMIT_CHILD_TABLES {
        transaction
            .prepare(&format!("DELETE FROM {table} WHERE commit_row = ?1"))
            .and_then(|mut statement| statement.execute([row]))
            .map_err(|source| {
                errors::history_store::database()
                    .operation("delete commit rows")
                    .detail(source)
                    .error()
            })?;
    }
    transaction
        .prepare("DELETE FROM commits WHERE row = ?1")
        .and_then(|mut statement| statement.execute([row]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("delete commit")
                .detail(source)
                .error()
        })?;
    Ok(())
}

/// The stored spelling of a change: its wire spelling, which serde owns.
fn change_code(change: SymbolVersionKind) -> String {
    match serde_json::to_value(change) {
        Ok(serde_json::Value::String(code)) => code,
        other => unreachable!("a version kind serializes as a string: {other:?}"),
    }
}

/// The change a stored spelling names; `None` for a spelling no variant
/// carries.
fn stored_change(code: String) -> Option<SymbolVersionKind> {
    serde_json::from_value(serde_json::Value::String(code)).ok()
}

/// Opens read connections to one store file.
#[derive(Clone, Debug)]
pub struct StoreReader {
    database: Arc<Path>,
}

impl StoreReader {
    pub(crate) fn new(database: PathBuf) -> Self {
        Self {
            database: Arc::from(database),
        }
    }

    /// Opens one read connection. A connection answers one request's reads;
    /// WAL mode keeps it off the filler's write lock.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the file.
    pub fn connect(&self) -> Result<StoreReads, RiftError> {
        let connection = connect(&self.database)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|source| {
                errors::history_store::database()
                    .operation("enter read-only mode")
                    .detail(source)
                    .error()
            })?;
        Ok(StoreReads { connection })
    }
}

/// One commit as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCommit {
    row: i64,
    /// The commit id, full lowercase hex.
    pub id: String,
    /// The commit it was compared with.
    pub base: Option<String>,
    /// Whether history past it is out of the store's reach.
    pub boundary: bool,
    /// The author the commit records.
    pub author: CommitAuthor,
    /// The committer time, an RFC 3339 date-time carrying the recorded
    /// offset.
    pub committed_at: String,
    /// The full commit message.
    pub message: String,
}

impl StoredCommit {
    /// The message's first line; `None` for an empty one.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.message
            .lines()
            .next()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
    }
}

/// One read connection's queries.
#[derive(Debug)]
pub struct StoreReads {
    connection: Connection,
}

impl StoreReads {
    /// The read connection, so a test reaches the tables directly.
    #[cfg(test)]
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Every commit the store holds, keyed by commit id, as the filler reads
    /// them: a server whose task does not hold the fill lock plans against
    /// this to know how far another server's fill has got.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn held(&self) -> Result<HashMap<String, HeldCommit>, RiftError> {
        held_commits(&self.connection)
    }

    /// The commit `id` names, when the store holds it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn commit(&self, id: &str) -> Result<Option<StoredCommit>, RiftError> {
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT row, id, base, boundary, author_name, author_email, committed_at, \
                     message FROM commits WHERE id = ?1",
                )
                .and_then(|mut statement| {
                    statement
                        .query_row([id], |row| {
                            Ok(StoredCommit {
                                row: row.get(0)?,
                                id: row.get(1)?,
                                base: row.get(2)?,
                                boundary: row.get(3)?,
                                author: CommitAuthor {
                                    name: row.get(4)?,
                                    email: row.get(5)?,
                                },
                                committed_at: row.get(6)?,
                                message: row.get(7)?,
                            })
                        })
                        .optional()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read commit")
                        .detail(source)
                        .error()
                })
        })
    }

    /// How `commit` changed the declaration `qualified_name` at `path`, when
    /// it changed it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn declaration_change(
        &self,
        commit: &StoredCommit,
        path: &str,
        qualified_name: &str,
    ) -> Result<Option<SymbolVersionKind>, RiftError> {
        let code: Option<String> = timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT change FROM changed_declarations \
                 WHERE commit_row = ?1 AND path = ?2 AND qualified_name = ?3",
                )
                .and_then(|mut statement| {
                    statement
                        .query_row(params![commit.row, path, qualified_name], |row| row.get(0))
                        .optional()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read declaration change")
                        .detail(source)
                        .error()
                })
        })?;
        Ok(code.and_then(stored_change))
    }

    /// The path `commit` moved the file now at `new_path` from, when it moved
    /// it without changing its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn renamed_from(
        &self,
        commit: &StoredCommit,
        new_path: &str,
    ) -> Result<Option<String>, RiftError> {
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT old_path FROM renamed_paths WHERE commit_row = ?1 AND new_path = ?2",
                )
                .and_then(|mut statement| {
                    statement
                        .query_row(params![commit.row, new_path], |row| row.get(0))
                        .optional()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read renamed path")
                        .detail(source)
                        .error()
                })
        })
    }

    /// The path `commit` moved the declaration `qualified_name` now at `new_path`
    /// from, when it moved it between files that were no pure rename.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn moved_from(
        &self,
        commit: &StoredCommit,
        new_path: &str,
        qualified_name: &str,
    ) -> Result<Option<String>, RiftError> {
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT old_path FROM moved_declarations \
                 WHERE commit_row = ?1 AND new_path = ?2 AND qualified_name = ?3",
                )
                .and_then(|mut statement| {
                    statement
                        .query_row(params![commit.row, new_path, qualified_name], |row| {
                            row.get(0)
                        })
                        .optional()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read moved declaration")
                        .detail(source)
                        .error()
                })
        })
    }

    /// The paths `commit` changed against the commit it was compared with, in path
    /// order, at most `limit` of them.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn changed_paths(
        &self,
        commit: &StoredCommit,
        limit: usize,
    ) -> Result<Vec<String>, RiftError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT path FROM changed_paths WHERE commit_row = ?1 ORDER BY path LIMIT ?2",
                )
                .and_then(|mut statement| {
                    statement
                        .query_map(params![commit.row, limit], |row| row.get(0))?
                        .collect()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read changed paths")
                        .detail(source)
                        .error()
                })
        })
    }

    /// The held commit no other held commit was compared with, newest first:
    /// the newest release a release chain starts at.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn chain_head(&self) -> Result<Option<String>, RiftError> {
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT id FROM commits WHERE id NOT IN \
                 (SELECT base FROM commits WHERE base IS NOT NULL) \
                 ORDER BY time DESC, id LIMIT 1",
                )
                .and_then(|mut statement| statement.query_row([], |row| row.get(0)).optional())
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read chain head")
                        .detail(source)
                        .error()
                })
        })
    }

    /// The ids of the commits whose message matches the full-text `query`,
    /// newest first, at most `limit` of them.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the query, a malformed
    /// full-text query included.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<String>, RiftError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        timed(QUERY_OPERATION, || {
            self.connection
                .prepare_cached(
                    "SELECT commits.id FROM commit_text \
                 JOIN commits ON commits.row = commit_text.rowid \
                 WHERE commit_text MATCH ?1 ORDER BY commits.time DESC, commits.id LIMIT ?2",
                )
                .and_then(|mut statement| {
                    statement
                        .query_map(params![query, limit], |row| row.get(0))?
                        .collect()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("search commit messages")
                        .detail(source)
                        .error()
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rift_error::errors;
    use rift_protocol::read::CommitAuthor;
    use rift_tracing::{MetricSeries, MetricSnapshot, ScopedRecorder, SeriesValue};
    use rusqlite::ffi;

    use super::{QUERY_OPERATION, error_type, timed};
    use crate::{CommitRecord, HistoryStore, StoreLocation};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// One commit with no changed path, rename, move, or declaration: writing it new runs
    /// three statements, the held read and the two inserts.
    fn bare_commit(id: &str) -> CommitRecord {
        CommitRecord {
            id: id.to_owned(),
            base: None,
            boundary: false,
            author: CommitAuthor {
                name: "Rift Fixture".to_owned(),
                email: "fixture@rift.invalid".to_owned(),
            },
            committed_at: "2026-01-01T00:00:00+00:00".to_owned(),
            time: 0,
            message: "Bare commit".to_owned(),
            paths: Vec::new(),
            renames: Vec::new(),
            moves: Vec::new(),
            declarations: Vec::new(),
        }
    }

    /// The count and sum of the histogram series `name` carries under exactly `labels`, or
    /// zeros when no point reached it.
    fn histogram(metrics: &MetricSnapshot, name: &str, labels: &[(&str, &str)]) -> (u64, f64) {
        match metrics.find(name, labels).map(MetricSeries::value) {
            Some(SeriesValue::Buckets { count, sum, .. }) => (*count, *sum),
            _ => (0, 0.0),
        }
    }

    /// The `db.client.operation.duration` points of the history operation `operation`
    /// that ended without a failure.
    fn operations(metrics: &MetricSnapshot, operation: &str) -> u64 {
        let labels = [
            ("db.system.name", "sqlite"),
            ("db.namespace", "history"),
            ("db.operation.name", operation),
        ];
        histogram(metrics, "db.client.operation.duration", &labels).0
    }

    /// The store records each open, batch, trim, and read as one operation, counts the
    /// statements each write transaction ran, and reports its file size while its filler
    /// lives.
    #[test]
    fn the_store_records_its_operations_statements_and_file_size() -> TestResult {
        let (recorder, _drain) = ScopedRecorder::builder().install()?;
        let folder = tempfile::tempdir()?;
        let store = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))?;
        let mut filler = store.filler()?.ok_or("the first filler takes the lock")?;
        let statements = [("db.namespace", "history")];
        let counted = |metrics: &MetricSnapshot| {
            histogram(metrics, "sqlite.transaction.statement.count", &statements)
        };

        filler.write_batch(&[bare_commit("c1")])?;
        let written = recorder.metrics();
        filler.write_batch(&[bare_commit("c1")])?;
        let replaced = recorder.metrics();
        let trimmed_count = filler.trim(&BTreeSet::new())?;
        let trimmed = recorder.metrics();
        store.reader().connect()?.held()?;
        let read = recorder.metrics();

        assert_eq!(counted(&written), (1, 3.0), "a held read and two inserts");
        assert_eq!(
            counted(&replaced),
            (2, 12.0),
            "a held read, six deletes, and two inserts more"
        );
        assert_eq!(trimmed_count, 1);
        assert_eq!(
            counted(&trimmed),
            (3, 19.0),
            "a read of the held and six deletes more"
        );
        assert_eq!(
            operations(&read, "connect"),
            3,
            "the store, the filler, the reader"
        );
        assert_eq!(operations(&read, "insert"), 2);
        assert_eq!(operations(&read, "trim"), 1);
        assert_eq!(operations(&read, "query"), 1);
        let database_file = [
            ("db.namespace", "history"),
            ("sqlite.file.type", "database"),
        ];
        assert!(
            matches!(
                read.find("sqlite.file.size", &database_file).map(MetricSeries::value),
                Some(SeriesValue::Sum(size)) if *size > 0.0
            ),
            "{read:?}"
        );
        drop(filler);
        assert!(
            recorder
                .metrics()
                .find("sqlite.file.size", &database_file)
                .is_none(),
            "a dropped filler reports no size"
        );
        Ok(())
    }

    /// A refused operation whose source chain carries `SQLite`'s busy result records the
    /// code as `db.response.status_code`; one without a `SQLite` cause records `_OTHER`
    /// alone.
    #[test]
    fn a_refused_operation_records_the_sqlite_result_its_refusal_carries() -> TestResult {
        let (recorder, _drain) = ScopedRecorder::builder().install()?;

        let busy = timed(QUERY_OPERATION, || -> Result<(), _> {
            errors::history_store::database()
                .operation("read held commits")
                .detail(failure(ffi::SQLITE_BUSY))
                .fail()
        });
        let other = timed(QUERY_OPERATION, || -> Result<(), _> {
            errors::history_store::database()
                .operation("read held commits")
                .detail(std::io::Error::other("not a database failure"))
                .fail()
        });
        let metrics = recorder.metrics();

        assert!(busy.is_err() && other.is_err());
        let refused = |status: Option<&'static str>| {
            let mut labels = vec![
                ("db.system.name", "sqlite"),
                ("db.namespace", "history"),
                ("db.operation.name", "query"),
                ("error.type", "_OTHER"),
            ];
            labels.extend(status.map(|code| ("db.response.status_code", code)));
            histogram(&metrics, "db.client.operation.duration", &labels).0
        };
        assert_eq!(refused(Some("5")), 1, "{metrics:?}");
        assert_eq!(refused(None), 1, "{metrics:?}");
        Ok(())
    }

    fn failure(code: std::ffi::c_int) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(ffi::Error::new(code), None)
    }

    #[test]
    fn error_type_names_busy_and_locked_by_result_code_and_every_other_failure_as_other() {
        assert_eq!(error_type(&failure(ffi::SQLITE_BUSY)), "5");
        assert_eq!(error_type(&failure(ffi::SQLITE_LOCKED)), "6");
        assert_eq!(error_type(&failure(ffi::SQLITE_LOCKED_SHAREDCACHE)), "6");
        assert_eq!(error_type(&failure(ffi::SQLITE_IOERR)), "_OTHER");
        assert_eq!(error_type(&rusqlite::Error::QueryReturnedNoRows), "_OTHER");
    }
}
