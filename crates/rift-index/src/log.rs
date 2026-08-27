//! The server's own log records, stored beside the search index in `.rift/db`.
//!
//! A server whose index will not settle answers no read, and its diagnostics
//! leave on stderr, where the agent asking the question cannot reach them. The
//! rows here are what an agent reads back through `rift://logs` while that
//! index lane is still stuck, so the run explains itself without an operator
//! copying a terminal.
//!
//! The rows live in the workspace database the search tiers share. One drain
//! task writes them through the database's write turn, while request handlers
//! read committed WAL snapshots through read-only connections.

use std::sync::Arc;

use toasty::db::Connection;

use crate::database::WorkspaceDatabase;
use crate::lexical::{LexicalIndexError, storage_error};

/// Most records one append may carry. A drain task holding more splits.
pub const LOG_BATCH_RECORDS_MAX: usize = 4_096;
/// Maximum UTF-8 bytes kept for one record's message. A longer message is
/// truncated at a character boundary rather than refused: a log record exists
/// to be read back, and half of it read back beats none.
pub const LOG_MESSAGE_BYTES_MAX: usize = 8_192;
/// Maximum UTF-8 bytes kept for one record's rendered fields.
pub const LOG_FIELDS_BYTES_MAX: usize = 8_192;
/// Maximum UTF-8 bytes kept for one record's level, target, component, or
/// operation. These are short by construction; the bound stops a caller's
/// runaway value from filling the file.
pub const LOG_LABEL_BYTES_MAX: usize = 256;
/// Most records one read may return, whatever page size a caller asks for.
pub const LOG_PAGE_RECORDS_MAX: usize = 5_000;

/// One record on its way into the store: when it happened, how severe it was,
/// where it came from, and what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRecord {
    recorded_at_ms: i64,
    level: String,
    target: String,
    component: String,
    operation: String,
    message: String,
    fields: String,
}

impl LogRecord {
    /// Builds one record, bounding every field it carries.
    ///
    /// Every string is truncated to its maximum at a character boundary, so a
    /// record built from an unbounded message still costs a bounded row.
    #[must_use]
    pub fn new(
        recorded_at_ms: i64,
        level: &str,
        target: &str,
        component: &str,
        operation: &str,
        message: &str,
        fields: &str,
    ) -> Self {
        Self {
            recorded_at_ms,
            level: bounded(&level.to_lowercase(), LOG_LABEL_BYTES_MAX),
            target: bounded(target, LOG_LABEL_BYTES_MAX),
            component: bounded(component, LOG_LABEL_BYTES_MAX),
            operation: bounded(operation, LOG_LABEL_BYTES_MAX),
            message: bounded(message, LOG_MESSAGE_BYTES_MAX),
            fields: bounded(fields, LOG_FIELDS_BYTES_MAX),
        }
    }

    /// Milliseconds since the Unix epoch at which the record was emitted.
    #[must_use]
    pub const fn recorded_at_ms(&self) -> i64 {
        self.recorded_at_ms
    }

    /// The record's severity in lower case, whatever case the caller spelled
    /// it: a level read and a stored row have to match on one spelling.
    #[must_use]
    pub fn level(&self) -> &str {
        &self.level
    }

    /// The emitting module path.
    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The `component` field the emitting span or event carried, empty when it
    /// carried none.
    #[must_use]
    pub fn component(&self) -> &str {
        &self.component
    }

    /// The `operation` field the emitting span or event carried, empty when it
    /// carried none.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }

    /// The record's message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The record's remaining fields, rendered as a JSON object.
    #[must_use]
    pub fn fields(&self) -> &str {
        &self.fields
    }
}

/// One record read back, carrying the identity the store filed it under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLogRecord {
    identity: i64,
    record: LogRecord,
}

impl StoredLogRecord {
    /// The store's own ascending identity for this record. Two records from one
    /// run compare by it, whatever clock wrote their timestamps.
    #[must_use]
    pub const fn identity(&self) -> i64 {
        self.identity
    }

    /// The record itself.
    #[must_use]
    pub const fn record(&self) -> &LogRecord {
        &self.record
    }
}

/// Which records one read returns, and how many.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogQuery {
    level: Option<String>,
    component: Option<String>,
    limit: usize,
}

impl LogQuery {
    /// A read of the newest `limit` records, bounded by
    /// [`LOG_PAGE_RECORDS_MAX`].
    #[must_use]
    pub fn newest(limit: usize) -> Self {
        Self {
            level: None,
            component: None,
            limit: limit.min(LOG_PAGE_RECORDS_MAX),
        }
    }

    /// Restricts the read to one severity, matched in lower case.
    #[must_use]
    pub fn at_level(mut self, level: &str) -> Self {
        self.level = Some(bounded(&level.to_lowercase(), LOG_LABEL_BYTES_MAX));
        self
    }

    /// Restricts the read to one component.
    #[must_use]
    pub fn for_component(mut self, component: &str) -> Self {
        self.component = Some(bounded(component, LOG_LABEL_BYTES_MAX));
        self
    }

    /// The severity this read is restricted to, when it is.
    #[must_use]
    pub fn level(&self) -> Option<&str> {
        self.level.as_deref()
    }

    /// The component this read is restricted to, when it is.
    #[must_use]
    pub fn component(&self) -> Option<&str> {
        self.component.as_deref()
    }

    /// The record count this read returns at most.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// The workspace's stored log records, one row per emitted event.
#[derive(Debug)]
pub struct LogStore {
    database: Arc<WorkspaceDatabase>,
}

impl LogStore {
    /// Attaches the log store to one already-open workspace database.
    #[must_use]
    pub fn attached(database: Arc<WorkspaceDatabase>) -> Self {
        Self { database }
    }

    /// Appends one batch and trims the store back to `retention_records`.
    ///
    /// Identities ascend from the highest the store already holds, assigned
    /// inside the same transaction that inserts, so two concurrent appends
    /// cannot mint one identity twice. A batch longer than
    /// [`LOG_BATCH_RECORDS_MAX`] is refused whole rather than half written.
    ///
    /// Returns the number of rows the trim dropped.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the batch is oversized or a write
    /// fails.
    ///
    /// # Cancel safety
    ///
    /// The append and its trim run in one transaction. Dropping this future
    /// before it commits leaves the store exactly as it was.
    pub async fn append(
        &self,
        records: &[LogRecord],
        retention_records: u64,
    ) -> Result<u64, LexicalIndexError> {
        if records.is_empty() {
            return Ok(0);
        }
        if records.len() > LOG_BATCH_RECORDS_MAX {
            return Err(crate::lexical::batch_limit_error(
                "logs.batch_records",
                records.len() as u64,
                LOG_BATCH_RECORDS_MAX as u64,
            ));
        }
        let mut access = self.database.writing().await?;
        let mut transaction = access.transaction().await?;
        let newest = LogRecordRow::all()
            .order_by(LogRecordRow::fields().id().desc())
            .first()
            .exec(&mut transaction)
            .await
            .map_err(storage_error)?;
        let first_identity = newest.map_or(1, |row| row.id.saturating_add(1));
        for (offset, record) in records.iter().enumerate() {
            LogRecordRow::create()
                .id(first_identity.saturating_add(batch_as_i64(offset)))
                .recorded_at(record.recorded_at_ms)
                .level(record.level.clone())
                .target(record.target.clone())
                .component(record.component.clone())
                .operation(record.operation.clone())
                .message(record.message.clone())
                .fields(record.fields.clone())
                .exec(&mut transaction)
                .await
                .map_err(storage_error)?;
        }
        let dropped = trim(&mut transaction, retention_records).await?;
        transaction.commit().await.map_err(storage_error)?;
        Ok(dropped)
    }

    /// The newest records the query selects, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the query fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only query.
    pub async fn recent(
        &self,
        query: &LogQuery,
    ) -> Result<Vec<StoredLogRecord>, LexicalIndexError> {
        let mut connection = self.connection().await?;
        let mut rows = LogRecordRow::all()
            .order_by(LogRecordRow::fields().id().desc())
            .limit(query.limit.min(LOG_PAGE_RECORDS_MAX));
        if let Some(level) = &query.level {
            rows = rows.filter(LogRecordRow::fields().level().eq(level.clone()));
        }
        if let Some(component) = &query.component {
            rows = rows.filter(LogRecordRow::fields().component().eq(component.clone()));
        }
        let found = rows.exec(&mut connection).await.map_err(storage_error)?;
        Ok(found.into_iter().map(stored_record).collect())
    }

    /// How many records the store currently holds.
    ///
    /// # Errors
    ///
    /// Returns [`LexicalIndexError`] when the query fails.
    ///
    /// # Cancel safety
    ///
    /// Cancellation performs no writes; this issues one read-only query.
    pub async fn count(&self) -> Result<u64, LexicalIndexError> {
        let mut connection = self.connection().await?;
        LogRecordRow::all()
            .count()
            .exec(&mut connection)
            .await
            .map_err(storage_error)
    }

    /// A pooled connection carrying the workspace database's required pragmas.
    async fn connection(&self) -> Result<Connection, LexicalIndexError> {
        self.database.connection().await
    }
}

/// Drops every record outside the newest `retention_records`, and reports how
/// many went. A retention of zero keeps the store empty rather than unbounded:
/// the configuration's own bound refuses zero, so this is the floor case alone.
async fn trim(
    transaction: &mut toasty::db::Transaction<'_>,
    retention_records: u64,
) -> Result<u64, LexicalIndexError> {
    let held = LogRecordRow::all()
        .count()
        .exec(&mut *transaction)
        .await
        .map_err(storage_error)?;
    let dropped = held.saturating_sub(retention_records);
    if dropped == 0 {
        return Ok(0);
    }
    let threshold_offset = usize::try_from(dropped.saturating_sub(1)).unwrap_or(usize::MAX);
    let threshold = LogRecordRow::all()
        .order_by(LogRecordRow::fields().id().asc())
        .limit(1)
        .offset(threshold_offset)
        .first()
        .exec(&mut *transaction)
        .await
        .map_err(storage_error)?;
    let Some(threshold) = threshold else {
        return Ok(0);
    };
    LogRecordRow::all()
        .filter(LogRecordRow::fields().id().le(threshold.id))
        .delete()
        .exec(transaction)
        .await
        .map_err(storage_error)?;
    Ok(dropped)
}

/// One stored row as the reader sees it.
fn stored_record(row: LogRecordRow) -> StoredLogRecord {
    StoredLogRecord {
        identity: row.id,
        record: LogRecord {
            recorded_at_ms: row.recorded_at,
            level: row.level,
            target: row.target,
            component: row.component,
            operation: row.operation,
            message: row.message,
            fields: row.fields,
        },
    }
}

/// One batch count as the integer an identity is minted from. A batch is
/// bounded by [`LOG_BATCH_RECORDS_MAX`] before it reaches here, so the
/// conversion only guards the type boundary.
fn batch_as_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// `value` truncated to at most `maximum` UTF-8 bytes, cut at a character
/// boundary.
fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Debug, toasty::Model)]
#[table = "log_records"]
pub(crate) struct LogRecordRow {
    #[key]
    id: i64,
    recorded_at: i64,
    #[index]
    level: String,
    target: String,
    #[index]
    component: String,
    operation: String,
    message: String,
    fields: String,
}

#[cfg(test)]
mod tests {
    use super::{
        LOG_BATCH_RECORDS_MAX, LOG_LABEL_BYTES_MAX, LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX,
        LogQuery, LogRecord, LogStore, bounded,
    };
    use crate::DatabasePool;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// Retention no suite here reaches, so nothing trims unless the suite is
    /// about trimming.
    const KEEP_EVERY: u64 = 1_000;

    fn pool() -> DatabasePool {
        DatabasePool::new(4, 1_000)
    }

    fn record(message: &str) -> LogRecord {
        LogRecord::new(
            1,
            "info",
            "rift_mcp::server",
            "index",
            "index.reconcile",
            message,
            "{}",
        )
    }

    async fn store(directory: &tempfile::TempDir) -> Result<LogStore, Box<dyn std::error::Error>> {
        let database = crate::WorkspaceDatabase::open(&directory.path().join("db"), pool()).await?;
        Ok(LogStore::attached(database))
    }

    #[tokio::test]
    async fn appended_records_read_back_newest_first() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;

        store
            .append(&[record("first"), record("second")], KEEP_EVERY)
            .await?;

        let read = store.recent(&LogQuery::newest(10)).await?;
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].record().message(), "second");
        assert_eq!(read[1].record().message(), "first");
        assert!(read[0].identity() > read[1].identity());
        Ok(())
    }

    #[tokio::test]
    async fn identities_ascend_across_appends() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;

        store.append(&[record("first")], KEEP_EVERY).await?;
        store.append(&[record("second")], KEEP_EVERY).await?;

        let read = store.recent(&LogQuery::newest(10)).await?;
        assert_eq!(read[0].identity(), 2);
        assert_eq!(read[1].identity(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn retention_drops_the_oldest_records() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        let batch: Vec<LogRecord> = (0..10).map(|index| record(&index.to_string())).collect();

        let dropped = store.append(&batch, 4).await?;

        assert_eq!(dropped, 6);
        assert_eq!(store.count().await?, 4);
        let read = store.recent(&LogQuery::newest(10)).await?;
        assert_eq!(read[0].record().message(), "9");
        assert_eq!(read[3].record().message(), "6");
        Ok(())
    }

    #[tokio::test]
    async fn retention_counts_records_the_store_already_held() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        store
            .append(&[record("first"), record("second")], 8)
            .await?;

        let dropped = store.append(&[record("third")], 2).await?;

        assert_eq!(dropped, 1);
        assert_eq!(store.count().await?, 2);
        Ok(())
    }

    #[tokio::test]
    async fn a_level_read_returns_only_that_level() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        let warning = LogRecord::new(1, "warn", "rift_mcp", "search", "search.open", "late", "{}");
        store
            .append(&[record("routine"), warning], KEEP_EVERY)
            .await?;

        let read = store.recent(&LogQuery::newest(10).at_level("WARN")).await?;

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].record().message(), "late");
        Ok(())
    }

    #[tokio::test]
    async fn a_component_read_returns_only_that_component() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        let search = LogRecord::new(
            1,
            "info",
            "rift_mcp",
            "search",
            "search.open",
            "opened",
            "{}",
        );
        store
            .append(&[record("reconciled"), search], KEEP_EVERY)
            .await?;

        let read = store
            .recent(&LogQuery::newest(10).for_component("search"))
            .await?;

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].record().message(), "opened");
        Ok(())
    }

    #[tokio::test]
    async fn a_read_bounds_its_own_page() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        let batch: Vec<LogRecord> = (0..5).map(|index| record(&index.to_string())).collect();
        store.append(&batch, KEEP_EVERY).await?;

        let read = store.recent(&LogQuery::newest(2)).await?;

        assert_eq!(read.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn an_oversized_batch_is_refused_whole() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;
        let batch: Vec<LogRecord> = (0..=LOG_BATCH_RECORDS_MAX).map(|_| record("x")).collect();

        let refusal = store.append(&batch, KEEP_EVERY).await;

        assert!(refusal.is_err(), "an oversized batch must be refused");
        assert_eq!(store.count().await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_batch_changes_nothing() -> TestResult {
        let directory = tempfile::tempdir()?;
        let store = store(&directory).await?;

        assert_eq!(store.append(&[], KEEP_EVERY).await?, 0);
        assert_eq!(store.count().await?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn a_reopened_store_keeps_its_records() -> TestResult {
        let directory = tempfile::tempdir()?;
        {
            let store = store(&directory).await?;
            store.append(&[record("survivor")], KEEP_EVERY).await?;
        }

        let store = store(&directory).await?;

        let read = store.recent(&LogQuery::newest(10)).await?;
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].record().message(), "survivor");
        Ok(())
    }

    #[test]
    fn a_long_message_is_bounded_at_a_character_boundary() {
        let long = "é".repeat(LOG_MESSAGE_BYTES_MAX);

        let record = LogRecord::new(1, "info", "rift", "index", "index.reconcile", &long, "{}");

        assert!(record.message().len() <= LOG_MESSAGE_BYTES_MAX);
        assert!(record.message().chars().all(|character| character == 'é'));
    }

    #[test]
    fn a_long_label_is_bounded() {
        let long = "c".repeat(LOG_LABEL_BYTES_MAX * 2);

        let record = LogRecord::new(1, "info", &long, &long, &long, "message", "{}");

        assert_eq!(record.target().len(), LOG_LABEL_BYTES_MAX);
        assert_eq!(record.component().len(), LOG_LABEL_BYTES_MAX);
    }

    #[test]
    fn a_page_larger_than_the_maximum_is_bounded() {
        assert_eq!(
            LogQuery::newest(LOG_PAGE_RECORDS_MAX * 2).limit(),
            LOG_PAGE_RECORDS_MAX
        );
    }

    #[test]
    fn a_value_within_the_bound_is_unchanged() {
        assert_eq!(bounded("short", 64), "short");
    }

    #[test]
    fn a_bound_inside_a_multibyte_character_moves_to_its_start() {
        assert_eq!(bounded("éé", 3), "é");
    }
}
