//! Body matches and excluded lockfiles through the read path: a store publication of a
//! real tree, ranked for one query and answered by [`ReadService::search`].

use std::{error::Error, fs, path::Path};

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{LexicalIndexLimits, WorkspaceIndexLimits};
use rift_protocol::configuration::{HistoryConfiguration, RankingConfiguration};
use rift_protocol::read::{MatchedField, ReadWarning, SearchParams, SearchResult};
use rift_ranking::{BodyTerms, ParsedQuery, QueryPhase, RankingInput, RankingWeights};
use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits};
use serde_json::{Value, json};

use super::{ReadService, SearchHitTarget, StoreAnswer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// A workspace holding `files`, indexed with every visible file as text and the default
/// lockfile exclusion.
fn service(root: &Path, files: &[(&str, &str)]) -> TestResult<ReadService> {
    for (path, text) in files {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)?;
    }
    Ok(ReadService::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        HistoryConfiguration::default(),
    )?)
}

/// The store `service` publishes into, below `directory`.
async fn published(directory: &Path, service: &ReadService) -> TestResult<SearchIndex> {
    let store = SearchIndex::open(
        &directory.join("search.db"),
        SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_vector()
            .build(),
    )
    .await?;
    store
        .replace_lexical(&service.index_documents(), service.tree_revision())
        .await?;
    Ok(store)
}

fn weights() -> TestResult<RankingWeights> {
    let ranking = RankingConfiguration::default();
    Ok(RankingWeights::new(
        ranking.identifier_weight,
        ranking.lexical_weight,
        ranking.vector_weight,
        ranking.fusion_k,
    )?)
}

async fn phase(
    store: &SearchIndex,
    service: &ReadService,
    query: &ParsedQuery,
    phase: QueryPhase,
) -> TestResult<Vec<RankingInput>> {
    let RevisionScoped::Matched(ranking) = store
        .rank(service.tree_revision(), query, phase, 1_000)
        .await?
    else {
        return Err("the store holds the published revision".into());
    };
    Ok(ranking.into_inputs())
}

/// What the store answers for `query`, the document frequencies a body match reads
/// included, the way the server reads it.
async fn answered(
    store: &SearchIndex,
    service: &ReadService,
    query: &str,
) -> TestResult<StoreAnswer> {
    let parsed = ParsedQuery::parse(query)?;
    let precise = phase(store, service, &parsed, QueryPhase::Precise).await?;
    let broad = if parsed.has_broad_phase() {
        phase(store, service, &parsed, QueryPhase::Broad).await?
    } else {
        Vec::new()
    };
    let RevisionScoped::Matched(frequencies) = store
        .file_row_frequencies(service.tree_revision(), &BodyTerms::of(&parsed))
        .await?
    else {
        return Err("the store holds the published revision".into());
    };
    Ok(StoreAnswer::new(precise, broad, weights()?).with_file_rows(Some(frequencies)))
}

/// Each hit as `path#qualified_name` for a declaration and `path` for a file, in order.
fn spelled(result: &SearchResult) -> Vec<String> {
    result
        .results
        .iter()
        .map(|hit| {
            let path = hit.path.as_ref().map_or("", |path| path.0.as_str());
            match &hit.hit {
                SearchHitTarget::Symbol { symbol } => format!("{path}#{}", symbol.name),
                _ => path.to_owned(),
            }
        })
        .collect()
}

fn search(service: &ReadService, store: &StoreAnswer, request: Value) -> TestResult<SearchResult> {
    let params: SearchParams = serde_json::from_value(request)?;
    Ok(service.search(&params, store)?)
}

const BODIES: &str = "pub fn alpha() -> u32 {\n    let quokka = 1;\n    1\n}\n\n\
                      pub fn beta() -> u32 {\n    let quokka = 2;\n    quokka + quokka\n}\n\n\
                      pub fn gamma() {}\n";

/// A word only declaration bodies hold answers those declarations, matched through their
/// content, and never the file row that carried them under `target: "symbol"`; the
/// declaration holding the term more often ranks first.
#[tokio::test]
async fn a_body_term_answers_the_declarations_holding_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path(), &[("src/lib.rs", BODIES)])?;
    let store = published(directory.path(), &service).await?;
    let answer = answered(&store, &service, "quokka").await?;

    let symbols = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "symbol"}),
    )?;
    assert_eq!(spelled(&symbols), ["src/lib.rs#beta", "src/lib.rs#alpha"]);
    for hit in &symbols.results {
        assert_eq!(hit.matched_by, [MatchedField::Content], "{hit:?}");
    }

    let everything = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "all"}),
    )?;
    assert_eq!(
        spelled(&everything),
        ["src/lib.rs#beta", "src/lib.rs#alpha", "src/lib.rs"],
        "the file row keeps its place after the declarations found in it"
    );

    let files = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "file"}),
    )?;
    assert_eq!(spelled(&files), ["src/lib.rs"]);

    let without = StoreAnswer::new(
        answer.precise().to_vec(),
        answer.broad().to_vec(),
        weights()?,
    );
    let unmatched = search(
        &service,
        &without,
        json!({"query": "quokka", "target": "symbol"}),
    )?;
    assert!(
        unmatched.results.is_empty(),
        "without document frequencies no body match runs: {:?}",
        spelled(&unmatched)
    );
    Ok(())
}

/// A direct match keeps its own place before the first file row, and the declarations
/// the file rows hold follow it, a declaration already placed never twice.
#[tokio::test]
async fn body_matches_follow_a_direct_match_and_never_repeat_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(
        directory.path(),
        &[
            ("src/lib.rs", BODIES),
            ("src/quokka.rs", "pub fn quokka() -> u32 {\n    7\n}\n"),
        ],
    )?;
    let store = published(directory.path(), &service).await?;
    let answer = answered(&store, &service, "quokka").await?;
    let symbols = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "symbol"}),
    )?;
    let spelled = spelled(&symbols);
    assert_eq!(
        spelled.first().map(String::as_str),
        Some("src/quokka.rs#quokka")
    );
    assert_eq!(
        spelled
            .iter()
            .filter(|hit| hit.as_str() == "src/quokka.rs#quokka")
            .count(),
        1,
        "{spelled:?}"
    );
    assert!(
        spelled.contains(&"src/lib.rs#alpha".to_owned()),
        "{spelled:?}"
    );
    assert!(
        spelled.contains(&"src/lib.rs#beta".to_owned()),
        "{spelled:?}"
    );
    Ok(())
}

/// Pages cut the list the body matches expanded: `total_pages` counts every declaration
/// the pool placed, and the second page serves the next window of the same order.
#[tokio::test]
async fn pages_window_the_list_body_matches_expanded() -> TestResult {
    let directory = tempfile::tempdir()?;
    let bodies = (0..5)
        .map(|index| format!("pub fn holder_{index}() {{\n    let quokka = {index};\n}}\n\n"))
        .collect::<Vec<_>>()
        .concat();
    let service = service(directory.path(), &[("src/lib.rs", &bodies)])?;
    let store = published(directory.path(), &service).await?;
    let answer = answered(&store, &service, "quokka").await?;
    let whole = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "symbol", "limit": 10}),
    )?;
    assert_eq!(whole.results.len(), 5);
    let mut paged = Vec::new();
    for page_index in 0..3 {
        let page = search(
            &service,
            &answer,
            json!({"query": "quokka", "target": "symbol", "limit": 2, "page_index": page_index}),
        )?;
        assert_eq!(page.pagination.total_pages, 3);
        paged.extend(spelled(&page));
    }
    assert_eq!(paged, spelled(&whole), "three windows of one ordered list");
    Ok(())
}

/// A lockfile answers no search; naming it in `paths.include` says why, and
/// `paths.force_include` reaches a parsed one for the one request.
#[tokio::test]
async fn an_excluded_lockfile_answers_no_search_and_its_selection_warns() -> TestResult {
    let directory = tempfile::tempdir()?;
    let lock = "version = 4\n\n[[package]]\nname = \"quokka\"\n";
    let service = service(
        directory.path(),
        &[
            ("Cargo.lock", lock),
            ("web/deno.lock", "{\"quokka\": \"1.0\"}\n"),
            ("web/package-lock.json", "{\"quokka\": \"1.0\"}\n"),
            ("src/lib.rs", "pub fn quokka() {}\n"),
        ],
    )?;
    let store = published(directory.path(), &service).await?;
    let answer = answered(&store, &service, "quokka").await?;

    let plain = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "file"}),
    )?;
    assert_eq!(spelled(&plain), ["src/lib.rs"]);
    assert!(
        !plain
            .warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::LockfileExcluded { .. })),
        "a request selecting no lockfile carries no lockfile warning"
    );

    let selected = search(
        &service,
        &answer,
        json!({"query": "quokka", "paths": {"include": ["*.lock"]}}),
    )?;
    assert!(selected.results.is_empty(), "{:?}", spelled(&selected));
    let files = selected
        .warnings
        .iter()
        .find_map(|warning| match warning {
            ReadWarning::LockfileExcluded { files, detail } => {
                assert!(detail.contains("excluded_lockfiles"), "{detail}");
                Some(files.iter().map(|file| file.0.as_str()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .ok_or("selecting lockfiles warns")?;
    assert_eq!(
        files,
        ["rift://file/Cargo.lock", "rift://file/web/deno.lock"]
    );

    let symbols = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "symbol"}),
    )?;
    assert_eq!(
        spelled(&symbols),
        ["src/lib.rs#quokka"],
        "no lockfile key is a declaration"
    );
    let forced = search(
        &service,
        &answer,
        json!({
            "query": "quokka",
            "target": "symbol",
            "paths": {"force_include": ["web/package-lock.json"]}
        }),
    )?;
    assert!(
        spelled(&forced)
            .iter()
            .any(|hit| hit.starts_with("web/package-lock.json#")),
        "force_include reaches the parsed lockfile's keys: {:?}",
        spelled(&forced)
    );
    Ok(())
}

/// Emptying the exclusion list indexes every lockfile again.
#[tokio::test]
async fn an_empty_exclusion_list_indexes_every_lockfile() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("Cargo.lock"), "name = \"quokka\"\n")?;
    let service = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default().excluding_lockfiles(Vec::new()),
        HistoryConfiguration::default(),
    )?;
    let store = published(directory.path(), &service).await?;
    let answer = answered(&store, &service, "quokka").await?;
    let result = search(
        &service,
        &answer,
        json!({"query": "quokka", "target": "file"}),
    )?;
    assert_eq!(spelled(&result), ["Cargo.lock"]);
    Ok(())
}
