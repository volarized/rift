//! Compare incremental and cold read-service facts after file replacement and restore.
//!
//! Keep complete file digests, index documents, documentation metadata, and relationship
//! reads in equality checks; the fixture must retain live files named only by the earlier tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rift_core::{SourceVisibility, SymbolId, TextFileInclusion, symbol_identity};
use rift_index::{
    DatabasePool, LexicalIndexLimits, LexicalSearchIndex, LexicalStamp, PathChanges,
    WorkspaceDatabase, WorkspaceIndex, WorkspaceIndexLimits, capture_digests,
};
use rift_protocol::{
    configuration::HistoryConfiguration,
    map::WorkspaceMap,
    read::{GetSymbolInclude, GetSymbolParams, SearchScope},
};
use rift_server::ReadService;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type RelationshipRow = (String, String, String, String, u64, u64, Option<String>);
const SYMBOL_PAGES_MAX: u64 = 32;

const SOURCE_A: &str = "pub struct Beacon;\npub fn old() {}\n";
const README_A: &str = "See [Beacon](src/lib.rs#Beacon).\n";
const SOURCE_B: &str = "pub struct Beacon;\npub fn new() {}\n";
const README_B: &str = "See [new](src/lib.rs#new).\n";

fn write_tree_a(root: &Path) -> TestResult {
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/lib.rs"), SOURCE_A)?;
    fs::write(root.join("src/removed.rs"), "pub struct Removed;\n")?;
    let added = root.join("src/added.rs");
    if added.exists() {
        fs::remove_file(added)?;
    }
    fs::write(root.join("README.md"), README_A)?;
    Ok(())
}

fn write_tree_b(root: &Path) -> TestResult {
    fs::write(root.join("src/lib.rs"), SOURCE_B)?;
    fs::remove_file(root.join("src/removed.rs"))?;
    fs::write(root.join("src/added.rs"), "pub struct Added;\n")?;
    fs::write(root.join("README.md"), README_B)?;
    Ok(())
}

fn build(root: &Path) -> TestResult<ReadService> {
    Ok(ReadService::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )?)
}

fn updated(previous: &ReadService, root: &Path) -> TestResult<ReadService> {
    let observed = capture_digests(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
    )?;
    let changes = PathChanges::between(&previous.workspace_digests(), &observed);
    if changes.is_empty() {
        return Err("fixture change must replace at least one path".into());
    }
    Ok(previous.rebuilt(&changes)?)
}

fn symbol_facts(root: &Path) -> TestResult<(BTreeSet<SymbolId>, BTreeSet<String>)> {
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?;
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for file in index.files() {
        let language = file.syntax().language().identity_segment();
        for symbol in file.syntax().symbols() {
            names.insert(symbol.name.clone());
            ids.insert(SymbolId::new(symbol_identity(
                &language,
                file.path().as_str(),
                &symbol.qualified_name,
            ))?);
        }
    }
    Ok((ids, names))
}

fn case_source_paths(root: &Path) -> TestResult<BTreeSet<String>> {
    let index = WorkspaceIndex::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
    )?;
    Ok(index
        .files()
        .map(|file| file.path().as_str().to_owned())
        .filter(|path| matches!(path.as_str(), "src/Case.rs" | "src/case.rs"))
        .collect())
}

fn relationship_rows(
    service: &ReadService,
    ids: &BTreeSet<SymbolId>,
) -> TestResult<BTreeSet<RelationshipRow>> {
    let relationships = service.relationships();
    if !relationships.is_complete() || relationships.dropped_edges() != 0 {
        return Err(format!(
            "relationship store incomplete: dropped_edges={}",
            relationships.dropped_edges()
        )
        .into());
    }
    let mut rows = BTreeSet::new();
    for id in ids {
        for edge in relationships
            .outgoing(id)
            .iter()
            .chain(relationships.incoming(id))
        {
            let occurrence = edge.occurrence();
            let range = occurrence.range();
            rows.insert((
                edge.from().as_str().to_owned(),
                edge.to().as_str().to_owned(),
                serde_json::to_string(&edge.facet())?,
                occurrence.unit().to_string(),
                range.start(),
                range.end(),
                occurrence.node().map(|node| node.0.clone()),
            ));
        }
    }
    Ok(rows)
}

fn symbol_pages(service: &ReadService, name: &str) -> TestResult<Vec<serde_json::Value>> {
    let mut pages = Vec::new();
    let mut page_index = 0;
    loop {
        let result = service.get_symbol(&GetSymbolParams {
            name: name.to_owned(),
            language: None,
            scope: SearchScope::Local,
            packages: Vec::new(),
            include: vec![GetSymbolInclude::Source],
            limit: 1,
            page_index,
            rev: None,
        })?;
        let total_pages = result.pagination.total_pages;
        if total_pages > SYMBOL_PAGES_MAX {
            return Err(format!("symbol lookup exceeded {SYMBOL_PAGES_MAX} pages: {name}").into());
        }
        if !result.warnings.is_empty() {
            return Err(format!(
                "symbol lookup is incomplete for {name}: {:?}",
                result.warnings
            )
            .into());
        }
        pages.push(serde_json::to_value(&result)?);
        page_index += 1;
        if page_index >= total_pages {
            return Ok(pages);
        }
    }
}

fn symbol_rows(service: &ReadService, names: &BTreeSet<String>) -> TestResult<BTreeSet<String>> {
    names
        .iter()
        .map(|name| {
            Ok(serde_json::to_string(&(
                name,
                symbol_pages(service, name)?,
            ))?)
        })
        .collect()
}

fn assert_matches_cold(
    incremental: &ReadService,
    root: &Path,
    symbol_ids: &BTreeSet<SymbolId>,
    symbol_names: &BTreeSet<String>,
) -> TestResult {
    let cold = build(root)?;
    assert_eq!(
        incremental.workspace_digests(),
        cold.workspace_digests(),
        "incremental file digests must equal a cold build"
    );
    let incremental_map: WorkspaceMap = incremental.workspace_map();
    assert_eq!(
        incremental_map,
        cold.workspace_map(),
        "incremental map must equal a cold build"
    );
    assert_eq!(
        incremental.index_documents(),
        cold.index_documents(),
        "incremental derived rows must equal a cold build"
    );
    assert_eq!(
        symbol_rows(incremental, symbol_names)?,
        symbol_rows(&cold, symbol_names)?,
        "all served symbol pages and source excerpts must equal a cold build"
    );
    let incremental_documentation = incremental.documentation_snapshot();
    let cold_documentation = cold.documentation_snapshot();
    assert!(
        !incremental_documentation.index().references.is_empty(),
        "fixture must publish documentation references"
    );
    assert_eq!(
        serde_json::to_value(incremental_documentation.index())?,
        serde_json::to_value(cold_documentation.index())?,
        "incremental documentation blocks, links, and references must equal a cold build"
    );
    assert_eq!(
        relationship_rows(incremental, symbol_ids)?,
        relationship_rows(&cold, symbol_ids)?,
        "incremental semantic relationship edges must equal a cold build"
    );
    Ok(())
}

#[test]
fn an_incremental_read_service_matches_cold_workspace_facts() -> TestResult {
    let directory = TempDir::new()?;
    let root = directory.path();
    write_tree_a(root)?;
    let (mut symbol_ids, mut symbol_names) = symbol_facts(root)?;
    let mut service = build(root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    write_tree_b(root)?;
    let (ids_b, names_b) = symbol_facts(root)?;
    symbol_ids.extend(ids_b);
    symbol_names.extend(names_b);
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    write_tree_a(root)?;
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)
}

#[test]
fn case_distinct_read_service_matches_cold_workspace_facts() -> TestResult {
    let directory = TempDir::new()?;
    let root = directory.path();
    write_tree_a(root)?;
    let (mut symbol_ids, mut symbol_names) = symbol_facts(root)?;
    let mut service = build(root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    let source_directory = root.join("src");
    let upper = source_directory.join("Case.rs");
    let lower = source_directory.join("case.rs");
    fs::write(&upper, "pub struct UpperBeacon;\n")?;
    let upper_paths = case_source_paths(root)?;
    assert_eq!(
        upper_paths,
        BTreeSet::from(["src/Case.rs".to_owned()]),
        "upper-case fixture path is visible"
    );
    let (ids, names) = symbol_facts(root)?;
    symbol_ids.extend(ids);
    symbol_names.extend(names);
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    fs::write(&lower, "pub struct LowerBeacon;\n")?;
    let paths = case_source_paths(root)?;
    let both_paths_coexist = paths.contains("src/Case.rs") && paths.contains("src/case.rs");
    assert_eq!(
        paths.len(),
        if both_paths_coexist { 2 } else { 1 },
        "fixture must expose one path per filesystem entry"
    );
    eprintln!("case-distinct fixture retains both paths: {both_paths_coexist}");
    let (ids, names) = symbol_facts(root)?;
    symbol_ids.extend(ids);
    symbol_names.extend(names);
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    fs::write(&lower, "pub struct EditedBeacon;\n")?;
    assert_eq!(
        case_source_paths(root)?,
        paths,
        "editing the lower-case spelling preserves observed path entries"
    );
    let (ids, names) = symbol_facts(root)?;
    symbol_ids.extend(ids);
    symbol_names.extend(names);
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)?;

    fs::remove_file(&lower)?;
    let remaining_paths = case_source_paths(root)?;
    assert_eq!(
        remaining_paths,
        if both_paths_coexist {
            BTreeSet::from(["src/Case.rs".to_owned()])
        } else {
            BTreeSet::new()
        },
        "removal follows the path entries observed on this filesystem"
    );
    let (ids, names) = symbol_facts(root)?;
    symbol_ids.extend(ids);
    symbol_names.extend(names);
    service = updated(&service, root)?;
    assert_matches_cold(&service, root, &symbol_ids, &symbol_names)
}

/// Ordered complete stored values; row ids are checked through FTS integrity instead.
const STORED_FACT_QUERIES: &[&str] = &[
    "SELECT identity, path, kind, digest, byte_length, byte_offset, name, qualified_name, \
     identifier_terms, signature, documentation, file_content FROM lexical_documents ORDER BY identity",
    "SELECT path, digest FROM lexical_files ORDER BY path",
    "SELECT identity, digest, payload FROM documentation_sources ORDER BY identity",
    "SELECT identity, source, target, block FROM documentation_references ORDER BY identity",
    "SELECT id, payload FROM documentation_manifest ORDER BY id",
    "SELECT tree_revision, corpus_revision, derivation_revision FROM lexical_index_state ORDER BY id",
    "SELECT term, col, doc, cnt FROM lexical_documents_vocabulary ORDER BY term, col",
];

type StoredFacts = Vec<Vec<Vec<rusqlite::types::Value>>>;

fn stored_facts(path: &Path) -> TestResult<StoredFacts> {
    let connection = rusqlite::Connection::open(path)?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    assert_eq!(integrity, "ok");
    for index in ["lexical_documents_fts", "lexical_documents_trigram"] {
        connection.execute(
            &format!("INSERT INTO {index}({index}, rank) VALUES('integrity-check', 1)"),
            [],
        )?;
    }
    let pending: i64 =
        connection.query_row("SELECT count(*) FROM lexical_trigram_pending", [], |row| {
            row.get(0)
        })?;
    assert_eq!(pending, 0, "complete store has no pending trigram rows");
    STORED_FACT_QUERIES
        .iter()
        .map(|query| {
            let mut statement = connection.prepare(query)?;
            let columns = statement.column_count();
            let rows = statement.query_map([], |row| {
                (0..columns)
                    .map(|column| row.get(column))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .collect()
}

/// Reopens the actual store and applies only differences derived by the read service.
async fn persist_service(service: &ReadService, database_path: &Path) -> TestResult {
    const DERIVATION: &str = "local-index-oracle";
    let database = WorkspaceDatabase::open(database_path, DatabasePool::new(2, 1_000)).await?;
    let store = LexicalSearchIndex::attached(
        std::sync::Arc::clone(&database),
        LexicalIndexLimits::default(),
    );
    let recorded = store.recorded_files(DERIVATION).await?.unwrap_or_default();
    let content = service.content_digests();
    let changes = PathChanges::between(&recorded, &content);
    let change = service.lexical_change(&changes);
    let stamp = LexicalStamp::published(service.tree_revision(), DERIVATION);
    let documentation = service.documentation_snapshot();
    store
        .apply_with_documentation(&change, &stamp, &documentation)
        .await?;
    assert_eq!(store.recorded_files(DERIVATION).await?, Some(content));
    let mut completed = false;
    for _ in 0..4 {
        if store.index_trigrams().await?.pending() == 0 {
            completed = true;
            break;
        }
    }
    assert!(completed, "fixture trigram batches must complete");
    database
        .shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
        .await?;
    Ok(())
}

async fn assert_persisted_matches_cold(
    service: &ReadService,
    root: &Path,
    path: &Path,
) -> TestResult {
    persist_service(service, path).await?;
    let cold_directory = TempDir::new()?;
    let cold_path = cold_directory.path().join("db");
    let cold = build(root)?;
    persist_service(&cold, &cold_path).await?;
    assert_eq!(
        stored_facts(path)?,
        stored_facts(&cold_path)?,
        "complete persisted rows and FTS terms equal a cold build"
    );
    Ok(())
}

#[tokio::test]
async fn persisted_incremental_facts_match_cold_after_restart_and_restore() -> TestResult {
    let directory = TempDir::new()?;
    let store_directory = TempDir::new()?;
    let root = directory.path();
    let database_path = store_directory.path().join("db");
    write_tree_a(root)?;
    let mut service = build(root)?;
    assert_persisted_matches_cold(&service, root, &database_path).await?;
    let initial = stored_facts(&database_path)?;
    let maximum_id = |path: &Path| -> TestResult<i64> {
        Ok(rusqlite::Connection::open(path)?.query_row(
            "SELECT coalesce(max(id), 0) FROM lexical_documents",
            [],
            |row| row.get(0),
        )?)
    };
    let initial_maximum = maximum_id(&database_path)?;
    persist_service(&service, &database_path).await?;
    assert_eq!(
        maximum_id(&database_path)?,
        initial_maximum,
        "unchanged restart rewrites no lexical row"
    );
    assert_eq!(stored_facts(&database_path)?, initial);

    write_tree_b(root)?;
    service = updated(&service, root)?;
    assert_persisted_matches_cold(&service, root, &database_path).await?;
    write_tree_a(root)?;
    service = updated(&service, root)?;
    assert_persisted_matches_cold(&service, root, &database_path).await?;
    assert_eq!(
        stored_facts(&database_path)?,
        initial,
        "restored source restores all stored facts"
    );
    Ok(())
}
