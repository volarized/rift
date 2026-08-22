//! Integration coverage for [`rift_index::LexicalSearchIndex`] against a
//! file-backed `SQLite` database: WAL persistence, restart survival, and
//! concurrent-read isolation require a real file, not an in-memory database.

use std::path::{Path, PathBuf};

use rift_core::ProjectPath;
use rift_index::{
    LexicalIndexLimits, LexicalIndexViolation, LexicalMatch, LexicalSearchIndex, LexicalUnit,
    LexicalUnitKind,
};
use tempfile::TempDir;
use toasty::Db;
use toasty_driver_sqlite::Sqlite;

/// Builds one text-file unit; its identity is its own path, per convention.
fn text_unit(path: &str, content: &str) -> Result<LexicalUnit, Box<dyn std::error::Error>> {
    let project_path = ProjectPath::new(path)?;
    Ok(LexicalUnit::new(
        path,
        project_path,
        LexicalUnitKind::TextFile,
        None,
        content,
    )?)
}

/// Builds one symbol unit with an explicit declaration name.
fn symbol_unit(
    identity: &str,
    path: &str,
    name: &str,
    content: &str,
) -> Result<LexicalUnit, Box<dyn std::error::Error>> {
    let project_path = ProjectPath::new(path)?;
    let kind = LexicalUnitKind::Symbol;
    Ok(LexicalUnit::new(
        identity,
        project_path,
        kind,
        Some(name.to_owned()),
        content,
    )?)
}

fn database_path(directory: &TempDir) -> PathBuf {
    directory.path().join("lexical.db")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_and_search_multi_word_query_hits()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let units = [
        text_unit("docs/guide.md", "alpha configuration guide")?,
        text_unit("docs/other.md", "some other release notes")?,
    ];
    index.replace_all(&units, "revision-1").await?;

    let hits = index.search("alpha other", 10).await?;
    let identities: Vec<&str> = hits.iter().map(LexicalMatch::identity).collect();
    assert_eq!(
        identities.len(),
        2,
        "OR-joined terms must match either unit"
    );
    assert!(identities.contains(&"docs/guide.md"));
    assert!(identities.contains(&"docs/other.md"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_orders_better_match_first()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let units = [
        text_unit("docs/light.md", "beacon mentioned once")?,
        text_unit("docs/heavy.md", "beacon beacon beacon beacon beacon")?,
    ];
    index.replace_all(&units, "revision-1").await?;

    let hits = index.search("beacon", 10).await?;
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].identity(),
        "docs/heavy.md",
        "denser match must rank first"
    );
    assert!(
        hits[0].rank() <= hits[1].rank(),
        "rank must be ascending, best first"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_limit_and_matches_max_cap_results()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(100, 1_048_576, 32, 2, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, limits).await?;

    let mut units = Vec::new();
    for identifier in 0..5 {
        units.push(text_unit(&format!("docs/{identifier}.md"), "shared token")?);
    }
    index.replace_all(&units, "revision-1").await?;

    let capped_by_matches_max = index.search("shared", 10).await?;
    assert_eq!(
        capped_by_matches_max.len(),
        2,
        "matches_max must cap results"
    );

    let capped_by_limit = index.search("shared", 1).await?;
    assert_eq!(capped_by_limit.len(), 1, "explicit limit must cap results");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_empty_query_returns_empty()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let units = [text_unit("docs/a.md", "content")?];
    index.replace_all(&units, "revision-1").await?;

    let hits = index.search("   ", 10).await?;
    assert_eq!(hits, Vec::new());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_open_with_single_pool_slot_still_serves_search()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let limits = LexicalIndexLimits::new(100, 1_048_576, 32, 100, 1, 1_000);
    let index = LexicalSearchIndex::open(&path, limits).await?;

    let units = [text_unit("docs/a.md", "single slot content")?];
    index.replace_all(&units, "revision-1").await?;

    let hits = index.search("single", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "pool_slots must reach the builder and still serve reads"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_over_units_max_refuses_and_prior_state_still_served()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(1, 1_048_576, 32, 100, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, limits).await?;

    let first_batch = [text_unit("docs/kept.md", "kept content survives")?];
    index.replace_all(&first_batch, "revision-1").await?;

    let oversized_batch = [
        text_unit("docs/one.md", "one")?,
        text_unit("docs/two.md", "two")?,
    ];
    let outcome = index.replace_all(&oversized_batch, "revision-2").await;
    let error = outcome.expect_err("batch bound violation must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::UnitLimit);

    let hits = index.search("kept", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "refused batch must not disturb prior committed state"
    );
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_unit_over_bytes_max_refuses_naming_path()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(100, 8, 32, 100, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, limits).await?;

    let oversized = [text_unit("docs/big.md", "too many bytes here")?];
    let outcome = index.replace_all(&oversized, "revision-1").await;
    let error = outcome.expect_err("batch bound violation must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::UnitTooLarge
    );
    assert_eq!(error.fault().path(), Some(Path::new("docs/big.md")));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_duplicate_identity_refuses_atomically()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let baseline = [text_unit("docs/kept.md", "baseline content")?];
    index.replace_all(&baseline, "revision-1").await?;

    let first_copy = ProjectPath::new("docs/dup.md")?;
    let second_copy = ProjectPath::new("docs/dup-again.md")?;
    let kind = LexicalUnitKind::TextFile;
    let first_unit = LexicalUnit::new("docs/dup.md", first_copy, kind, None, "first")?;
    let second_unit = LexicalUnit::new("docs/dup.md", second_copy, kind, None, "second")?;
    let duplicated = [first_unit, second_unit];
    let outcome = index.replace_all(&duplicated, "revision-2").await;
    let error = outcome.expect_err("batch bound violation must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::DuplicateIdentity
    );

    assert_eq!(
        index.content("docs/kept.md").await?,
        Some("baseline content".to_owned())
    );
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_second_replace_all_fully_supersedes_first()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let first = [text_unit("docs/a.md", "firstwordonly content")?];
    index.replace_all(&first, "revision-1").await?;

    let second = [text_unit("docs/b.md", "secondwordonly content")?];
    index.replace_all(&second, "revision-2").await?;

    let stale_hits = index.search("firstwordonly", 10).await?;
    assert_eq!(stale_hits, Vec::new(), "old rows must be unfindable");
    let hits = index.search("secondwordonly", 10).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(index.tree_revision().await?, Some("revision-2".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_tree_revision_none_then_some_after_replace_all()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    assert_eq!(index.tree_revision().await?, None);

    let units = [text_unit("docs/a.md", "content")?];
    index.replace_all(&units, "revision-1").await?;
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_content_returns_some_and_none()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let units = [text_unit("docs/a.md", "found content")?];
    index.replace_all(&units, "revision-1").await?;

    assert_eq!(
        index.content("docs/a.md").await?,
        Some("found content".to_owned())
    );
    assert_eq!(index.content("docs/missing.md").await?, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_reopen_from_file_serves_persisted_rows()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);

    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;
    let units = [text_unit("docs/a.md", "persisted content")?];
    index.replace_all(&units, "revision-1").await?;
    drop(index);

    let reopened = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;
    assert_eq!(
        reopened.content("docs/a.md").await?,
        Some("persisted content".to_owned())
    );
    assert_eq!(
        reopened.tree_revision().await?,
        Some("revision-1".to_owned())
    );
    let hits = reopened.search("persisted", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "FTS rows must survive restart alongside typed rows"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_symbol_and_text_file_units_coexist()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let symbol = symbol_unit(
        "crate::widgets::render",
        "src/widgets.rs",
        "render",
        "fn render() { paint_surface() }",
    )?;
    let text = text_unit("docs/widgets.md", "widget documentation prose")?;
    index.replace_all(&[symbol, text], "revision-1").await?;

    let symbol_hits = index.search("paint_surface", 10).await?;
    assert_eq!(symbol_hits.len(), 1);
    assert_eq!(symbol_hits[0].kind(), LexicalUnitKind::Symbol);
    assert_eq!(symbol_hits[0].path().as_str(), "src/widgets.rs");
    assert_eq!(symbol_hits[0].name(), Some("render"));

    let text_hits = index.search("prose", 10).await?;
    assert_eq!(text_hits.len(), 1);
    assert_eq!(text_hits[0].kind(), LexicalUnitKind::TextFile);
    assert_eq!(text_hits[0].path().as_str(), "docs/widgets.md");
    assert_eq!(text_hits[0].name(), None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_name_match_outranks_body_only_match()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let named = symbol_unit(
        "crate::index::SearchHit",
        "src/index.rs",
        "SearchHit",
        "fn locate() { finds nothing relevant here }",
    )?;
    let bodied = symbol_unit(
        "crate::index::Unrelated",
        "src/index.rs",
        "Unrelated",
        "this function will search the entire tree",
    )?;
    index.replace_all(&[named, bodied], "revision-1").await?;

    let hits = index.search("search", 10).await?;
    assert_eq!(hits.len(), 2, "both name and body matches must be found");
    assert_eq!(
        hits[0].identity(),
        "crate::index::SearchHit",
        "name match must outrank body-only match"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_finds_camel_case_name_by_expanded_words()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let unit = symbol_unit(
        "crate::account::getUserName",
        "src/account.rs",
        "getUserName",
        "returns account holder identifier",
    )?;
    index.replace_all(&[unit], "revision-1").await?;

    let hits = index.search("user name", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "camelCase expansion must make the name discoverable by its split words"
    );
    assert_eq!(hits[0].identity(), "crate::account::getUserName");
    Ok(())
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_units"]
struct ConcurrentUnitRecord {
    #[key]
    identity: String,
    path: String,
    kind: String,
    name: Option<String>,
    byte_length: i64,
    content: String,
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_index_state"]
struct ConcurrentLexicalIndexStateRecord {
    #[key]
    id: i64,
    tree_revision: String,
}

async fn open_concurrent_probe(path: &Path) -> toasty::Result<Db> {
    let mut builder = Db::builder();
    let models = toasty::models!(ConcurrentUnitRecord, ConcurrentLexicalIndexStateRecord);
    builder.models(models).max_pool_size(1);
    builder.build(Sqlite::open(path)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_content_sees_only_committed_writes_during_concurrent_transaction()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let committed = [text_unit("docs/a.md", "alpha content")?];
    index.replace_all(&committed, "revision-1").await?;

    let probe_database = open_concurrent_probe(&path).await?;
    let mut probe_connection = probe_database.connection().await?;
    let mut probe_transaction = probe_connection.transaction().await?;
    toasty::create!(ConcurrentUnitRecord {
        identity: "docs/b.md",
        path: "docs/b.md",
        kind: "text_file",
        name: None,
        byte_length: 5,
        content: "bravo",
    })
    .exec(&mut probe_transaction)
    .await?;

    let uncommitted_read = index.content("docs/b.md").await?;
    assert_eq!(
        uncommitted_read, None,
        "reader must not see an uncommitted write"
    );

    probe_transaction.commit().await?;
    let committed_read = index.content("docs/b.md").await?;
    assert_eq!(
        committed_read,
        Some("bravo".to_owned()),
        "reader must see the write once committed"
    );
    Ok(())
}

async fn insert_corrupt_lexical_row(
    probe_database: &Db,
    identity: &str,
    path: &str,
    kind: &str,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut probe_connection = probe_database.connection().await?;
    toasty::create!(ConcurrentUnitRecord {
        identity: identity.to_owned(),
        path: path.to_owned(),
        kind: kind.to_owned(),
        name: None,
        byte_length: i64::try_from(content.len())?,
        content: content.to_owned(),
    })
    .exec(&mut probe_connection)
    .await?;
    toasty::sql::statement(
        "INSERT INTO lexical_units_fts(identity, name, content) VALUES (?1, ?2, ?3)",
    )
    .bind(identity.to_owned())
    .bind(String::new())
    .bind(content.to_owned())
    .exec(&mut probe_connection)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_stored_invalid_path_refuses()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let probe_database = open_concurrent_probe(&path).await?;
    insert_corrupt_lexical_row(
        &probe_database,
        "bad-path",
        "../outside.md",
        "text_file",
        "corrupt marker",
    )
    .await?;

    let outcome = index.search("corrupt", 10).await;
    let error = outcome.expect_err("a stored row with an invalid path must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::StoredPathInvalid
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_stored_invalid_kind_refuses()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await?;

    let probe_database = open_concurrent_probe(&path).await?;
    insert_corrupt_lexical_row(
        &probe_database,
        "bad-kind",
        "docs/bad.md",
        "not_a_real_kind",
        "corrupt marker",
    )
    .await?;

    let outcome = index.search("corrupt", 10).await;
    let error = outcome.expect_err("a stored row with an unknown kind must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::StoredKindInvalid
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_open_at_unusable_path_refuses_with_storage_failure()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("missing-parent").join("lexical.db");

    let outcome = LexicalSearchIndex::open(&path, LexicalIndexLimits::default()).await;
    let error = outcome.expect_err("opening under a missing parent directory must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
    assert_eq!(error.fault().path(), Some(path.as_path()));
    Ok(())
}
