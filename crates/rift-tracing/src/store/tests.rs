use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use tokio::time::Instant;

use super::{LogStore, METRICS_SCHEMA_VERSION, StoreClose, WalCheckpoint};
use crate::{
    LOG_BATCH_RECORDS_MAX, LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogReader,
    LogReads, LogRecord, SeriesValue,
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

/// Closes `store` by [`close_deadline`] and answers the checkpoint of a close that ended.
async fn closed(store: &LogStore) -> Result<WalCheckpoint, Box<dyn std::error::Error>> {
    match store.close(close_deadline()).await? {
        StoreClose::Closed(checkpoint) => Ok(checkpoint),
        timeout @ StoreClose::Timeout { .. } => Err(format!("the close ended: {timeout:?}").into()),
    }
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

/// A record recorded at `recorded_at_ms`.
fn recorded(recorded_at_ms: i64, message: &str) -> LogRecord {
    LogRecord::new(recorded_at_ms, "info", "rift", "", "process", message, "{}")
}

/// The messages of `records`, in order.
fn messages(records: &[crate::StoredLogRecord]) -> Vec<&str> {
    records
        .iter()
        .map(|stored| stored.record().message())
        .collect()
}

#[tokio::test]
async fn a_window_holds_the_records_recorded_inside_it_in_time_order() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store
        .append(
            &[
                recorded(99, "before"),
                recorded(100, "opened"),
                recorded(150, "inside"),
                recorded(199, "closed"),
                recorded(200, "after"),
            ],
            KEEP_EVERY,
        )
        .await?;
    let reads = reads(&store)?;
    let window = LogQuery::newest(10).since_ms(100).until_ms(200);

    assert_eq!(
        messages(&reads.following(&window)?),
        ["opened", "inside", "closed"]
    );
    assert_eq!(messages(&reads.recent(&LogQuery::newest(1))?), ["after"]);
    Ok(())
}

#[tokio::test]
async fn a_window_pages_past_the_page_bound_and_starts_at_what_retention_kept() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let batch_records = i64::try_from(LOG_BATCH_RECORDS_MAX)?;
    let batch = |offset: i64| -> Vec<LogRecord> {
        (0..batch_records)
            .map(|index| recorded(offset + index, "paged"))
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

/// The `sqlite_schema` rows a new metrics database holds, one block per row in name order,
/// spelled as the index database's schema fixture.
const METRICS_SCHEMA: &str = include_str!("../../tests/fixtures/metrics_schema.txt");

/// `fixture` with every line ending `\n`, as `SQLite` stores the schema text: a Windows
/// checkout may rewrite the fixture's endings to `\r\n`.
fn fixture_text(fixture: &str) -> String {
    fixture.replace("\r\n", "\n")
}

/// The schema rows of the file at `path`, rendered as [`METRICS_SCHEMA`] spells them.
fn rendered_schema(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let connection = rusqlite::Connection::open(path)?;
    let mut statement =
        connection.prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY name")?;
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

/// A new metrics database holds exactly its recorded table and indexes: `log_records`
/// with its `kind` column, and the `level` and `component` indexes.
#[tokio::test]
async fn a_new_file_holds_its_recorded_schema() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    closed(&store).await?;

    assert_eq!(rendered_schema(store.path())?, fixture_text(METRICS_SCHEMA));
    Ok(())
}

/// A fixture a Windows checkout rewrote to `\r\n` reads as the one with `\n` endings.
#[test]
fn a_crlf_checkout_of_the_schema_fixture_reads_as_written() {
    let crlf = METRICS_SCHEMA.replace("\r\n", "\n").replace('\n', "\r\n");
    assert_eq!(fixture_text(&crlf), fixture_text(METRICS_SCHEMA));
    assert!(!fixture_text(&crlf).contains('\r'));
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

/// A close runs no checkpoint: the write-ahead log keeps the committed frames, and the
/// next open moves them into the database and empties the log, with every record readable.
#[tokio::test]
async fn a_close_leaves_the_write_ahead_log_for_the_next_open() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("written")], KEEP_EVERY).await?;

    let checkpoint = closed(&store).await?;

    assert!(
        checkpoint.log() > checkpoint.checkpointed(),
        "the close leaves unmoved frames in the log: {checkpoint:?}"
    );
    let left = std::fs::metadata(wal_path(store.path()))?.len();
    assert!(
        left > 0,
        "the close keeps the write-ahead log: {left} bytes"
    );
    let path = store.path().to_path_buf();
    drop(store);

    let reopened = LogStore::open(&path, None).await?;
    assert_eq!(
        std::fs::metadata(wal_path(&path))?.len(),
        0,
        "the open's checkpoint empties the log"
    );
    assert_eq!(
        reads(&reopened)?.count()?,
        1,
        "the next open reads every record"
    );
    closed(&reopened).await?;
    Ok(())
}

/// A close beside another connection leaves the log to that connection, which still reads
/// every committed record.
#[tokio::test]
async fn a_close_beside_another_connection_keeps_its_records_readable() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("written")], KEEP_EVERY).await?;
    let other = reads(&store)?;

    closed(&store).await?;

    assert_eq!(other.count()?, 1);
    assert!(wal_path(store.path()).exists());
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

/// A close queued behind an append the writer thread is held inside answers a timeout
/// naming the queued stage, and the thread keeps its owner past it, releasing the owner
/// only once it runs the close queued behind the append. The owner stands in for the
/// election guard the serving process hands the thread.
#[tokio::test]
async fn a_close_queued_behind_a_held_writer_times_out_in_the_queued_stage() -> TestResult {
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
            queued: std::time::Instant::now(),
            reply,
        })
        .await
        .map_err(|_| "the writer thread accepts the append")?;

    let missed = store
        .close(Instant::now() + Duration::from_millis(50))
        .await?;

    let StoreClose::Timeout { stage, .. } = missed else {
        return Err(format!("the close misses its deadline: {missed:?}").into());
    };
    assert_eq!(stage, "queued", "the close names the stage it waited in");
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

/// Copies the metrics database and its write-ahead log, and no shared-memory index, into
/// `into`: the files a process leaves when it exits with its connection open.
fn copy_left_files(path: &Path, into: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let copied = into.join("metrics");
    std::fs::copy(path, &copied)?;
    std::fs::copy(wal_path(path), wal_path(&copied))?;
    Ok(copied)
}

/// A close the thread is held inside past its deadline answers a timeout naming the stage, and
/// the files the running writer thread leaves recover every committed record from the log
/// at the next open. The thread keeps its owner until it finishes.
#[tokio::test]
async fn a_close_past_its_deadline_times_out_and_the_next_open_recovers_the_log() -> TestResult {
    let directory = tempfile::tempdir()?;
    let (owner, released) = release_probe();
    let weak = Arc::downgrade(&owner);
    let store = LogStore::open(&metrics_path(&directory), Some(owner)).await?;
    let written: Vec<LogRecord> = (0..8)
        .map(|index| record(&format!("committed {index}")))
        .collect();
    store.append(&written, KEEP_EVERY).await?;
    let (holding, release) = store.hold_next_close();
    let deadline = Instant::now() + THREAD_WAIT_MAX;

    // The held thread answers nothing, so the paused clock reaches the deadline only once
    // the close waits inside its close stage.
    let (answer, held) = tokio::join!(store.close(deadline), async {
        let held = holding.await;
        tokio::time::pause();
        tokio::time::advance(THREAD_WAIT_MAX).await;
        held
    });

    held?;
    let StoreClose::Timeout { stage, elapsed } = answer? else {
        return Err("a held close outlasts the deadline".into());
    };
    // The thread's stage times run on the monotonic clock the paused clock does not move.
    assert_eq!(stage, "close", "{elapsed:?}");
    assert!(
        weak.upgrade().is_some(),
        "the held thread keeps its owner past the deadline"
    );
    let copies = tempfile::tempdir()?;
    let left = copy_left_files(store.path(), copies.path())?;
    let main_alone = copies.path().join("main-alone");
    std::fs::copy(&left, &main_alone)?;
    // The open's checkpoint moved the schema into the database file; the records written
    // after it live in the log alone.
    let rows: i64 = rusqlite::Connection::open(&main_alone)?.query_row(
        "SELECT COUNT(*) FROM log_records",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(rows, 0, "the committed records live in the log alone");
    let recovered = rusqlite::Connection::open(&left)?;
    let mut messages = recovered.prepare("SELECT message FROM log_records ORDER BY id")?;
    let messages = messages
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        messages,
        written
            .iter()
            .map(|record| record.message.clone())
            .collect::<Vec<_>>(),
        "the next open recovers every committed record"
    );
    tokio::time::resume();
    release.send(())?;
    assert_eq!(
        released.recv_timeout(THREAD_WAIT_MAX)?.as_deref(),
        Some("rift-db-metrics"),
        "the thread finishes the close and releases its owner"
    );
    assert_eq!(
        store.close(close_deadline()).await?,
        StoreClose::Timeout { stage, elapsed },
        "a second close answers what the first answered"
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
        queued: std::time::Instant::now(),
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

/// One append records the metrics database's queue wait, write lock wait, commit, and
/// transaction, and a collection reads its queue length and file size; once the store
/// drops, a collection reads neither.
#[tokio::test]
async fn an_append_records_the_metrics_database_signals() -> TestResult {
    let (recorder, _drain) = crate::ScopedRecorder::builder().install()?;
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("measured")], KEEP_EVERY).await?;
    let metrics = recorder.metrics();

    let metrics_namespace = ("db.namespace", "metrics");
    let observed = |name: &str, labels: &[(&str, &str)]| match metrics
        .find(name, labels)
        .map(crate::MetricSeries::value)
    {
        Some(SeriesValue::Buckets { count, .. }) => *count,
        _ => 0,
    };
    let append = [metrics_namespace, ("db.operation.name", "append")];
    assert_eq!(observed("sqlite.queue.wait.duration", &append), 1);
    assert_eq!(
        observed("sqlite.write_lock.wait.duration", &[metrics_namespace]),
        1
    );
    assert_eq!(observed("sqlite.commit.duration", &[metrics_namespace]), 1);
    let committed = [metrics_namespace, ("sqlite.transaction.result", "commit")];
    assert_eq!(observed("sqlite.transaction.duration", &committed), 1);
    assert_eq!(
        metrics
            .find("sqlite.queue.length", &[metrics_namespace])
            .map(crate::MetricSeries::value),
        Some(&SeriesValue::Sum(0.0))
    );
    let database_file = [metrics_namespace, ("sqlite.file.type", "database")];
    assert!(
        matches!(
            metrics.find("sqlite.file.size", &database_file).map(crate::MetricSeries::value),
            Some(SeriesValue::Sum(size)) if *size > 0.0
        ),
        "{metrics:?}"
    );
    drop(store);
    let closed = recorder.metrics();
    assert!(closed.find("sqlite.file.size", &database_file).is_none());
    assert!(
        closed
            .find("sqlite.queue.length", &[metrics_namespace])
            .is_none()
    );
    Ok(())
}

/// The close records each statement it runs and the connection's close as one
/// `db.client.operation.duration` point named for it.
#[tokio::test]
async fn the_close_records_each_statement_it_runs() -> TestResult {
    let (recorder, _drain) = crate::ScopedRecorder::builder().install()?;
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("written")], KEEP_EVERY).await?;

    closed(&store).await?;
    let metrics = recorder.metrics();

    // The open runs the truncate checkpoint; the close reads the frames and closes.
    for statement in [
        "PRAGMA wal_checkpoint(TRUNCATE)",
        "PRAGMA wal_checkpoint(NOOP)",
        "close",
    ] {
        let labels = [
            ("db.system.name", "sqlite"),
            ("db.namespace", "metrics"),
            ("db.operation.name", statement),
        ];
        assert!(
            matches!(
                metrics
                    .find("db.client.operation.duration", &labels)
                    .map(crate::MetricSeries::value),
                Some(SeriesValue::Buckets { count: 1, .. })
            ),
            "{statement}: {metrics:?}"
        );
    }
    Ok(())
}

/// The busy handler sleeps `SQLite`'s own steps, so its whole wait is the busy timeout,
/// and gives up on the call past it.
#[test]
fn the_busy_handler_waits_the_busy_timeout_in_sqlites_steps() {
    let steps: Vec<u64> = (0..)
        .map_while(|count| super::busy_delay(count, super::METRICS_BUSY_TIMEOUT_MS))
        .map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
        .collect();
    assert_eq!(
        steps[..12],
        [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100],
        "the first steps are SQLite's delays table"
    );
    assert_eq!(steps.iter().sum::<u64>(), super::METRICS_BUSY_TIMEOUT_MS);
    assert_eq!(
        steps.last(),
        Some(&72),
        "the last step is cut to the timeout"
    );
    assert_eq!(super::busy_delay(-1, super::METRICS_BUSY_TIMEOUT_MS), None);
    assert_eq!(super::busy_delay(0, 0), None);
}

/// An append that meets another connection's write lock counts each busy handler call
/// in `sqlite.busy.retries` and commits once the lock is released.
#[tokio::test]
async fn an_append_behind_another_writer_counts_its_busy_retries() -> TestResult {
    let (recorder, _drain) = crate::ScopedRecorder::builder().install()?;
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    let other = rusqlite::Connection::open(store.path())?;
    other.execute_batch("BEGIN IMMEDIATE")?;
    let retries = || {
        let metrics = recorder.metrics();
        match metrics
            .find("sqlite.busy.retries", &[("db.namespace", "metrics")])
            .map(crate::MetricSeries::value)
        {
            Some(SeriesValue::Sum(count)) => *count,
            _ => 0.0,
        }
    };
    let before = retries();

    let behind = [record("behind")];
    let append = store.append(&behind, KEEP_EVERY);
    let mut append = std::pin::pin!(append);
    let waited = Instant::now() + THREAD_WAIT_MAX;
    while retries() <= before {
        assert!(Instant::now() < waited, "the append never met the lock");
        tokio::select! {
            appended = &mut append => return Err(format!("the append passed the lock: {appended:?}").into()),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    other.execute_batch("COMMIT")?;
    append.await?;

    assert!(retries() > before);
    assert_eq!(reads(&store)?.count()?, 1);
    Ok(())
}

/// A collection reads the metrics database's pages in use and on its freelist from the
/// file's header, as `SQLite` counts them once a checkpoint wrote them into the file; once
/// the store drops, a collection reads none.
#[tokio::test]
async fn a_collection_reads_the_metrics_database_page_counts() -> TestResult {
    let (recorder, _drain) = crate::ScopedRecorder::builder().install()?;
    let directory = tempfile::tempdir()?;
    let store = store(&directory).await?;
    store.append(&[record("counted")], KEEP_EVERY).await?;
    let (used, free) = {
        let connection = rusqlite::Connection::open(store.path())?;
        connection.busy_timeout(Duration::from_secs(1))?;
        let busy: i64 =
            connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        assert_eq!(busy, 0, "the checkpoint met no lock");
        let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let free: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        (pages - free, free)
    };
    let metrics = recorder.metrics();

    let state = |state: &'static str| [("db.namespace", "metrics"), ("sqlite.page.state", state)];
    #[expect(clippy::cast_precision_loss, reason = "a test file holds few pages")]
    let expected = |pages: i64| SeriesValue::Sum(pages as f64);
    let reported_used = metrics
        .find("sqlite.page.count", &state("used"))
        .ok_or("the used pages are reported")?;
    assert_eq!(reported_used.unit(), "{page}");
    assert_eq!(reported_used.value(), &expected(used));
    assert!(used > 0, "the store holds its tables");
    assert_eq!(
        metrics
            .find("sqlite.page.count", &state("free"))
            .map(crate::MetricSeries::value),
        Some(&expected(free))
    );
    drop(store);
    assert!(
        recorder
            .metrics()
            .find("sqlite.page.count", &state("used"))
            .is_none(),
        "a dropped store reports no pages"
    );
    Ok(())
}
