//! Reads of the metrics database, one connection per read.
//!
//! A read connection never creates the file, never creates a table, and never switches
//! the journal mode: those belong to the writer. WAL mode keeps it off the writer's lock,
//! so a read answers from the last committed snapshot while a write is in flight. The
//! reads are synchronous; a caller on an async runtime runs them on a blocking thread.

use std::path::Path;
use std::sync::Arc;

use rift_error::RiftError;
use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, Row, params_from_iter};

use crate::record::{LOG_KIND, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, StoredLogRecord};
use crate::store::{METRICS_BUSY_TIMEOUT, METRICS_SCHEMA_VERSION, store_failure, timed};

/// The `db.operation.name` of one read connection's open: the file's open, its busy
/// timeout, its read-only mode, and the read of its schema version.
const CONNECT_OPERATION: &str = "connect";
/// The `db.operation.name` of one read connection's query: a page or a count.
const QUERY_OPERATION: &str = "query";

/// The columns one page selects, in the order [`stored_record`] reads them.
const SELECT_RECORDS: &str = "SELECT id, recorded_at, level, target, component, \
                              operation, message, fields FROM log_records";

/// Opens read connections to one metrics database file.
#[derive(Clone, Debug)]
pub struct LogReader {
    path: Arc<Path>,
}

impl LogReader {
    /// A reader of the metrics database at `path`. Nothing is opened until
    /// [`Self::connect`].
    #[must_use]
    pub fn new(path: &Path) -> Self {
        Self {
            path: Arc::from(path),
        }
    }

    /// The metrics database file this reader opens.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Opens one read connection, in read-only query mode, without the create flag.
    ///
    /// A file at schema version zero holds no table yet and answers no records. The open,
    /// up to the schema version's read, is one `db.client.operation.duration` point.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when the file is absent or refused, or when it
    /// carries a schema version other than [`METRICS_SCHEMA_VERSION`].
    pub fn connect(&self) -> Result<LogReads, RiftError> {
        let (connection, found) = timed(
            CONNECT_OPERATION,
            || {
                let connection = Connection::open_with_flags(
                    &self.path,
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
                .map_err(|source| ("open for reading", source))?;
                connection
                    .busy_timeout(METRICS_BUSY_TIMEOUT)
                    .map_err(|source| ("set the busy timeout", source))?;
                connection
                    .pragma_update(None, "query_only", true)
                    .map_err(|source| ("enter read-only mode", source))?;
                let found: i64 = connection
                    .pragma_query_value(None, "user_version", |row| row.get(0))
                    .map_err(|source| ("read the schema version", source))?;
                Ok((connection, found))
            },
            |(_, source)| source,
        )
        .map_err(|(operation, source)| store_failure(operation, &self.path, source))?;
        let holds_records = match found {
            0 => false,
            METRICS_SCHEMA_VERSION => true,
            other => {
                return Err(store_failure(
                    "read the schema version",
                    &self.path,
                    std::io::Error::other(format!(
                        "the file carries schema version {other}, and this build reads \
                         version {METRICS_SCHEMA_VERSION}"
                    )),
                ));
            }
        };
        Ok(LogReads {
            connection,
            path: Arc::clone(&self.path),
            holds_records,
        })
    }
}

/// One read connection's queries.
#[derive(Debug)]
pub struct LogReads {
    connection: Connection,
    path: Arc<Path>,
    holds_records: bool,
}

/// Which end of the store one page reads from.
#[derive(Clone, Copy, Debug)]
enum RecordOrder {
    /// Highest identity first.
    Newest,
    /// Lowest identity first.
    Oldest,
}

impl RecordOrder {
    /// The ordering and limit clause of one page.
    const fn clause(self) -> &'static str {
        match self {
            Self::Newest => " ORDER BY id DESC LIMIT ?",
            Self::Oldest => " ORDER BY id ASC LIMIT ?",
        }
    }
}

impl LogReads {
    /// The newest records the query selects, newest first. The read is one
    /// `db.client.operation.duration` point.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when `SQLite` refuses the read.
    pub fn recent(&self, query: &LogQuery) -> Result<Vec<StoredLogRecord>, RiftError> {
        self.page(query, RecordOrder::Newest)
    }

    /// The records the query selects, oldest first.
    ///
    /// A caller following the store reads with [`LogQuery::after`] set to the last
    /// identity it took, so one call returns only what landed since. The page is bounded
    /// by the query's own limit, itself bounded by [`LOG_PAGE_RECORDS_MAX`]. The read is one
    /// `db.client.operation.duration` point.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when `SQLite` refuses the read.
    pub fn following(&self, query: &LogQuery) -> Result<Vec<StoredLogRecord>, RiftError> {
        self.page(query, RecordOrder::Oldest)
    }

    /// How many records the store currently holds. The read is one
    /// `db.client.operation.duration` point.
    ///
    /// # Errors
    ///
    /// Returns `tracing.log_store_failed` when `SQLite` refuses the read.
    pub fn count(&self) -> Result<u64, RiftError> {
        if !self.holds_records {
            return Ok(0);
        }
        let held: i64 = timed(
            QUERY_OPERATION,
            || {
                self.connection
                    .query_row("SELECT COUNT(*) FROM log_records", [], |row| row.get(0))
            },
            |source| source,
        )
        .map_err(|source| store_failure("count records", &self.path, source))?;
        Ok(u64::try_from(held).unwrap_or(0))
    }

    /// One page in `order`. The statement is assembled from fixed fragments for the
    /// filters the query carries, and every value is bound.
    fn page(
        &self,
        query: &LogQuery,
        order: RecordOrder,
    ) -> Result<Vec<StoredLogRecord>, RiftError> {
        if !self.holds_records {
            return Ok(Vec::new());
        }
        let mut conditions: Vec<&str> = vec!["kind = ?"];
        let mut values: Vec<Value> = Vec::with_capacity(7);
        values.push(Value::Text(LOG_KIND.to_owned()));
        if let Some(level) = &query.level {
            conditions.push("level = ?");
            values.push(Value::Text(level.clone()));
        }
        if let Some(component) = &query.component {
            conditions.push("component = ?");
            values.push(Value::Text(component.clone()));
        }
        if let Some(after) = query.after {
            conditions.push("id > ?");
            values.push(Value::Integer(after));
        }
        if let Some(since_ms) = query.since_ms {
            conditions.push("recorded_at >= ?");
            values.push(Value::Integer(since_ms));
        }
        if let Some(until_ms) = query.until_ms {
            conditions.push("recorded_at < ?");
            values.push(Value::Integer(until_ms));
        }
        let limit = query.limit.min(LOG_PAGE_RECORDS_MAX);
        values.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        let mut sql = String::from(SELECT_RECORDS);
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
        sql.push_str(order.clause());
        timed(
            QUERY_OPERATION,
            || {
                let mut statement = self.connection.prepare_cached(&sql)?;
                let rows = statement.query_map(params_from_iter(values.iter()), stored_record)?;
                rows.collect::<Result<Vec<_>, _>>()
            },
            |source| source,
        )
        .map_err(|source| store_failure("read records", &self.path, source))
    }
}

/// One stored row as the reader sees it.
fn stored_record(row: &Row<'_>) -> rusqlite::Result<StoredLogRecord> {
    Ok(StoredLogRecord {
        identity: row.get(0)?,
        record: LogRecord {
            recorded_at_ms: row.get(1)?,
            level: row.get(2)?,
            target: row.get(3)?,
            component: row.get(4)?,
            operation: row.get(5)?,
            message: row.get(6)?,
            fields: row.get(7)?,
        },
    })
}
