//! Compare incremental and cold read-service facts after file replacement and restore.
//!
//! Keep complete file digests, index documents, documentation metadata, and relationship
//! reads in equality checks; the fixture must retain live files named only by the earlier tree.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use rift_core::{SourceVisibility, SymbolId, TextFileInclusion};
use rift_index::DatabaseName;
use rift_index::{
    DatabasePool, LexicalIndexLimits, LexicalSearchIndex, LexicalStamp, PathChanges,
    WorkspaceDatabase, WorkspaceIndex, WorkspaceIndexLimits, capture_digests,
};
use rift_protocol::{
    configuration::HistoryConfiguration,
    map::WorkspaceMap,
    read::{
        GetSymbolInclude, GetSymbolParams, GetSymbolResult, NodesParams, ReadWarning,
        SYMBOL_ALTERNATIVES_MAX, SearchScope, SyntaxFramework,
    },
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
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n",
    )?;
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

#[test]
fn unresolved_angular_template_urls_keep_source_nodes_and_path_owned_warnings() -> TestResult {
    for template in ["https://example.test/view.html", "view.html?mode=preview"] {
        let directory = tempfile::tempdir()?;
        let source = format!(
            "import {{ Component }} from '@angular/core';\n@Component({{ templateUrl: '{template}' }})\nexport class Beacon {{ label = '灯'; }}\n"
        );
        let plain = "<p class=\"flex\">灯{{title}}</p>";
        fs::write(directory.path().join("component.ts"), &source)?;
        fs::write(directory.path().join("view.html"), plain)?;
        let service = build(directory.path())?;
        let declaration = service.get_symbol(&GetSymbolParams {
            name: "Beacon".to_owned(),
            language: None,
            scope: SearchScope::Local,
            packages: Vec::new(),
            include: vec![GetSymbolInclude::Source],
            limit: 10,
            page_index: 0,
            rev: None,
        })?;
        let hit = declaration.hits.first().ok_or("Beacon declaration")?;
        assert_eq!(
            hit.path.as_ref().ok_or("declaration path")?.0,
            "component.ts"
        );
        let start = usize::try_from(hit.range.start)?;
        let end = usize::try_from(hit.range.end)?;
        assert_eq!(hit.source.as_deref(), Some(&source[start..end]));
        assert!(
            hit.source
                .as_deref()
                .is_some_and(|text| text.contains("class Beacon"))
        );

        let position = u64::try_from(source.find("灯").ok_or("Unicode source")?)?;
        let nodes = service.nodes(NodesParams {
            path: rift_protocol::read::ProjectPath("component.ts".to_owned()),
            position,
            rev: None,
        })?;
        assert!(!nodes.nodes.is_empty());
        assert_eq!(nodes.nodes.len(), nodes.source.len());
        for (node, excerpt) in nodes.nodes.iter().zip(&nodes.source) {
            assert_eq!(node.language.identity_segment(), "typescript");
            let start = usize::try_from(node.range.start)?;
            let end = usize::try_from(node.range.end)?;
            assert_eq!(excerpt, &source[start..end]);
        }
        assert!(nodes.warnings.iter().any(|warning| matches!(
            warning,
            ReadWarning::FrameworkContextUnresolved {
                unit,
                framework: SyntaxFramework::Angular,
                detail,
            } if unit.0 == "rift://file/component.ts" && !detail.is_empty()
        )));
        let plain_nodes = service.nodes(NodesParams {
            path: rift_protocol::read::ProjectPath("view.html".to_owned()),
            position: u64::try_from(plain.find("灯").ok_or("plain Unicode source")?)?,
            rev: None,
        })?;
        assert!(!plain_nodes.nodes.is_empty());
        for (node, excerpt) in plain_nodes.nodes.iter().zip(&plain_nodes.source) {
            assert_eq!(node.language.identity_segment(), "html");
            let start = usize::try_from(node.range.start)?;
            let end = usize::try_from(node.range.end)?;
            assert_eq!(excerpt, &plain[start..end]);
        }
        assert!(
            !plain_nodes
                .warnings
                .iter()
                .any(|warning| matches!(warning, ReadWarning::FrameworkContextUnresolved { .. }))
        );
    }
    Ok(())
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
        for symbol in file.syntax().symbols() {
            names.insert(symbol.name.clone());
            let matched = rift_index::SymbolMatch {
                file,
                symbol,
                rank: rift_ranking::IdentifierMatchClass::QualifiedExact.into(),
            };
            if let Some(identity) = index.assembled_symbol(matched)?.identity() {
                ids.insert(identity.clone());
            }
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

fn symbol_pages(
    service: &ReadService,
    name: &str,
    current_ids: &BTreeSet<SymbolId>,
    current_names: &BTreeSet<String>,
) -> TestResult<Vec<serde_json::Value>> {
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
        validate_symbol_page(&result, name, current_ids, current_names)?;
        pages.push(serde_json::to_value(&result)?);
        page_index += 1;
        if page_index >= total_pages {
            return Ok(pages);
        }
    }
}

/// A complete empty lookup carries only its named miss and current declaration identities.
/// Oracle requests use extracted short names: a name still declared must match exactly.
fn validate_symbol_page(
    result: &GetSymbolResult,
    name: &str,
    current_ids: &BTreeSet<SymbolId>,
    current_names: &BTreeSet<String>,
) -> TestResult {
    match result.warnings.as_slice() {
        [] if !result.hits.is_empty() || result.pagination.total_pages > 0 => Ok(()),
        [
            ReadWarning::SymbolNotFound {
                name: requested,
                alternatives,
                detail,
            },
        ] if requested == name
            && !current_names.contains(name)
            && result.hits.is_empty()
            && result.pagination.total_pages == 0
            && result.pagination.page_index == 0
            && detail.is_none() =>
        {
            let distinct = alternatives
                .iter()
                .map(|id| id.0.as_str())
                .collect::<BTreeSet<_>>();
            let expected = current_ids.len().min(SYMBOL_ALTERNATIVES_MAX);
            if alternatives.len() != expected
                || distinct.len() != alternatives.len()
                || distinct
                    .iter()
                    .any(|id| !current_ids.iter().any(|current| current.as_str() == *id))
            {
                return Err(format!(
                    "symbol alternatives are invalid for {name}: {alternatives:?}"
                )
                .into());
            }
            Ok(())
        }
        _ => Err(format!(
            "symbol lookup is incomplete for {name}: {:?}",
            result.warnings
        )
        .into()),
    }
}

#[test]
fn symbol_page_validation_rejects_incomplete_or_invalid_misses() -> TestResult {
    let directory = TempDir::new()?;
    write_tree_a(directory.path())?;
    let service = build(directory.path())?;
    let (current_ids, current_names) = symbol_facts(directory.path())?;
    let request = |name: &str| -> TestResult<GetSymbolParams> {
        Ok(serde_json::from_value(
            serde_json::json!({"name": name, "include": []}),
        )?)
    };
    let miss = service.get_symbol(&request("Absent")?)?;
    validate_symbol_page(&miss, "Absent", &current_ids, &current_names)?;
    let wire = serde_json::to_value(&miss)?;
    let preparing = serde_json::json!({
        "code": "local_index_preparing", "prepared": 0, "detail": "selected files are preparing"
    });
    let mut wrong_name = wire["warnings"].clone();
    wrong_name[0]["name"] = serde_json::json!("Other");
    let mut unavailable = wire["warnings"].clone();
    unavailable[0]["detail"] =
        serde_json::json!("closest alternatives unavailable at the work bound");
    let mut unknown = wire["warnings"].clone();
    unknown[0]["alternatives"][0] = serde_json::json!(
        rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            rift_protocol::read::Language {
                name: "rust".to_owned(),
                dialect: None
            },
            vec!["beacon".to_owned(), "Unknown".to_owned()],
        )
        .expect("canonical unknown fixture identity")
        .wire_identity()
    );
    let mut repeated = wire["warnings"].clone();
    repeated[0]["alternatives"][1] = repeated[0]["alternatives"][0].clone();
    let mut oversized = wire["warnings"].clone();
    let extra = oversized[0]["alternatives"][0].clone();
    oversized[0]["alternatives"]
        .as_array_mut()
        .ok_or("alternatives absent")?
        .push(extra);
    let mut mixed = wire["warnings"].clone();
    mixed
        .as_array_mut()
        .ok_or("warnings absent")?
        .push(preparing.clone());
    for warnings in [
        serde_json::json!([]),
        wrong_name,
        unavailable,
        unknown,
        repeated,
        oversized,
        serde_json::json!([preparing]),
        mixed,
    ] {
        let mut invalid = wire.clone();
        invalid["warnings"] = warnings;
        let result: GetSymbolResult = serde_json::from_value(invalid)?;
        assert!(
            validate_symbol_page(&result, "Absent", &current_ids, &current_names).is_err(),
            "{result:?}"
        );
    }
    let mut nonempty = miss.clone();
    nonempty.hits = service.get_symbol(&request("Beacon")?)?.hits;
    assert!(validate_symbol_page(&nonempty, "Absent", &current_ids, &current_names).is_err());
    let mut existing = wire;
    existing["warnings"][0]["name"] = serde_json::json!("Beacon");
    let existing: GetSymbolResult = serde_json::from_value(existing)?;
    assert!(validate_symbol_page(&existing, "Beacon", &current_ids, &current_names).is_err());
    let mut continuation = miss;
    continuation.pagination.page_index = 1;
    assert!(validate_symbol_page(&continuation, "Absent", &current_ids, &current_names).is_err());
    Ok(())
}

fn symbol_rows(
    service: &ReadService,
    names: &BTreeSet<String>,
    current_ids: &BTreeSet<SymbolId>,
    current_names: &BTreeSet<String>,
) -> TestResult<BTreeSet<String>> {
    names
        .iter()
        .map(|name| {
            Ok(serde_json::to_string(&(
                name,
                symbol_pages(service, name, current_ids, current_names)?,
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
    let (current_ids, current_names) = symbol_facts(root)?;
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
        symbol_rows(incremental, symbol_names, &current_ids, &current_names)?,
        symbol_rows(&cold, symbol_names, &current_ids, &current_names)?,
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
    let database = WorkspaceDatabase::open(
        database_path,
        DatabaseName::Index,
        DatabasePool::new(2, 1_000),
    )
    .await?;
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
