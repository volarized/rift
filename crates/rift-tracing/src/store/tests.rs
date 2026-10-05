use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::time::Instant;

use super::{LogStore, METRICS_SCHEMA_VERSION};
use crate::{
    LOG_BATCH_RECORDS_MAX, LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogReader,
    LogReads, LogRecord, RecordKind,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Retention no suite here reaches, so nothing trims unless the suite is about trimming.
const KEEP_EVERY: u64 = 1_000;
/// Failure bound on one wait for the writer thread; never a way to order two events.
const THREAD_WAIT_MAX: Duration = Duration::from_secs(10);

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

fn metrics_path(directory: &tempfile::TempDir) -> PathBuf {
    directory.path().join("metrics")
}

async fn store(directory: &tempfile::TempDir) -> Result<LogStore, Box<dyn std::error::Error>> {
    Ok(LogStore::open(&metrics_path(directory), None).await?)
}

fn reads(store: &LogStore) -> Result<LogReads, Box<dyn std::error::Error>> {
    Ok(store.reader().connect()?)
}

fn close_deadline() -> Instant {
    Instant::now() + THREAD_WAIT_MAX
}

/// The schema version the file at `path` carries, read on a connection of the test's own.
fn user_version(path: &Path) -> Result<i64, Box<dyn std::error::Error>> {
    let connection = rusqlite::Connection::open(path)?;
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// The write-ahead log file beside the metrics database at `path`.
fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// An owner that reports, from the thread that drops it, that thread's name.
struct ReleaseProbe(mpsc::Sender<Option<String>>);

impl Drop for ReleaseProbe {
    fn drop(&mut self) {
        let _ = self
            .0
            .send(std::thread::current().name().map(str::to_owned));
    }
}

fn release_probe() -> (Arc<dyn Send + Sync>, mpsc::Receiver<Option<String>>) {
    let (sender, receiver) = mpsc::channel();
    (Arc::new(ReleaseProbe(sender)), receiver)
}

#[tokio::test]
async fn appended_records_read_back_newest_first() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;

    store
        .append(&[record("first"), record("second")], KEEP_EVERY)
        .await?;

    let read = reads(&store)?.recent(&LogQuery::newest(10))?;
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

    let read = reads(&store)?.recent(&LogQuery::newest(10))?;
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
    let reads = reads(&store)?;
    assert_eq!(reads.count()?, 4);
    let read = reads.recent(&LogQuery::newest(10))?;
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
    assert_eq!(reads(&store)?.count()?, 2);
    Ok(())
}

#[tokio::test]
async fn a_retention_of_zero_leaves_the_store_empty() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;

    let dropped = store
        .append(&[record("first"), record("second")], 0)
        .await?;

    assert_eq!(dropped, 2);
    assert_eq!(reads(&store)?.count()?, 0);
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

    let read = reads(&store)?.recent(&LogQuery::newest(10).at_level("WARN"))?;

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

    let read = reads(&store)?.recent(&LogQuery::newest(10).for_component("search"))?;

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

    let read = reads(&store)?.recent(&LogQuery::newest(2))?;

    assert_eq!(read.len(), 2);
    Ok(())
}

#[tokio::test]
async fn a_followed_read_returns_records_oldest_first() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store
        .append(&[record("first"), record("second")], KEEP_EVERY)
        .await?;

    let read = reads(&store)?.following(&LogQuery::newest(10))?;

    assert_eq!(read.len(), 2);
    assert_eq!(read[0].record().message(), "first");
    assert_eq!(read[1].record().message(), "second");
    assert!(read[0].identity() < read[1].identity());
    Ok(())
}

#[tokio::test]
async fn a_read_after_an_identity_returns_only_later_records() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let batch = [record("first"), record("second"), record("third")];
    store.append(&batch, KEEP_EVERY).await?;

    let read = reads(&store)?.following(&LogQuery::newest(10).after(2))?;

    assert_eq!(read.len(), 1);
    assert_eq!(read[0].record().message(), "third");
    Ok(())
}

#[tokio::test]
async fn a_since_read_drops_records_recorded_earlier() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let older = LogRecord::new(
        1,
        "info",
        "rift_mcp",
        "index",
        "index.reconcile",
        "old",
        "{}",
    );
    let newer = LogRecord::new(
        9,
        "info",
        "rift_mcp",
        "index",
        "index.reconcile",
        "new",
        "{}",
    );
    store.append(&[older, newer], KEEP_EVERY).await?;

    let reads = reads(&store)?;
    let followed = reads.following(&LogQuery::newest(10).since_ms(9))?;
    let recent = reads.recent(&LogQuery::newest(10).since_ms(9))?;

    assert_eq!(followed.len(), 1);
    assert_eq!(followed[0].record().message(), "new");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].record().message(), "new");
    Ok(())
}

/// A record recorded at `recorded_at_ms`, of `kind`.
fn recorded(recorded_at_ms: i64, kind: RecordKind, message: &str) -> LogRecord {
    let record = LogRecord::new(
        recorded_at_ms,
        "info",
        "rift_tracing::metric",
        "",
        "process",
        message,
        "{}",
    );
    match kind {
        RecordKind::Log => record,
        RecordKind::Metric => record.into_metric(),
    }
}

/// The messages and kinds of `records`, in order.
fn kinds(records: &[crate::StoredLogRecord]) -> Vec<(&str, RecordKind)> {
    records
        .iter()
        .map(|stored| (stored.record().message(), stored.record().kind()))
        .collect()
}

#[tokio::test]
async fn a_window_holds_records_and_snapshots_in_time_order_and_only_its_kind() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store
        .append(
            &[
                recorded(99, RecordKind::Log, "before"),
                recorded(100, RecordKind::Log, "opened"),
                recorded(150, RecordKind::Metric, "snapshot"),
                recorded(199, RecordKind::Log, "closed"),
                recorded(200, RecordKind::Metric, "after"),
            ],
            KEEP_EVERY,
        )
        .await?;
    let reads = reads(&store)?;
    let window = LogQuery::newest(10).since_ms(100).until_ms(200);

    assert_eq!(
        kinds(&reads.following(&window.clone().of_every_kind())?),
        [
            ("opened", RecordKind::Log),
            ("snapshot", RecordKind::Metric),
            ("closed", RecordKind::Log),
        ]
    );
    assert_eq!(
        kinds(&reads.following(&window.clone())?),
        [("opened", RecordKind::Log), ("closed", RecordKind::Log)],
        "a read asks for log records unless it names a kind"
    );
    assert_eq!(
        kinds(&reads.following(&window.of_kind(RecordKind::Metric))?),
        [("snapshot", RecordKind::Metric)]
    );
    assert_eq!(
        kinds(&reads.recent(&LogQuery::newest(1).of_kind(RecordKind::Metric))?),
        [("after", RecordKind::Metric)]
    );
    Ok(())
}

#[tokio::test]
async fn a_window_pages_past_the_page_bound_and_starts_at_what_retention_kept() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let batch_records = i64::try_from(LOG_BATCH_RECORDS_MAX)?;
    let batch = |offset: i64| -> Vec<LogRecord> {
        (0..batch_records)
            .map(|index| recorded(offset + index, RecordKind::Log, "paged"))
            .collect()
    };
    let retention = (2 * LOG_BATCH_RECORDS_MAX - 100) as u64;
    store.append(&batch(0), retention).await?;
    store.append(&batch(batch_records), retention).await?;
    let reads = reads(&store)?;
    let window = LogQuery::newest(LOG_PAGE_RECORDS_MAX)
        .since_ms(0)
        .until_ms(i64::MAX);

    let mut read = Vec::new();
    let mut after = 0;
    for _ in 0..4 {
        let page = reads.following(&window.clone().after(after))?;
        let Some(last) = page.last() else {
            break;
        };
        after = last.identity();
        read.extend(page.iter().map(|stored| stored.record().recorded_at_ms()));
    }

    assert_eq!(
        read.len() as u64,
        retention,
        "the window reads every kept record"
    );
    assert!(read.len() > LOG_PAGE_RECORDS_MAX);
    assert_eq!(
        read.first(),
        Some(&100),
        "a window reaching before retention starts at what was kept"
    );
    assert!(read.windows(2).all(|pair| pair[0] < pair[1]));
    Ok(())
}

#[tokio::test]
async fn a_followed_read_keeps_the_level_and_component_filters() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let warning = LogRecord::new(1, "warn", "rift_mcp", "search", "search.open", "late", "{}");
    store
        .append(&[record("routine"), warning], KEEP_EVERY)
        .await?;

    let reads = reads(&store)?;
    let by_level = reads.following(&LogQuery::newest(10).at_level("warn"))?;
    let by_component = reads.following(&LogQuery::newest(10).for_component("search"))?;

    assert_eq!(by_level.len(), 1);
    assert_eq!(by_level[0].record().message(), "late");
    assert_eq!(by_component.len(), 1);
    assert_eq!(by_component[0].record().message(), "late");
    Ok(())
}

#[tokio::test]
async fn a_followed_page_bounds_itself() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let batch: Vec<LogRecord> = (0..5).map(|index| record(&index.to_string())).collect();
    store.append(&batch, KEEP_EVERY).await?;

    let read = reads(&store)?.following(&LogQuery::newest(2))?;

    assert_eq!(read.len(), 2);
    assert_eq!(read[0].record().message(), "0");
    Ok(())
}

#[tokio::test]
async fn an_oversized_batch_is_refused_whole() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let batch: Vec<LogRecord> = (0..=LOG_BATCH_RECORDS_MAX).map(|_| record("x")).collect();

    let refusal = store
        .append(&batch, KEEP_EVERY)
        .await
        .expect_err("an oversized batch must be refused");

    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_batch_limit::SLUG
    );
    assert_eq!(reads(&store)?.count()?, 0);
    Ok(())
}

#[tokio::test]
async fn an_empty_batch_changes_nothing() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;

    assert_eq!(store.append(&[], KEEP_EVERY).await?, 0);
    assert_eq!(reads(&store)?.count()?, 0);
    Ok(())
}

#[tokio::test]
async fn a_reopened_store_keeps_its_records() -> TestResult {
    let directory = tempfile::tempdir()?;
    let first = store(&directory).await?;
    first.append(&[record("survivor")], KEEP_EVERY).await?;
    first.close(close_deadline()).await?;

    let store = store(&directory).await?;

    let read = reads(&store)?.recent(&LogQuery::newest(10))?;
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].record().message(), "survivor");
    Ok(())
}

#[tokio::test]
async fn a_new_file_carries_the_schema_version() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;

    assert_eq!(user_version(store.path())?, METRICS_SCHEMA_VERSION);
    Ok(())
}

#[tokio::test]
async fn a_file_of_another_version_is_refused_by_a_reader_and_recreated_by_the_writer() -> TestResult
{
    let directory = tempfile::tempdir()?;
    let path = metrics_path(&directory);
    {
        let connection = rusqlite::Connection::open(&path)?;
        connection.execute_batch(
            "CREATE TABLE log_records(id INTEGER PRIMARY KEY, note TEXT);
             INSERT INTO log_records(note) VALUES ('kept by an older schema');
             PRAGMA user_version = 7;",
        )?;
    }

    let refusal = LogReader::new(&path)
        .connect()
        .expect_err("a reader refuses a schema it does not read");
    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_store_failed::SLUG
    );
    let rendered = refusal.to_string();
    assert!(
        rendered.contains("version 7")
            && rendered.contains(&format!("version {METRICS_SCHEMA_VERSION}")),
        "{rendered}"
    );

    let store = LogStore::open(&path, None).await?;
    assert_eq!(user_version(&path)?, METRICS_SCHEMA_VERSION);
    let reads = reads(&store)?;
    assert_eq!(reads.count()?, 0, "recreation discards the older rows");
    store.append(&[record("fresh")], KEEP_EVERY).await?;
    assert_eq!(
        reads.recent(&LogQuery::newest(10))?[0].record().message(),
        "fresh"
    );
    Ok(())
}

#[test]
fn a_reader_on_an_absent_path_creates_no_file() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let path = metrics_path(&directory);

    let refusal = LogReader::new(&path)
        .connect()
        .expect_err("an absent file cannot be read");

    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_store_failed::SLUG
    );
    assert!(!path.exists(), "a reader never creates the file");
}

#[test]
fn a_reader_on_a_file_without_a_schema_answers_no_records() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = metrics_path(&directory);
    assert_eq!(user_version(&path)?, 0);

    let reads = LogReader::new(&path).connect()?;

    assert!(reads.recent(&LogQuery::newest(10))?.is_empty());
    assert!(reads.following(&LogQuery::newest(10))?.is_empty());
    assert_eq!(reads.count()?, 0);
    Ok(())
}

#[tokio::test]
async fn the_writer_thread_is_named_and_releases_its_owner_before_the_close_answer() -> TestResult {
    let directory = tempfile::tempdir()?;
    let (owner, released) = release_probe();
    let weak = Arc::downgrade(&owner);
    let store = LogStore::open(&metrics_path(&directory), Some(owner)).await?;
    assert!(
        weak.upgrade().is_some(),
        "the thread holds the owner while open"
    );

    store.close(close_deadline()).await?;

    assert!(
        weak.upgrade().is_none(),
        "a received close answer means the owner is released"
    );
    assert_eq!(
        released.recv_timeout(THREAD_WAIT_MAX)?.as_deref(),
        Some("rift-db-metrics")
    );
    assert_eq!(
        store.close(close_deadline()).await?,
        store.close(close_deadline()).await?,
        "a second close answers the first one's checkpoint"
    );
    let refusal = store
        .append(&[record("late")], KEEP_EVERY)
        .await
        .expect_err("a closed store writes nothing");
    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_store_failed::SLUG
    );
    Ok(())
}

#[tokio::test]
async fn a_store_dropped_without_a_close_releases_its_owner() -> TestResult {
    let directory = tempfile::tempdir()?;
    let (owner, released) = release_probe();
    let store = LogStore::open(&metrics_path(&directory), Some(owner)).await?;

    drop(store);

    assert_eq!(
        released.recv_timeout(THREAD_WAIT_MAX)?.as_deref(),
        Some("rift-db-metrics"),
        "the thread ends on the closed queue and releases the owner"
    );
    Ok(())
}

#[tokio::test]
async fn an_open_creates_no_lock_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    let _store = store(&directory).await?;

    assert!(!directory.path().join("metrics.lock").exists());
    Ok(())
}

#[tokio::test]
async fn a_failed_open_releases_its_owner() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = metrics_path(&directory);
    std::fs::create_dir(&path)?;
    let (owner, released) = release_probe();

    let refusal = LogStore::open(&path, Some(owner))
        .await
        .expect_err("a directory is not a database");

    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_store_failed::SLUG
    );
    assert_eq!(
        released.recv_timeout(THREAD_WAIT_MAX)?.as_deref(),
        Some("rift-db-metrics")
    );
    Ok(())
}

#[tokio::test]
async fn a_close_removes_the_write_ahead_log() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("written")], KEEP_EVERY).await?;
    assert!(
        wal_path(store.path()).exists(),
        "an open WAL database keeps its log"
    );

    let checkpoint = store.close(close_deadline()).await?;

    assert!(!checkpoint.is_busy());
    assert_eq!(checkpoint.log(), checkpoint.checkpointed());
    assert!(
        !wal_path(store.path()).exists(),
        "the last connection's close deletes the WAL file"
    );
    Ok(())
}

#[tokio::test]
async fn a_close_beside_another_connection_leaves_an_empty_write_ahead_log() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("written")], KEEP_EVERY).await?;
    let other = reads(&store)?;
    assert_eq!(other.count()?, 1);

    let checkpoint = store.close(close_deadline()).await?;

    assert!(!checkpoint.is_busy());
    assert_eq!(std::fs::metadata(wal_path(store.path()))?.len(), 0);
    drop(other);
    Ok(())
}

#[tokio::test]
async fn a_restarted_write_ahead_log_is_cut_to_its_limit() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let long = "x".repeat(LOG_MESSAGE_BYTES_MAX);
    let batch: Vec<LogRecord> = (0..1_024).map(|_| record(&long)).collect();
    store.append(&batch, KEEP_EVERY * 4).await?;
    let grown = std::fs::metadata(wal_path(store.path()))?.len();
    assert!(
        grown > super::METRICS_JOURNAL_SIZE_LIMIT_BYTES as u64,
        "{grown}"
    );

    store.append(&[record("restart")], KEEP_EVERY * 4).await?;

    let kept = std::fs::metadata(wal_path(store.path()))?.len();
    assert!(
        kept <= super::METRICS_JOURNAL_SIZE_LIMIT_BYTES as u64,
        "the commit that restarts the WAL cuts it to the limit: {kept}"
    );
    Ok(())
}

/// A writer thread held inside an append keeps its owner past a close that missed its
/// deadline, and releases it only once the thread runs the close queued behind the append. The owner stands in
/// for the election guard the serving process hands the thread.
#[tokio::test]
async fn a_held_writer_keeps_its_owner_past_a_missed_close_deadline() -> TestResult {
    let directory = tempfile::tempdir()?;
    let (owner, released) = release_probe();
    let weak = Arc::downgrade(&owner);
    let store = LogStore::open(&metrics_path(&directory), Some(owner)).await?;
    let holder = rusqlite::Connection::open(metrics_path(&directory))?;
    holder.execute_batch("BEGIN IMMEDIATE")?;
    // Queued directly, so the append is ahead of the close in the writer's queue.
    let (reply, appended) = tokio::sync::oneshot::channel();
    store
        .sender
        .send(super::Command::Append {
            records: vec![record("held")],
            retention_records: KEEP_EVERY,
            reply,
        })
        .await
        .map_err(|_| "the writer thread accepts the append")?;

    let missed = store
        .close(Instant::now() + Duration::from_millis(50))
        .await;

    let missed = missed.expect_err("the close misses its deadline");
    let rendered = format!("{missed}: {}", rift_error::causes(&missed).join(": "));
    // The queue holds the append until the writer thread receives it, which races the
    // close request, so the depth the failure names is 0 or 1.
    assert!(
        rendered.contains("stage queued running for")
            && (rendered.contains("the queue held 0 of 1 commands at the close request")
                || rendered.contains("the queue held 1 of 1 commands at the close request")),
        "the missed close names the stage it waited in and the queue it waited behind: {rendered}"
    );
    assert!(
        weak.upgrade().is_some(),
        "the held thread keeps the owner past the missed deadline"
    );
    holder.execute_batch("ROLLBACK")?;
    appended.await??;
    // The missed close stayed queued behind the append, and the thread runs it next.
    assert_eq!(
        released.recv_timeout(THREAD_WAIT_MAX)?.as_deref(),
        Some("rift-db-metrics")
    );
    assert!(
        weak.upgrade().is_none(),
        "the close the thread ran released the owner"
    );
    Ok(())
}

#[tokio::test]
async fn a_reader_names_the_file_its_store_writes() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;

    assert_eq!(store.reader().path(), store.path());
    Ok(())
}

#[test]
fn a_command_debugs_with_its_record_count_and_no_reply() {
    let (reply, _answer) = tokio::sync::oneshot::channel();
    let append = super::Command::Append {
        records: vec![record("first"), record("second")],
        retention_records: 5,
        reply,
    };
    let (reply, _answer) = tokio::sync::oneshot::channel();
    let close = super::Command::Close { reply };

    assert_eq!(format!("{append:?}"), "Append { records: 2, .. }");
    assert_eq!(format!("{close:?}"), "Close { .. }");
}

#[tokio::test]
async fn an_append_the_database_refuses_names_the_step_that_failed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("first")], KEEP_EVERY).await?;
    rusqlite::Connection::open(store.path())?.execute_batch("DROP TABLE log_records")?;

    let refusal = store
        .append(&[record("second")], KEEP_EVERY)
        .await
        .expect_err("an append needs the table it writes");

    assert_eq!(
        refusal.slug(),
        rift_error::errors::tracing::log_store_failed::SLUG
    );
    let rendered = refusal.to_string();
    assert!(rendered.contains("read the newest identity"), "{rendered}");
    Ok(())
}
