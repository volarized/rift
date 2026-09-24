//! Integration coverage for [`rift_index::LexicalSearchIndex`] against a
//! file-backed `SQLite` database: WAL persistence, restart survival, and
//! concurrent-read isolation require a real file, not an in-memory database.

use std::path::{Path, PathBuf};

use rift_core::{ErrorCode, ErrorName, ProjectPath, SourceUnitId};
use rift_index::{DatabasePool, FileDigest, WorkspaceDatabase, WorkspaceDigests};
use rift_index::{
    LexicalChange, LexicalIndexLimits, LexicalIndexViolation, LexicalMatch, LexicalRanking,
    LexicalSearchIndex, LexicalStamp, RevisionScoped,
};
use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, FieldSet,
    IDENTIFIER_TERMS_BYTES_MAX, IndexDocument, ParsedQuery, QueryPhase, SearchableField,
    identifier_terms,
};
use tempfile::TempDir;
use toasty::Db;
use toasty::stmt::Type;
use toasty_core::driver::operation::TransactionMode;
use toasty_driver_sqlite::Sqlite;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The ranking one revision-qualified search returned under `phase`, refusing any answer
/// the store could not place under `tree_revision`.
async fn phase_ranking(
    index: &LexicalSearchIndex,
    tree_revision: &str,
    query: &str,
    phase: QueryPhase,
    limit: u32,
) -> Result<LexicalRanking, Box<dyn std::error::Error>> {
    let parsed = ParsedQuery::parse(query)?;
    match index.search(tree_revision, &parsed, phase, limit).await? {
        RevisionScoped::Matched(ranking) => Ok(ranking),
        other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
    }
}

/// The matches one revision-qualified search returned under `phase`, best first.
async fn phase_matches(
    index: &LexicalSearchIndex,
    tree_revision: &str,
    query: &str,
    phase: QueryPhase,
    limit: u32,
) -> Result<Vec<LexicalMatch>, Box<dyn std::error::Error>> {
    Ok(phase_ranking(index, tree_revision, query, phase, limit)
        .await?
        .into_matches())
}

/// The ranking the precise phase returned: every member required, which is the phase
/// every reader runs first.
async fn search_ranking(
    index: &LexicalSearchIndex,
    tree_revision: &str,
    query: &str,
    limit: u32,
) -> Result<LexicalRanking, Box<dyn std::error::Error>> {
    phase_ranking(index, tree_revision, query, QueryPhase::Precise, limit).await
}

/// The matches the precise phase returned, best first.
async fn search_matches(
    index: &LexicalSearchIndex,
    tree_revision: &str,
    query: &str,
    limit: u32,
) -> Result<Vec<LexicalMatch>, Box<dyn std::error::Error>> {
    Ok(search_ranking(index, tree_revision, query, limit)
        .await?
        .into_matches())
}

/// One document identity, refusing a spelling the shape would not accept.
fn identity(value: &str) -> Result<DocumentIdentity, Box<dyn std::error::Error>> {
    Ok(DocumentIdentity::new(value)?)
}

/// Builds one project document from an explicit field set, so a suite can state which
/// column carries the term it searches for.
fn document(
    identity: &str,
    path: &str,
    kind: DocumentKind,
    fields: DocumentFields,
) -> Result<IndexDocument, Box<dyn std::error::Error>> {
    let digest = fields.digest();
    Ok(IndexDocument::new(
        DocumentIdentity::new(identity)?,
        DocumentLocation::Project(ProjectPath::new(path)?),
        kind,
        digest,
        fields,
    )?)
}

/// Builds one text-file document under an explicit identity: the final path segment with
/// its extension in `name`, that name's split words beside it, and the text in
/// `file_content`. A text file declares nothing, so every declaration field stays absent.
fn text_chunk(
    identity: &str,
    path: &str,
    content: &str,
) -> Result<IndexDocument, Box<dyn std::error::Error>> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(
            SearchableField::IdentifierTerms,
            identifier_terms([name], IDENTIFIER_TERMS_BYTES_MAX),
        )
        .with(SearchableField::FileContent, content);
    document(identity, path, DocumentKind::TextFile, fields)
}

/// Builds one whole text-file document; its identity is its own path, per convention.
fn text_document(path: &str, content: &str) -> Result<IndexDocument, Box<dyn std::error::Error>> {
    text_chunk(path, path, content)
}

/// Builds one symbol document carrying a declaration name, that name's split words, and
/// its declaration source.
fn symbol_document(
    identity: &str,
    path: &str,
    name: &str,
    declaration_source: &str,
) -> Result<IndexDocument, Box<dyn std::error::Error>> {
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, name)
        .with(
            SearchableField::IdentifierTerms,
            identifier_terms([name], IDENTIFIER_TERMS_BYTES_MAX),
        )
        .with(SearchableField::DeclarationSource, declaration_source);
    document(identity, path, DocumentKind::Symbol, fields)
}

/// The pooled-connection bounds every suite here opens the database with.
fn database_pool() -> DatabasePool {
    DatabasePool::new(4, 1_000)
}

fn database_path(directory: &TempDir) -> PathBuf {
    directory.path().join("lexical.db")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_precise_requires_every_term_and_broad_widens_them() -> TestResult
{
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [
        text_document("docs/guide.md", "alpha configuration guide")?,
        text_document("docs/other.md", "some other release notes")?,
    ];
    index.replace_all(&documents, "revision-1").await?;

    let precise =
        phase_matches(&index, "revision-1", "alpha other", QueryPhase::Precise, 10).await?;
    assert_eq!(
        precise,
        Vec::new(),
        "the precise phase requires every term, and no document carries both"
    );

    let broad = phase_matches(&index, "revision-1", "alpha other", QueryPhase::Broad, 10).await?;
    let identities: Vec<&str> = broad.iter().map(|hit| hit.identity().as_str()).collect();
    assert_eq!(
        identities.len(),
        2,
        "the broad phase widens the unquoted terms and reaches either document"
    );
    assert!(identities.contains(&"docs/guide.md"));
    assert!(identities.contains(&"docs/other.md"));

    let carried = phase_matches(
        &index,
        "revision-1",
        "alpha configuration",
        QueryPhase::Precise,
        10,
    )
    .await?;
    assert_eq!(
        carried.len(),
        1,
        "the precise phase answers the document carrying every term"
    );
    assert_eq!(carried[0].identity().as_str(), "docs/guide.md");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_a_quoted_phrase_stays_one_phrase_in_both_phases() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [
        text_document("docs/adjacent.md", "the alpha beacon reports nightly")?,
        text_document("docs/apart.md", "beacon first and alpha second")?,
    ];
    index.replace_all(&documents, "revision-1").await?;

    for phase in [QueryPhase::Precise, QueryPhase::Broad] {
        let hits = phase_matches(&index, "revision-1", "\"alpha beacon\"", phase, 10).await?;
        let label = phase.label();
        assert_eq!(
            hits.len(),
            1,
            "a quoted phrase reaches adjacent words alone: phase={label}"
        );
        assert_eq!(hits[0].identity().as_str(), "docs/adjacent.md");
        assert_eq!(hits[0].fields(), FieldSet::of(SearchableField::FileContent));
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_names_the_column_that_carried_the_term() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let named = document(
        "crate::beacon",
        "src/beacon.rs",
        DocumentKind::Symbol,
        DocumentFields::empty()
            .with(SearchableField::Name, "beacon")
            .with(SearchableField::DeclarationSource, "pub fn declare() {}"),
    )?;
    let documented = document(
        "crate::relay",
        "src/relay.rs",
        DocumentKind::Symbol,
        DocumentFields::empty()
            .with(SearchableField::Name, "relay")
            .with(
                SearchableField::Documentation,
                "forwards every beacon it receives",
            )
            .with(SearchableField::DeclarationSource, "pub fn relay() {}"),
    )?;
    index
        .replace_all(&[named, documented], "revision-1")
        .await?;

    let hits = search_matches(&index, "revision-1", "beacon", 10).await?;
    assert_eq!(hits.len(), 2, "both documents carry the term somewhere");
    let name_hit = hits
        .iter()
        .find(|hit| hit.identity().as_str() == "crate::beacon")
        .ok_or("the name hit must be ranked")?;
    let documentation_hit = hits
        .iter()
        .find(|hit| hit.identity().as_str() == "crate::relay")
        .ok_or("the documentation hit must be ranked")?;
    assert_eq!(name_hit.fields(), FieldSet::of(SearchableField::Name));
    assert_eq!(
        documentation_hit.fields(),
        FieldSet::of(SearchableField::Documentation)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_reaches_a_document_through_any_one_field() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [
        document(
            "crate::dispatch",
            "src/dispatch.rs",
            DocumentKind::Symbol,
            DocumentFields::empty()
                .with(SearchableField::Name, "dispatch")
                .with(
                    SearchableField::Signature,
                    "fn dispatch(payload: Envelope) -> Receipt",
                )
                .with(SearchableField::DeclarationSource, "pub fn dispatch() {}"),
        )?,
        document(
            "crate::collect",
            "src/collect.rs",
            DocumentKind::Symbol,
            DocumentFields::empty()
                .with(SearchableField::Name, "collect")
                .with(
                    SearchableField::Documentation,
                    "drains the mailbox before it returns",
                )
                .with(SearchableField::DeclarationSource, "pub fn collect() {}"),
        )?,
        document(
            "crate::render",
            "src/render.rs",
            DocumentKind::Symbol,
            DocumentFields::empty()
                .with(SearchableField::Name, "render")
                .with(
                    SearchableField::DeclarationSource,
                    "pub fn render() { paint_surface() }",
                ),
        )?,
        text_document("docs/notes.md", "the quarterly retrospective lives here")?,
    ];
    index.replace_all(&documents, "revision-1").await?;

    for (query, expected, field) in [
        ("envelope", "crate::dispatch", SearchableField::Signature),
        ("mailbox", "crate::collect", SearchableField::Documentation),
        (
            "paint_surface",
            "crate::render",
            SearchableField::DeclarationSource,
        ),
        (
            "retrospective",
            "docs/notes.md",
            SearchableField::FileContent,
        ),
    ] {
        let hits = search_matches(&index, "revision-1", query, 10).await?;
        assert_eq!(
            hits.len(),
            1,
            "one field alone must reach its document: query={query}"
        );
        assert_eq!(hits[0].identity().as_str(), expected);
        let column = field.column();
        assert_eq!(
            hits[0].fields(),
            FieldSet::of(field),
            "only {column} carried the term: query={query}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_orders_better_match_first() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [
        text_document("docs/light.md", "beacon mentioned once")?,
        text_document("docs/heavy.md", "beacon beacon beacon beacon beacon")?,
    ];
    index.replace_all(&documents, "revision-1").await?;

    let hits = search_matches(&index, "revision-1", "beacon", 10).await?;
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].identity().as_str(),
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
async fn test_lexical_search_index_search_limit_and_matches_max_cap_results() -> TestResult {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(100, 1_048_576, 2, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        limits,
    );

    let mut documents = Vec::new();
    for counter in 0..5 {
        documents.push(text_document(
            &format!("docs/{counter}.md"),
            "shared token",
        )?);
    }
    index.replace_all(&documents, "revision-1").await?;

    let capped_by_matches_max = search_matches(&index, "revision-1", "shared", 10).await?;
    assert_eq!(
        capped_by_matches_max.len(),
        2,
        "matches_max must cap results"
    );

    let capped_by_limit = search_matches(&index, "revision-1", "shared", 1).await?;
    assert_eq!(capped_by_limit.len(), 1, "explicit limit must cap results");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_names_the_bound_a_match_lies_past() -> TestResult {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(100, 1_048_576, 2, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        limits,
    );

    let at_the_bound = [
        text_document("docs/0.md", "shared token")?,
        text_document("docs/1.md", "shared token")?,
    ];
    index.replace_all(&at_the_bound, "revision-1").await?;
    let whole = search_ranking(&index, "revision-1", "shared", 10).await?;
    assert_eq!(whole.matches().len(), 2);
    assert_eq!(
        whole.truncated_at(),
        None,
        "exactly matches_max matches is not truncated"
    );

    let past_the_bound = [
        text_document("docs/0.md", "shared token")?,
        text_document("docs/1.md", "shared token")?,
        text_document("docs/2.md", "shared token")?,
    ];
    index.replace_all(&past_the_bound, "revision-2").await?;
    let cut = search_ranking(&index, "revision-2", "shared", 10).await?;
    assert_eq!(cut.matches().len(), 2, "the answer keeps matches_max rows");
    assert_eq!(
        cut.truncated_at(),
        Some(2),
        "one match past matches_max names the bound"
    );

    let cut_by_limit = search_ranking(&index, "revision-2", "shared", 1).await?;
    assert_eq!(cut_by_limit.matches().len(), 1);
    assert_eq!(
        cut_by_limit.truncated_at(),
        Some(1),
        "a limit below matches_max is the bound that cut"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_empty_query_returns_empty() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [text_document("docs/a.md", "content")?];
    index.replace_all(&documents, "revision-1").await?;

    let hits = search_matches(&index, "revision-1", "   ", 10).await?;
    assert_eq!(hits, Vec::new());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_open_with_single_pool_slot_still_serves_search() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let limits = LexicalIndexLimits::new(100, 1_048_576, 100, 1, 1_000);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        limits,
    );

    let documents = [text_document("docs/a.md", "single slot content")?];
    index.replace_all(&documents, "revision-1").await?;

    let hits = search_matches(&index, "revision-1", "single", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "pool_slots must reach the builder and still serve reads"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_over_units_max_refuses_and_prior_state_still_served()
-> TestResult {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(1, 1_048_576, 100, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        limits,
    );

    let first_batch = [text_document("docs/kept.md", "kept content survives")?];
    index.replace_all(&first_batch, "revision-1").await?;

    let oversized_batch = [
        text_document("docs/one.md", "one")?,
        text_document("docs/two.md", "two")?,
    ];
    let outcome = index.replace_all(&oversized_batch, "revision-2").await;
    let error = outcome.expect_err("batch bound violation must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::UnitLimit);

    let hits = search_matches(&index, "revision-1", "kept", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "refused batch must not disturb prior committed state"
    );
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_content_over_bytes_max_refuses_naming_path()
-> TestResult {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(100, 8, 100, 4, 1_000);
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        limits,
    );

    let oversized = [text_document("docs/big.md", "too many bytes here")?];
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
async fn test_lexical_search_index_replace_all_refuses_a_document_addressed_by_a_source_unit()
-> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index
        .replace_all(
            &[text_document("docs/kept.md", "kept content")?],
            "revision-1",
        )
        .await?;

    // This store holds project documents. A document addressed by a source unit belongs
    // to a package index, and filing its unit URI in the path column would make the
    // stored row unreadable as a project path.
    let unit = SourceUnitId::parse("rift://source/cargo/helper@0.1.0/src/lib.rs")?;
    let fields = DocumentFields::empty().with(SearchableField::Name, "helper");
    let digest = fields.digest();
    let packaged = IndexDocument::new(
        identity("rift://source/cargo/helper@0.1.0/src/lib.rs")?,
        DocumentLocation::Unit(unit),
        DocumentKind::Symbol,
        digest,
        fields,
    )?;

    let error = index
        .replace_all(&[packaged], "revision-2")
        .await
        .expect_err("a document addressed by a source unit must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::DocumentLocationUnsupported
    );
    assert_eq!(
        index.tree_revision().await?,
        Some("revision-1".to_owned()),
        "the refusal lands before any transaction opens"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_duplicate_identity_refuses_atomically() -> TestResult
{
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let baseline = [text_document("docs/kept.md", "baseline content")?];
    index.replace_all(&baseline, "revision-1").await?;

    let first_copy = text_chunk("docs/dup.md", "docs/dup.md", "first")?;
    let second_copy = text_chunk("docs/dup.md", "docs/dup-again.md", "second")?;
    let duplicated = [first_copy, second_copy];
    let outcome = index.replace_all(&duplicated, "revision-2").await;
    let error = outcome.expect_err("batch bound violation must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::DuplicateIdentity
    );

    assert_eq!(
        index.content(&identity("docs/kept.md")?).await?,
        Some("baseline content".to_owned())
    );
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_second_replace_all_fully_supersedes_first() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let first = [text_document("docs/a.md", "firstwordonly content")?];
    index.replace_all(&first, "revision-1").await?;

    let second = [text_document("docs/b.md", "secondwordonly content")?];
    index.replace_all(&second, "revision-2").await?;

    let stale_hits = search_matches(&index, "revision-2", "firstwordonly", 10).await?;
    assert_eq!(stale_hits, Vec::new(), "old rows must be unfindable");
    let hits = search_matches(&index, "revision-2", "secondwordonly", 10).await?;
    assert_eq!(hits.len(), 1);
    assert_eq!(index.tree_revision().await?, Some("revision-2".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_tree_revision_none_then_some_after_replace_all() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    assert_eq!(index.tree_revision().await?, None);

    let documents = [text_document("docs/a.md", "content")?];
    index.replace_all(&documents, "revision-1").await?;
    assert_eq!(index.tree_revision().await?, Some("revision-1".to_owned()));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_content_returns_each_kind_s_own_field_and_none() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let documents = [
        text_document("docs/a.md", "found content")?,
        symbol_document(
            "crate::beacon",
            "src/beacon.rs",
            "beacon",
            "pub fn beacon() {}",
        )?,
    ];
    index.replace_all(&documents, "revision-1").await?;

    assert_eq!(
        index.content(&identity("docs/a.md")?).await?,
        Some("found content".to_owned()),
        "a text-file document answers with its file content"
    );
    assert_eq!(
        index.content(&identity("crate::beacon")?).await?,
        Some("pub fn beacon() {}".to_owned()),
        "a symbol document answers with its declaration source"
    );
    assert_eq!(index.content(&identity("docs/missing.md")?).await?, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_reopen_from_file_serves_persisted_rows() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);

    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let documents = [text_document("docs/a.md", "persisted content")?];
    index.replace_all(&documents, "revision-1").await?;
    drop(index);

    let reopened = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    assert_eq!(
        reopened.content(&identity("docs/a.md")?).await?,
        Some("persisted content".to_owned())
    );
    assert_eq!(
        reopened.tree_revision().await?,
        Some("revision-1".to_owned())
    );
    let hits = search_matches(&reopened, "revision-1", "persisted", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "FTS rows must survive restart alongside typed rows"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_symbol_and_text_file_documents_coexist() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let symbol = symbol_document(
        "crate::widgets::render",
        "src/widgets.rs",
        "render",
        "fn render() { paint_surface() }",
    )?;
    let text = text_document("docs/widgets.md", "widget documentation prose")?;
    index.replace_all(&[symbol, text], "revision-1").await?;

    let symbol_hits = search_matches(&index, "revision-1", "paint_surface", 10).await?;
    assert_eq!(symbol_hits.len(), 1);
    assert_eq!(symbol_hits[0].kind(), DocumentKind::Symbol);
    assert_eq!(symbol_hits[0].path().as_str(), "src/widgets.rs");
    assert_eq!(
        symbol_hits[0].fields(),
        FieldSet::of(SearchableField::DeclarationSource)
    );

    let text_hits = search_matches(&index, "revision-1", "prose", 10).await?;
    assert_eq!(text_hits.len(), 1);
    assert_eq!(text_hits[0].kind(), DocumentKind::TextFile);
    assert_eq!(text_hits[0].path().as_str(), "docs/widgets.md");
    assert_eq!(
        text_hits[0].fields(),
        FieldSet::of(SearchableField::FileContent)
    );

    // A text file's `name` is its final path segment with the extension, so the file is
    // reachable by its own name and not only by what it contains.
    let by_name = search_matches(&index, "revision-1", "widgets.md", 10).await?;
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].identity().as_str(), "docs/widgets.md");
    assert!(by_name[0].fields().holds(SearchableField::Name));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_ranks_a_name_hit_above_a_declaration_source_hit()
-> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let named = document(
        "crate::index::SearchHit",
        "src/index.rs",
        DocumentKind::Symbol,
        DocumentFields::empty()
            .with(SearchableField::Name, "SearchHit")
            .with(
                SearchableField::DeclarationSource,
                "fn locate() { finds nothing relevant here }",
            ),
    )?;
    let bodied = document(
        "crate::index::Unrelated",
        "src/index.rs",
        DocumentKind::Symbol,
        DocumentFields::empty()
            .with(SearchableField::Name, "Unrelated")
            .with(
                SearchableField::DeclarationSource,
                "this function will search the entire tree",
            ),
    )?;
    index.replace_all(&[named, bodied], "revision-1").await?;

    let hits = search_matches(&index, "revision-1", "search", 10).await?;
    assert_eq!(
        hits.len(),
        2,
        "both the name hit and the declaration-source hit must be found"
    );
    assert_eq!(
        hits[0].identity().as_str(),
        "crate::index::SearchHit",
        "the declared bm25 weights rank a name hit above a declaration-source hit"
    );
    assert_eq!(hits[0].fields(), FieldSet::of(SearchableField::Name));
    assert_eq!(
        hits[1].fields(),
        FieldSet::of(SearchableField::DeclarationSource)
    );
    assert!(
        hits[0].rank() < hits[1].rank(),
        "rank is ascending, so the name hit carries the lower value"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_finds_camel_case_name_by_expanded_words() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let declaration = symbol_document(
        "crate::account::getUserName",
        "src/account.rs",
        "getUserName",
        "returns account holder identifier",
    )?;
    index.replace_all(&[declaration], "revision-1").await?;

    let hits = search_matches(&index, "revision-1", "user name", 10).await?;
    assert_eq!(
        hits.len(),
        1,
        "camelCase expansion must make the name discoverable by its split words"
    );
    assert_eq!(hits[0].identity().as_str(), "crate::account::getUserName");
    assert_eq!(
        hits[0].fields(),
        FieldSet::of(SearchableField::IdentifierTerms),
        "the derived terms column is what the split words matched"
    );
    Ok(())
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_documents"]
struct ConcurrentDocumentRecord {
    #[key]
    identity: String,
    path: String,
    kind: String,
    digest: String,
    byte_length: i64,
    name: Option<String>,
    qualified_name: Option<String>,
    identifier_terms: Option<String>,
    signature: Option<String>,
    documentation: Option<String>,
    declaration_source: Option<String>,
    file_content: Option<String>,
}

#[derive(Debug, toasty::Model)]
#[table = "lexical_index_state"]
struct ConcurrentLexicalIndexStateRecord {
    #[key]
    id: i64,
    tree_revision: Option<String>,
    corpus_revision: String,
    derivation_revision: String,
}

async fn open_concurrent_probe(path: &Path) -> toasty::Result<Db> {
    let mut builder = Db::builder();
    let models = toasty::models!(ConcurrentDocumentRecord, ConcurrentLexicalIndexStateRecord);
    builder.models(models).max_pool_size(1);
    builder.build(Sqlite::open(path)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_content_sees_only_committed_writes_during_concurrent_transaction()
-> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let committed = [text_document("docs/a.md", "alpha content")?];
    index.replace_all(&committed, "revision-1").await?;

    let probe_database = open_concurrent_probe(&path).await?;
    let mut probe_connection = probe_database.connection().await?;
    let mut probe_transaction = probe_connection.transaction().await?;
    toasty::create!(ConcurrentDocumentRecord {
        identity: "docs/b.md",
        path: "docs/b.md",
        kind: "text_file",
        digest: "0f1e2d3c",
        byte_length: 5,
        name: Some("b.md".to_owned()),
        qualified_name: None,
        identifier_terms: None,
        signature: None,
        documentation: None,
        declaration_source: None,
        file_content: Some("bravo".to_owned()),
    })
    .exec(&mut probe_transaction)
    .await?;

    let uncommitted_read = index.content(&identity("docs/b.md")?).await?;
    assert_eq!(
        uncommitted_read, None,
        "reader must not see an uncommitted write"
    );

    probe_transaction.commit().await?;
    let committed_read = index.content(&identity("docs/b.md")?).await?;
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
) -> TestResult {
    let mut probe_connection = probe_database.connection().await?;
    toasty::create!(ConcurrentDocumentRecord {
        identity: identity.to_owned(),
        path: path.to_owned(),
        kind: kind.to_owned(),
        digest: "0f1e2d3c".to_owned(),
        byte_length: i64::try_from(content.len())?,
        name: None,
        qualified_name: None,
        identifier_terms: None,
        signature: None,
        documentation: None,
        declaration_source: None,
        file_content: Some(content.to_owned()),
    })
    .exec(&mut probe_connection)
    .await?;
    // The index reads column values from the typed row its rowid names, so the corrupt
    // row is indexed from the row just written, the way the store indexes its own.
    let columns = SearchableField::ALL.map(SearchableField::column).join(", ");
    toasty::sql::statement(format!(
        "INSERT INTO lexical_documents_fts(rowid, {columns}) \
         SELECT id, {columns} FROM lexical_documents WHERE identity = ?1"
    ))
    .bind(identity.to_owned())
    .exec(&mut probe_connection)
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_stored_invalid_path_refuses() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    // The stamp qualifies the query, so the store carries one before the corrupt row
    // reaches it; the decode this test is about runs only for a store holding this tree.
    index.replace_all(&[], "revision-1").await?;

    let probe_database = open_concurrent_probe(&path).await?;
    insert_corrupt_lexical_row(
        &probe_database,
        "bad-path",
        "../outside.md",
        "text_file",
        "corrupt marker",
    )
    .await?;

    let query = ParsedQuery::parse("corrupt")?;
    let outcome = index
        .search("revision-1", &query, QueryPhase::Precise, 10)
        .await;
    let error = outcome.expect_err("a stored row with an invalid path must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::StoredPathInvalid
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_stored_invalid_kind_refuses() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index.replace_all(&[], "revision-1").await?;

    let probe_database = open_concurrent_probe(&path).await?;
    insert_corrupt_lexical_row(
        &probe_database,
        "bad-kind",
        "docs/bad.md",
        "not_a_real_kind",
        "corrupt marker",
    )
    .await?;

    let query = ParsedQuery::parse("corrupt")?;
    let outcome = index
        .search("revision-1", &query, QueryPhase::Precise, 10)
        .await;
    let error = outcome.expect_err("a stored row with an unknown kind must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::StoredKindInvalid
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_open_at_unusable_path_refuses_with_storage_failure() -> TestResult
{
    let directory = TempDir::new()?;
    let path = directory.path().join("missing-parent").join("lexical.db");

    let outcome = WorkspaceDatabase::open(&path, database_pool()).await;
    let error = outcome.expect_err("opening under a missing parent directory must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
    assert_eq!(error.fault().path(), Some(path.as_path()));
    Ok(())
}

/// Table shape mirroring the first migration's `lexical_units` so a raw connection can
/// create a conflicting table before the adapter ever opens the path: same table name,
/// incompatible columns.
#[derive(Debug, toasty::Model)]
#[table = "lexical_units"]
struct ConflictingUnitRecord {
    #[key]
    identity: String,
}

async fn open_conflicting_schema_probe(path: &Path) -> toasty::Result<Db> {
    let mut builder = Db::builder();
    builder
        .models(toasty::models!(ConflictingUnitRecord))
        .max_pool_size(1);
    builder.build(Sqlite::open(path)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_open_migration_apply_conflict_refuses_distinct_from_build_failure()
-> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);

    // Pre-create a `lexical_units` table with the wrong shape through a raw
    // connection, bypassing the adapter's own migrations entirely. `open`'s
    // own build step succeeds (the file and a connection are perfectly
    // usable); only the later `MIGRATIONS.apply` call fails, since the first
    // migration's `CREATE TABLE lexical_units` collides with the one already
    // present.
    let probe_database = open_conflicting_schema_probe(&path).await?;
    let mut probe_connection = probe_database.connection().await?;
    toasty::sql::statement("CREATE TABLE lexical_units(id INTEGER PRIMARY KEY)")
        .exec(&mut probe_connection)
        .await?;
    drop(probe_connection);
    drop(probe_database);

    let outcome = WorkspaceDatabase::open(&path, database_pool()).await;
    let error =
        outcome.expect_err("migration apply against a pre-existing conflicting table must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
    assert_eq!(error.fault().path(), Some(path.as_path()));
    assert!(
        std::error::Error::source(&error)
            .is_some_and(|source| source.to_string().contains("lexical_units")),
        "migration failure must preserve the underlying SQL conflict, not just a build failure"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_replace_all_against_external_writer_surfaces_storage_failure()
-> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, DatabasePool::new(4, 25)).await?,
        LexicalIndexLimits::default(),
    );

    // The process write lane cannot coordinate another process. A second pool
    // models that boundary and proves the configured busy timeout still turns
    // an externally held write lock into the store's typed storage failure.
    let mut blocker_builder = Db::builder();
    blocker_builder.max_pool_size(1);
    let blocker_database = blocker_builder.build(Sqlite::open(&path)).await?;
    let mut blocker_connection = blocker_database.connection().await?;
    let blocker = blocker_connection
        .transaction_builder()
        .mode(TransactionMode::Immediate)
        .begin()
        .await?;
    let outcome = index
        .replace_all(&[text_document("docs/a.md", "content")?], "revision-1")
        .await;
    blocker.rollback().await?;

    let error = outcome.expect_err("an externally held write lock must surface a storage failure");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::Storage);
    assert_eq!(error.name(), ErrorName::Wire(ErrorCode::StorageFailure));
    assert!(
        std::error::Error::source(&error).is_some(),
        "storage_error must preserve the underlying toasty/SQLite cause"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_replaces_one_path_and_keeps_the_rest() -> TestResult {
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let documents = [
        symbol_document(
            "rift://symbol/rust/kept.rs/keptalpha",
            "kept.rs",
            "keptalpha",
            "pub fn keptalpha() {}",
        )?,
        symbol_document(
            "rift://symbol/rust/moved.rs/firstbeta",
            "moved.rs",
            "firstbeta",
            "pub fn firstbeta() {}",
        )?,
    ];
    index.replace_all(&documents, "revision-one").await?;

    let replacement = symbol_document(
        "rift://symbol/rust/moved.rs/secondgamma",
        "moved.rs",
        "secondgamma",
        "pub fn secondgamma() {}",
    )?;
    let change = LexicalChange::new(vec![ProjectPath::new("moved.rs")?], vec![replacement]);
    index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await?;

    assert_eq!(
        index.tree_revision().await?,
        Some("revision-two".to_owned())
    );
    assert_eq!(
        search_matches(&index, "revision-two", "secondgamma", 8)
            .await?
            .len(),
        1
    );
    assert!(
        search_matches(&index, "revision-two", "firstbeta", 8)
            .await?
            .is_empty()
    );
    assert_eq!(
        search_matches(&index, "revision-two", "keptalpha", 8)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_deletes_every_chunk_filed_under_one_path() -> TestResult {
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    // A chunked text file files every chunk under its own path, so one delete by path has
    // to reach all of them.
    let first = text_chunk("docs/guide.md#0", "docs/guide.md", "chapter alphaone")?;
    let second = text_chunk("docs/guide.md#1", "docs/guide.md", "chapter betatwo")?;
    index.replace_all(&[first, second], "revision-one").await?;
    assert_eq!(
        search_matches(&index, "revision-one", "chapter", 8)
            .await?
            .len(),
        2
    );

    let change = LexicalChange::new(vec![ProjectPath::new("docs/guide.md")?], Vec::new());
    index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await?;
    assert!(
        search_matches(&index, "revision-two", "chapter", 8)
            .await?
            .is_empty()
    );
    assert_eq!(
        index.tree_revision().await?,
        Some("revision-two".to_owned())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_refuses_a_resulting_set_past_units_max() -> TestResult {
    let directory = TempDir::new()?;
    let limits = LexicalIndexLimits::new(2, 65_536, 64, 4, 1_000);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        limits,
    );
    index
        .replace_all(
            &[
                text_document("docs/a.md", "alphaone")?,
                text_document("docs/b.md", "betatwo")?,
            ],
            "revision-one",
        )
        .await?;

    // Nothing is dropped, so the two stored documents plus one insert cross the bound the
    // stored set is measured against, not the batch.
    let change = LexicalChange::new(Vec::new(), vec![text_document("docs/c.md", "gammathree")?]);
    let error = index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await
        .expect_err("a resulting set past units_max must refuse");
    assert_eq!(error.fault().violation(), LexicalIndexViolation::UnitLimit);
    assert_eq!(
        index.tree_revision().await?,
        Some("revision-one".to_owned()),
        "a refused apply leaves the previous stamp intact"
    );
    assert_eq!(
        search_matches(&index, "revision-one", "alphaone", 8)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_refuses_two_documents_sharing_one_identity() -> TestResult
{
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index
        .replace_all(&[text_document("docs/a.md", "alphaone")?], "revision-one")
        .await?;

    let change = LexicalChange::new(
        Vec::new(),
        vec![
            text_document("docs/b.md", "betatwo")?,
            text_document("docs/b.md", "betatwo")?,
        ],
    );
    let error = index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await
        .expect_err("two documents sharing one identity must refuse");
    assert_eq!(
        error.fault().violation(),
        LexicalIndexViolation::DuplicateIdentity
    );
    assert_eq!(
        index.tree_revision().await?,
        Some("revision-one".to_owned()),
        "the refusal lands before any transaction opens"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_survives_a_reopen_of_the_same_database() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index
        .replace_all(&[text_document("docs/a.md", "alphaone")?], "revision-one")
        .await?;
    let change = LexicalChange::new(
        vec![ProjectPath::new("docs/a.md")?],
        vec![text_document("docs/a.md", "betatwo")?],
    );
    index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await?;
    drop(index);

    let reopened = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    assert_eq!(
        reopened.tree_revision().await?,
        Some("revision-two".to_owned())
    );
    assert_eq!(
        search_matches(&reopened, "revision-two", "betatwo", 8)
            .await?
            .len(),
        1
    );
    assert!(
        search_matches(&reopened, "revision-two", "alphaone", 8)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_deletes_one_path_through_its_own_index() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    drop(index);

    // `apply` deletes by path on every incremental publication, so the planner
    // has to reach those rows through `lexical_documents_path` rather than
    // scanning every document the workspace indexed.
    let probe_database = open_concurrent_probe(&path).await?;
    let mut probe_connection = probe_database.connection().await?;
    let rows =
        toasty::sql::query("EXPLAIN QUERY PLAN DELETE FROM lexical_documents WHERE path = ?1")
            .bind("src/lib.rs".to_owned())
            .column_types([Type::I64, Type::I64, Type::I64, Type::String])
            .exec(&mut probe_connection)
            .await?;
    let plan = format!("{rows:?}");
    assert!(
        plan.contains("lexical_documents_path"),
        "a per-path delete must use the path index: plan={plan}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_under_another_revision_names_the_stored_one() -> TestResult
{
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index
        .replace_all(&[text_document("docs/a.md", "alphaone")?], "revision-two")
        .await?;

    // A caller holding the previous publication asks for a tree the store has moved past.
    // Naming what it holds is what lets that caller recapture instead of ranking rows it
    // cannot place.
    let query = ParsedQuery::parse("alphaone")?;
    assert_eq!(
        index
            .search("revision-one", &query, QueryPhase::Precise, 8)
            .await?,
        RevisionScoped::OtherRevision("revision-two".to_owned())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_before_any_population_reports_no_revision() -> TestResult
{
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    let query = ParsedQuery::parse("alphaone")?;
    assert_eq!(
        index
            .search("revision-one", &query, QueryPhase::Precise, 8)
            .await?,
        RevisionScoped::NoRevision
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_search_qualifies_an_empty_query_by_revision_too() -> TestResult {
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );

    // A query with no term matches nothing, but the store still has to be the one the
    // caller asked for: an empty answer from another tree is not the same answer.
    let query = ParsedQuery::parse("   ")?;
    assert_eq!(
        index
            .search("revision-one", &query, QueryPhase::Precise, 8)
            .await?,
        RevisionScoped::NoRevision
    );
    index.replace_all(&[], "revision-one").await?;
    let matched = index
        .search("revision-one", &query, QueryPhase::Precise, 8)
        .await?;
    let RevisionScoped::Matched(ranking) = matched else {
        return Err("the store holds the tree that was just stamped".into());
    };
    assert!(ranking.matches().is_empty());
    assert_eq!(ranking.truncated_at(), None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_apply_of_one_change_twice_leaves_one_document_set() -> TestResult
{
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index.replace_all(&[], "revision-one").await?;

    // Two rebuilds captured from one publication both name the file as added, and the
    // second commits after the first has written it. A change replaces the paths it names,
    // so the second commit is the first one repeated rather than a second insert of one
    // identity.
    let change = LexicalChange::new(
        vec![ProjectPath::new("docs/added.md")?],
        vec![text_document("docs/added.md", "alphaone content")?],
    );
    index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await?;
    index
        .apply(&change, &LexicalStamp::published("revision-two", ""))
        .await?;

    assert_eq!(
        search_matches(&index, "revision-two", "alphaone", 8)
            .await?
            .len(),
        1,
        "the repeated commit leaves one document, not two rows under one identity"
    );
    Ok(())
}

/// Runs FTS5's own consistency check, comparing the index with the typed rows it reads:
/// "If the value 1 is inserted into the rank column, the index is also compared to the
/// content table" (<https://www.sqlite.org/fts5.html>).
async fn assert_index_matches_rows(path: &Path) -> TestResult {
    let probe = open_concurrent_probe(path).await?;
    let mut connection = probe.connection().await?;
    toasty::sql::statement(
        "INSERT INTO lexical_documents_fts(lexical_documents_fts, rank) \
         VALUES('integrity-check', 1)",
    )
    .exec(&mut connection)
    .await?;
    Ok(())
}

fn recorded(
    path: &str,
    bytes: &[u8],
) -> Result<(ProjectPath, FileDigest), Box<dyn std::error::Error>> {
    Ok((ProjectPath::new(path)?, FileDigest::of(bytes)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_recorded_files_answer_what_one_derivation_recorded() -> TestResult
{
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    assert_eq!(
        index.recorded_files("derivation-a").await?,
        None,
        "an empty store records nothing"
    );

    let alpha = recorded("alpha.rs", b"pub fn alpha() {}")?;
    let beta = recorded("beta.rs", b"pub fn beta() {}")?;
    let change = LexicalChange::new(
        vec![alpha.0.clone(), beta.0.clone()],
        vec![
            symbol_document(
                "rift://symbol/rust/alpha.rs/alpha",
                "alpha.rs",
                "alpha",
                "pub fn alpha() {}",
            )?,
            symbol_document(
                "rift://symbol/rust/beta.rs/beta",
                "beta.rs",
                "beta",
                "pub fn beta() {}",
            )?,
        ],
    )
    .with_recorded(vec![alpha.clone(), beta.clone()]);
    index
        .apply(
            &change,
            &LexicalStamp::published("revision-one", "derivation-a"),
        )
        .await?;

    assert_eq!(
        index.recorded_files("derivation-a").await?,
        Some(WorkspaceDigests::new([alpha.clone(), beta])),
    );
    assert_eq!(
        index.recorded_files("derivation-b").await?,
        None,
        "rows another derivation stamped are never kept"
    );

    let removal = LexicalChange::new(vec![ProjectPath::new("beta.rs")?], Vec::new());
    index
        .apply(
            &removal,
            &LexicalStamp::published("revision-two", "derivation-a"),
        )
        .await?;
    assert_eq!(
        index.recorded_files("derivation-a").await?,
        Some(WorkspaceDigests::new([alpha])),
        "a replaced path the change records nothing for is forgotten"
    );
    assert!(
        search_matches(&index, "revision-two", "beta", 8)
            .await?
            .is_empty()
    );
    assert_index_matches_rows(&database_path(&directory)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_a_whole_replace_records_nothing_a_later_write_can_keep()
-> TestResult {
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let alpha = recorded("alpha.rs", b"pub fn alpha() {}")?;
    let change = LexicalChange::new(vec![alpha.0.clone()], Vec::new()).with_recorded(vec![alpha]);
    index
        .apply(
            &change,
            &LexicalStamp::published("revision-one", "derivation-a"),
        )
        .await?;
    index
        .replace_all(&[text_document("notes.md", "gamma notes")?], "revision-two")
        .await?;
    assert_eq!(index.recorded_files("derivation-a").await?, None);
    assert_eq!(
        search_matches(&index, "revision-two", "gamma", 8)
            .await?
            .len(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_an_unpublished_stamp_answers_no_revision_and_keeps_its_records()
-> TestResult {
    let directory = TempDir::new()?;
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(&directory), database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    index
        .replace_all(&[text_document("notes.md", "delta notes")?], "revision-one")
        .await?;
    let epsilon = recorded("epsilon.md", b"epsilon notes")?;
    let part = LexicalChange::new(
        vec![epsilon.0.clone()],
        vec![text_document("epsilon.md", "epsilon notes")?],
    )
    .with_recorded(vec![epsilon.clone()]);
    index
        .apply(&part, &LexicalStamp::unpublished("derivation-a"))
        .await?;

    assert_eq!(index.tree_revision().await?, None);
    let query = ParsedQuery::parse("epsilon")?;
    assert_eq!(
        index
            .search("revision-one", &query, QueryPhase::Precise, 8)
            .await?,
        RevisionScoped::NoRevision,
        "rows mid-write answer for no publication, neither the previous nor the next"
    );
    assert_eq!(
        index.recorded_files("derivation-a").await?,
        Some(WorkspaceDigests::new([epsilon])),
        "a write stopped between its parts keeps what those parts recorded"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_clear_empties_rows_records_and_the_publication() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let zeta = recorded("zeta.md", b"zeta notes")?;
    let change = LexicalChange::new(
        vec![zeta.0.clone()],
        vec![text_document("zeta.md", "zeta notes")?],
    )
    .with_recorded(vec![zeta]);
    index
        .apply(
            &change,
            &LexicalStamp::published("revision-one", "derivation-a"),
        )
        .await?;

    index.clear("derivation-b").await?;

    assert_eq!(index.tree_revision().await?, None);
    assert_eq!(
        index.recorded_files("derivation-b").await?,
        Some(WorkspaceDigests::default())
    );
    assert_eq!(index.recorded_files("derivation-a").await?, None);
    let refill = LexicalChange::new(Vec::new(), vec![text_document("zeta.md", "zeta again")?]);
    index
        .apply(
            &refill,
            &LexicalStamp::published("revision-two", "derivation-b"),
        )
        .await?;
    assert_eq!(
        search_matches(&index, "revision-two", "again", 8)
            .await?
            .len(),
        1
    );
    assert_index_matches_rows(&path).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_lexical_search_index_keeps_the_full_text_index_in_step_with_the_rows() -> TestResult {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&path, database_pool()).await?,
        LexicalIndexLimits::default(),
    );
    let chunks = [
        text_chunk("notes.md#1", "notes.md", "first eta chunk")?,
        text_chunk("notes.md#2", "notes.md", "second eta chunk")?,
        symbol_document(
            "rift://symbol/rust/theta.rs/theta",
            "theta.rs",
            "theta",
            "pub fn theta() {}",
        )?,
    ];
    index.replace_all(&chunks, "revision-one").await?;
    assert_index_matches_rows(&path).await?;

    let rewritten = LexicalChange::new(
        vec![ProjectPath::new("notes.md")?],
        vec![text_chunk("notes.md#1", "notes.md", "only iota chunk")?],
    );
    index
        .apply(
            &rewritten,
            &LexicalStamp::published("revision-two", "derivation-a"),
        )
        .await?;
    assert_index_matches_rows(&path).await?;
    assert!(
        search_matches(&index, "revision-two", "eta", 8)
            .await?
            .is_empty()
    );
    assert_eq!(
        search_matches(&index, "revision-two", "iota", 8)
            .await?
            .len(),
        1
    );

    index
        .apply(
            &rewritten,
            &LexicalStamp::published("revision-three", "derivation-a"),
        )
        .await?;
    assert_index_matches_rows(&path).await?;
    assert_eq!(
        search_matches(&index, "revision-three", "iota", 8)
            .await?
            .len(),
        1,
        "a change applied twice leaves one indexed row, never two"
    );
    Ok(())
}
