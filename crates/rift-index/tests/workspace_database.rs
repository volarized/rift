//! The one pool every store in `.rift/db` shares, and what sharing it proves.
//!
//! Both suites here are regressions. A log write that met an index commit came
//! back `database is locked` while the stores held pools of their own, and the
//! store that opened the file first configured that pool with its own bounds,
//! leaving the index committing against `units_max = 1`.

use std::sync::Arc;

use rift_core::ProjectPath;
use rift_index::{
    DatabasePool, LexicalIndexLimits, LexicalSearchIndex, LogQuery, LogRecord, LogStore,
    WorkspaceDatabase,
};
use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument,
    SearchableField,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Documents one commit carries, enough that the write holds its lock while the
/// log append asks for the same file.
const COMMIT_DOCUMENTS: usize = 2_000;
/// Records the log side writes against that commit.
const LOG_BATCHES: usize = 8;
/// Retention no suite here reaches.
const KEEP_EVERY: u64 = 100_000;

/// The pooled-connection bounds these suites open the database with.
fn database_pool() -> DatabasePool {
    DatabasePool::new(4, 15_000)
}

/// Index bounds wide enough for [`COMMIT_DOCUMENTS`].
fn index_limits() -> LexicalIndexLimits {
    LexicalIndexLimits::new(10_000, 1 << 20, 64, 4, 15_000)
}

fn document(index: usize) -> Result<IndexDocument, Box<dyn std::error::Error>> {
    let name = format!("declaration_{index}");
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name.clone())
        .with(SearchableField::QualifiedName, format!("crate::{name}"))
        .with(
            SearchableField::DeclarationSource,
            format!("fn {name}() -> u32 {{ {index} }}"),
        );
    let digest = fields.digest();
    Ok(IndexDocument::new(
        DocumentIdentity::new(format!("rift://symbol/rust/unit_{index}.rs/declaration"))?,
        DocumentLocation::Project(ProjectPath::new(format!("unit_{index}.rs"))?),
        DocumentKind::Symbol,
        digest,
        fields,
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
    let documents: Vec<IndexDocument> = (0..COMMIT_DOCUMENTS)
        .map(document)
        .collect::<Result<_, _>>()?;

    let commit = tokio::spawn(async move { index.replace_all(&documents, "revision").await });
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
    let documents: Vec<IndexDocument> = (0..COMMIT_DOCUMENTS)
        .map(document)
        .collect::<Result<_, _>>()?;

    let committed = index.replace_all(&documents, "revision").await;

    assert!(
        committed.is_ok(),
        "the log store must not narrow the index's bounds: {committed:?}"
    );
    Ok(())
}
