//! The one pool every store in `.rift/db` shares, and what sharing it proves.
//!
//! Both suites here are regressions. A log write that met an index commit came
//! back `database is locked` while the stores held pools of their own, and the
//! store that opened the file first configured that pool with its own bounds,
//! leaving the index committing against `units_max = 1`.

use std::sync::Arc;

use rift_core::ProjectPath;
use rift_index::{
    DatabasePool, LexicalIndexLimits, LexicalSearchIndex, LexicalUnit, LexicalUnitKind, LogQuery,
    LogRecord, LogStore, WorkspaceDatabase,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Units one commit carries, enough that the write holds its lock while the
/// log append asks for the same file.
const COMMIT_UNITS: usize = 2_000;
/// Records the log side writes against that commit.
const LOG_BATCHES: usize = 8;
/// Retention no suite here reaches.
const KEEP_EVERY: u64 = 100_000;

/// The pooled-connection bounds these suites open the database with.
fn database_pool() -> DatabasePool {
    DatabasePool::new(4, 15_000)
}

/// Index bounds wide enough for [`COMMIT_UNITS`].
fn index_limits() -> LexicalIndexLimits {
    LexicalIndexLimits::new(10_000, 1 << 20, 32, 64, 4, 15_000)
}

fn unit(index: usize) -> Result<LexicalUnit, Box<dyn std::error::Error>> {
    Ok(LexicalUnit::new(
        format!("rift://symbol/rust/unit_{index}.rs/declaration"),
        ProjectPath::new(format!("unit_{index}.rs"))?,
        LexicalUnitKind::Symbol,
        Some(format!("declaration_{index}")),
        format!("fn declaration_{index}() -> u32 {{ {index} }}"),
    )?)
}

fn record(message: &str) -> LogRecord {
    LogRecord::new(
        1,
        "info",
        "rift_index::tests",
        "index",
        "index.commit",
        message,
        "{}",
    )
}

#[tokio::test]
async fn a_log_append_lands_while_the_index_commits_to_the_same_file() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = WorkspaceDatabase::open(&directory.path().join("db"), database_pool()).await?;
    let index = LexicalSearchIndex::attached(Arc::clone(&database), index_limits());
    let logs = LogStore::attached(Arc::clone(&database));
    let units: Vec<LexicalUnit> = (0..COMMIT_UNITS).map(unit).collect::<Result<_, _>>()?;

    let commit = tokio::spawn(async move { index.replace_all(&units, "revision").await });
    let mut appended = 0;
    for batch in 0..LOG_BATCHES {
        logs.append(&[record(&format!("batch {batch}"))], KEEP_EVERY)
            .await?;
        appended += 1;
    }
    commit.await??;

    assert_eq!(appended, LOG_BATCHES);
    assert_eq!(logs.count().await?, LOG_BATCHES as u64);
    let read = logs.recent(&LogQuery::newest(LOG_BATCHES)).await?;
    assert_eq!(read.len(), LOG_BATCHES);
    Ok(())
}

#[tokio::test]
async fn a_log_store_attached_first_leaves_the_index_its_own_bounds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let database = WorkspaceDatabase::open(&directory.path().join("db"), database_pool()).await?;
    let _logs = LogStore::attached(Arc::clone(&database));
    let index = LexicalSearchIndex::attached(Arc::clone(&database), index_limits());
    let units: Vec<LexicalUnit> = (0..COMMIT_UNITS).map(unit).collect::<Result<_, _>>()?;

    let committed = index.replace_all(&units, "revision").await;

    assert!(
        committed.is_ok(),
        "the log store must not narrow the index's bounds: {committed:?}"
    );
    Ok(())
}
