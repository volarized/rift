use std::{error::Error, fs, path::Path, sync::Arc};

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_dependency::CatalogEntry;
use rift_index::{
    DependencyIndex, DependencyIndexLimits, LexicalIndexLimits, PackageIndex, WorkspaceIndexLimits,
    package_files,
};
use rift_protocol::configuration::{HistoryConfiguration, RankingConfiguration};
use rift_protocol::read::{
    GetSymbolParams, PackageIdentity, SearchHit, SearchParams, SearchResult,
};
use rift_ranking::{ParsedQuery, QueryPhase, RankingInput, RankingWeights};
use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits};
use rift_syntax::ShippedLanguage;
use serde_json::json;

use crate::packages::PackageBranch;

use super::{ReadService, SearchHitTarget, StoreAnswer};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn service(root: &std::path::Path) -> TestResult<ReadService> {
    Ok(ReadService::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::new(vec!["selected.txt".to_owned()], 1_048_576),
        HistoryConfiguration::default(),
    )?)
}

async fn stored(root: &std::path::Path, service: &ReadService) -> TestResult<SearchIndex> {
    let store = SearchIndex::open(
        &root.join("search.db"),
        SearchIndexLimits::builder(LexicalIndexLimits::default())
            .disable_vector()
            .build(),
    )
    .await?;
    store
        .replace_lexical_with_documentation(
            &service.index_documents(),
            service.tree_revision(),
            service.index().documentation(),
        )
        .await?;
    Ok(store)
}

async fn stored_without_documentation(
    root: &std::path::Path,
    service: &ReadService,
) -> TestResult<SearchIndex> {
    let store = SearchIndex::open(
        &root.join("baseline.db"),
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

async fn answer(
    store: &SearchIndex,
    service: &ReadService,
    query: &str,
) -> TestResult<StoreAnswer> {
    let query = ParsedQuery::parse(query)?;
    let RevisionScoped::Matched(precise) = store
        .rank(service.tree_revision(), &query, QueryPhase::Precise, 100)
        .await?
    else {
        return Err("matching revision required".into());
    };
    let weights = RankingConfiguration::default();
    Ok(StoreAnswer::new(
        precise.into_inputs(),
        Vec::new(),
        RankingWeights::new(
            weights.identifier_weight,
            weights.lexical_weight,
            weights.vector_weight,
            weights.fusion_k,
        )?,
    ))
}

async fn search_for(
    store: &SearchIndex,
    service: &ReadService,
    query: &str,
    target: &str,
) -> TestResult<SearchResult> {
    let params: SearchParams = serde_json::from_value(json!({
        "query": query,
        "target": target,
        "include": ["source", "score"],
        "limit": 100
    }))?;
    Ok(service.search(&params, &answer(store, service, query).await?)?)
}

fn hit_identity(hit: &SearchHit) -> Option<String> {
    match &hit.hit {
        SearchHitTarget::Documentation { documentation } => {
            Some(documentation.block.identity.0.clone())
        }
        SearchHitTarget::Symbol { symbol } => symbol.id.as_ref().map(|identity| identity.0.clone()),
        SearchHitTarget::Node { node } => Some(node.0.clone()),
        SearchHitTarget::File { .. } => hit.path.as_ref().map(|path| path.0.clone()),
    }
}

fn ranked_identities(answer: &StoreAnswer) -> Vec<String> {
    answer
        .precise()
        .iter()
        .flat_map(RankingInput::order)
        .map(|ranked| ranked.identity().as_str().to_owned())
        .collect()
}

#[tokio::test]
async fn required_documentation_formats_search_captured_content() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("guide.md"),
        "# Guide\r\n\r\ncompass markdown\r\n",
    )?;
    fs::write(
        directory.path().join("guide.mdx"),
        "# Guide\n\ncompass mdx\n\n<Component secretjsx />\n",
    )?;
    fs::write(
        directory.path().join("guide.rst"),
        "Guide\n=====\n\ncompass rst\n",
    )?;
    fs::write(directory.path().join("selected.txt"), "compass selected")?;
    fs::write(directory.path().join("ignored.txt"), "compass ignored")?;
    fs::write(
        directory.path().join("guide.ipynb"),
        r##"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"id":"guide","cell_type":"markdown","source":["# Notebook\n","\ncompass notebook\n"],"outputs":[{"text":"secretoutput"}]},{"id":"code","cell_type":"code","source":"compass()","outputs":[{"text":"secretoutput"}]}]}"##,
    )?;
    let service = service(directory.path())?;
    let store = stored(directory.path(), &service).await?;
    let params: SearchParams = serde_json::from_value(
        json!({"query":"compass","target":"documentation","include":["source"],"limit":100}),
    )?;
    let results = service.search(&params, &answer(&store, &service, "compass").await?)?;
    assert_eq!(results.results.len(), 6, "{results:#?}");
    for hit in &results.results {
        assert!(matches!(hit.hit, SearchHitTarget::Documentation { .. }));
        assert!(
            hit.source
                .as_ref()
                .is_some_and(|source| source.contains("compass"))
        );
        assert_ne!(
            hit.path.as_ref().map(|path| path.0.as_str()),
            Some("ignored.txt")
        );
    }
    for absent in ["secretoutput", "secretjsx", "nbformat"] {
        let mut params = params.clone();
        params.query = Some(absent.to_owned());
        assert!(
            service
                .search(&params, &answer(&store, &service, absent).await?)?
                .results
                .is_empty(),
            "{absent}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn heading_and_baseline_match_project_once_before_pagination() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("guide.md"),
        "# Compass\n\nBody text.\n",
    )?;
    let service = service(directory.path())?;
    let store = stored(directory.path(), &service).await?;
    for target in ["documentation", "all"] {
        let params: SearchParams = serde_json::from_value(
            json!({"query":"Compass","target":target,"limit":1,"include":["source"]}),
        )?;
        let result = service.search(&params, &answer(&store, &service, "Compass").await?)?;
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.pagination.total_pages, 1);
        assert!(matches!(
            result.results[0].hit,
            SearchHitTarget::Documentation { .. }
        ));
    }
    let params: SearchParams =
        serde_json::from_value(json!({"query":"Compass","target":"symbol"}))?;
    let result = service.search(&params, &answer(&store, &service, "Compass").await?)?;
    assert_eq!(result.results.len(), 1);
    assert!(matches!(
        result.results[0].hit,
        SearchHitTarget::Symbol { .. }
    ));
    Ok(())
}

#[test]
fn documentation_include_reads_exact_reverse_references_and_is_optional() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub struct Compass;\n")?;
    fs::write(
        directory.path().join("guide.md"),
        "Use `Compass` for navigation.\n",
    )?;
    let service = service(directory.path())?;
    let params: GetSymbolParams = serde_json::from_value(json!({"name":"Compass"}))?;
    let baseline = service.get_symbol(&params)?;
    assert!(
        serde_json::to_value(&baseline)?["hits"][0]
            .get("documentation")
            .is_none()
    );
    let params: GetSymbolParams =
        serde_json::from_value(json!({"name":"Compass","include":["documentation"]}))?;
    let result = service.get_symbol(&params)?;
    let context = result.hits[0]
        .documentation
        .as_ref()
        .ok_or("documentation context required")?;
    assert_eq!(context.references.len(), 1);
    assert_eq!(
        Some(&context.references[0].reference.target),
        result.hits[0].symbol.id.as_ref()
    );
    assert_eq!(
        context.references[0].excerpt.as_deref(),
        Some("Use `Compass` for navigation.\n")
    );
    Ok(())
}

#[tokio::test]
async fn attached_documentation_keeps_symbol_owner_and_outside_file_matches() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        "/// compass attached.\npub fn travel() { let outside = \"outsideguide\"; }\n",
    )?;
    let service = service(directory.path())?;
    let store = stored(directory.path(), &service).await?;
    let documentation: SearchParams = serde_json::from_value(
        json!({"query":"compass","target":"documentation","include":["source"]}),
    )?;
    let result = service.search(&documentation, &answer(&store, &service, "compass").await?)?;
    assert_eq!(result.results.len(), 1, "{result:#?}");
    let SearchHitTarget::Documentation { documentation } = &result.results[0].hit else {
        return Err("attached documentation required".into());
    };
    assert!(documentation.block.symbol.is_some());
    assert_eq!(
        result.results[0].source.as_deref(),
        Some("/// compass attached.\n")
    );
    let all: SearchParams = serde_json::from_value(json!({"query":"compass","target":"all"}))?;
    let result = service.search(&all, &answer(&store, &service, "compass").await?)?;
    assert_eq!(result.results.len(), 1, "{result:#?}");
    assert!(matches!(
        result.results[0].hit,
        SearchHitTarget::Symbol { .. }
    ));
    let outside: SearchParams =
        serde_json::from_value(json!({"query":"outsideguide","target":"all"}))?;
    let result = service.search(&outside, &answer(&store, &service, "outsideguide").await?)?;
    assert!(
        result
            .results
            .iter()
            .any(|hit| matches!(hit.hit, SearchHitTarget::File { .. })),
        "{result:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn exact_document_questions_project_expected_blocks_without_changing_symbols() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("docs"))?;
    fs::create_dir_all(directory.path().join("src"))?;
    fs::write(directory.path().join("src/lib.rs"), "pub struct Compass;\n")?;
    fs::write(
        directory.path().join("docs/api.md"),
        "# Navigation API\n\nThe celestial sextant measures parallax.\n",
    )?;
    fs::write(
        directory.path().join("docs/tutorial.md"),
        "# Field Tutorial\n\nThe lunar drift chart gives a route.\n",
    )?;
    fs::write(
        directory.path().join("docs/repeated.md"),
        "# Examples\n\n## Calibration\nThe red aperture aligns.\n\n## Calibration\nThe blue aperture records.\n",
    )?;
    fs::write(
        directory.path().join("docs/rust.md"),
        "# Rust Example\n\n```rust\nfn orbital_route() {}\n```\n",
    )?;
    let service = service(directory.path())?;
    let with_documentation = stored(directory.path(), &service).await?;
    let baseline = stored_without_documentation(directory.path(), &service).await?;
    for question in document_questions() {
        assert_document_question(&service, &with_documentation, &baseline, question).await?;
    }
    assert_symbol_query_unchanged(&service, &with_documentation, &baseline).await?;
    Ok(())
}

type DocumentQuestion<'a> = (
    &'a str,
    &'a str,
    &'a [&'a str],
    &'a str,
    &'a [&'a str],
    &'a [&'a str],
);

fn document_questions() -> [DocumentQuestion<'static>; 4] {
    [
        (
            "celestial sextant",
            "docs/api.md",
            &["Navigation API"][..],
            "6aa1b2916828c61e9ea5023fbe94e58ae6467a0c81ad4e3b95448222b7fa6ada",
            &[
                "docs/api.md",
                "rift://symbol/markdown/docs/api.md/Navigation%20API",
            ][..],
            &["rift://symbol/markdown/docs/api.md/Navigation%20API"][..],
        ),
        (
            "lunar drift",
            "docs/tutorial.md",
            &["Field Tutorial"][..],
            "97e7e5894ad3524019eb636fdbc15f9c23694e8cf98c2caef4aa8c3dad748e02",
            &[
                "docs/tutorial.md",
                "rift://symbol/markdown/docs/tutorial.md/Field%20Tutorial",
            ][..],
            &["rift://symbol/markdown/docs/tutorial.md/Field%20Tutorial"][..],
        ),
        (
            "records",
            "docs/repeated.md",
            &["Examples", "Examples > Calibration~2"][..],
            "f029465cd85732d93c71c1c5529623413a989f85526db2c5f79c9082470b899f",
            &[
                "rift://symbol/markdown/docs/repeated.md/Examples%20%3E%20Calibration~2",
                "rift://symbol/markdown/docs/repeated.md/Examples",
                "docs/repeated.md",
            ][..],
            &[
                "rift://symbol/markdown/docs/repeated.md/Examples%20%3E%20Calibration~2",
                "rift://symbol/markdown/docs/repeated.md/Examples",
            ][..],
        ),
        (
            "orbital_route",
            "docs/rust.md",
            &["Rust Example"][..],
            "0f6ff8c17f7f5e80fb03a0d9e73667bb629b52b42ad5d6aa8b2b4e98f98a4ae2",
            &[
                "docs/rust.md",
                "rift://symbol/markdown/docs/rust.md/Rust%20Example",
            ][..],
            &["rift://symbol/markdown/docs/rust.md/Rust%20Example"][..],
        ),
    ]
}

async fn assert_document_question(
    service: &ReadService,
    with_documentation: &SearchIndex,
    baseline: &SearchIndex,
    question_data: DocumentQuestion<'_>,
) -> TestResult {
    let (
        question,
        path,
        headings,
        expected_block,
        expected_baseline_ranking,
        expected_baseline_symbols,
    ) = question_data;
    let baseline_answer = answer(baseline, service, question).await?;
    let current_answer = answer(with_documentation, service, question).await?;
    assert_eq!(
        ranked_identities(&baseline_answer),
        ranked_identities(&current_answer),
        "documentation metadata changed ranked content rows for {question}"
    );
    assert!(
        !baseline_answer.precise().is_empty(),
        "baseline ranking must find content for {question}"
    );
    assert_eq!(
        ranked_identities(&baseline_answer),
        expected_baseline_ranking,
        "baseline top-rank identity for {question}"
    );
    let symbol_baseline = search_for(baseline, service, question, "symbol").await?;
    assert_eq!(
        symbol_baseline
            .results
            .iter()
            .map(hit_identity)
            .collect::<Vec<_>>(),
        expected_baseline_symbols
            .iter()
            .map(|identity| Some((*identity).to_owned()))
            .collect::<Vec<_>>(),
        "symbol baseline identities for {question}"
    );

    let result = search_for(with_documentation, service, question, "documentation").await?;
    assert_eq!(result.results.len(), 1, "question={question}: {result:#?}");
    let hit = &result.results[0];
    let SearchHitTarget::Documentation { documentation } = &hit.hit else {
        return Err(format!("documentation result required for {question}").into());
    };
    assert_eq!(hit.path.as_ref().map(|path| path.0.as_str()), Some(path));
    assert_eq!(
        documentation
            .block
            .heading_path
            .iter()
            .map(|heading| heading.name.as_str())
            .collect::<Vec<_>>(),
        headings
    );
    assert_eq!(documentation.block.identity.0, expected_block);
    assert!(
        hit.source
            .as_deref()
            .is_some_and(|source| source.contains(question))
    );
    Ok(())
}

async fn assert_symbol_query_unchanged(
    service: &ReadService,
    with_documentation: &SearchIndex,
    baseline: &SearchIndex,
) -> TestResult {
    let baseline_symbols = search_for(baseline, service, "Compass", "symbol").await?;
    let documented_symbols = search_for(with_documentation, service, "Compass", "symbol").await?;
    let expected_symbol = "rift://symbol/rust/src/lib.rs/Compass";
    assert_eq!(
        baseline_symbols
            .results
            .iter()
            .map(hit_identity)
            .collect::<Vec<_>>(),
        [Some(expected_symbol.to_owned())]
    );
    assert_eq!(
        baseline_symbols
            .results
            .iter()
            .map(hit_identity)
            .collect::<Vec<_>>(),
        documented_symbols
            .results
            .iter()
            .map(hit_identity)
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[tokio::test]
async fn attached_comment_overlap_keeps_one_existing_rank_and_symbol_owner() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::create_dir_all(directory.path().join("src"))?;
    fs::write(
        directory.path().join("src/lib.rs"),
        "/// Measures transit bearings.\npub fn calibrate() {}\n",
    )?;
    let service = service(directory.path())?;
    let store = stored(directory.path(), &service).await?;
    let query_answer = answer(&store, &service, "transit bearings").await?;
    let block = service
        .index()
        .documentation()
        .index()
        .blocks
        .iter()
        .find(|block| block.symbol.is_some())
        .ok_or("attached block required")?;
    let chunk_identities = block
        .chunks
        .iter()
        .map(|chunk| chunk.identity.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let overlapping = query_answer
        .precise()
        .iter()
        .map(|input| {
            input
                .order()
                .iter()
                .filter(|ranked| chunk_identities.contains(ranked.identity().as_str()))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        overlapping.iter().flatten().count(),
        1,
        "overlap={overlapping:?}"
    );
    assert!(
        query_answer.precise().iter().all(|input| input
            .order()
            .iter()
            .all(|ranked| { ranked.identity().as_str() != block.identity.0 })),
        "documentation block identity must not enter ranked content rows"
    );

    let documentation = search_for(&store, &service, "transit bearings", "documentation").await?;
    assert_eq!(documentation.results.len(), 1, "{documentation:#?}");
    let doc_hit = &documentation.results[0];
    let SearchHitTarget::Documentation { documentation } = &doc_hit.hit else {
        return Err("attached documentation hit required".into());
    };
    assert_eq!(documentation.block.identity.0, block.identity.0);
    assert_eq!(documentation.block.symbol, block.symbol);
    let best_per_input = query_answer
        .precise()
        .iter()
        .map(|input| {
            let best = input
                .order()
                .iter()
                .find(|ranked| chunk_identities.contains(ranked.identity().as_str()))
                .cloned();
            best.map_or_else(
                || RankingInput::unanswered(input.kind()),
                |ranked| RankingInput::new(input.kind(), vec![ranked]),
            )
        })
        .collect::<Vec<_>>();
    let best_answer = StoreAnswer::new(best_per_input, Vec::new(), query_answer.weights());
    let best_only =
        search_with_answer(&service, "transit bearings", "documentation", &best_answer)?;
    assert_eq!(best_only.results.len(), 1);
    assert_eq!(doc_hit.score, best_only.results[0].score);

    let all = search_for(&store, &service, "transit bearings", "all").await?;
    assert_eq!(all.results.len(), 1, "{all:#?}");
    assert_eq!(
        hit_identity(&all.results[0]),
        Some("rift://symbol/rust/src/lib.rs/calibrate".to_owned())
    );
    Ok(())
}

fn package_index(root: &Path, version: &str, content: &str) -> TestResult<PackageIndex> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/lib.rs"), "pub struct Beacon;\n")?;
    fs::write(root.join("README.md"), content)?;
    let entry = CatalogEntry::dependency(
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: "beacon-kit".to_owned(),
            version: version.to_owned(),
        },
        ShippedLanguage::Rust.language(),
        Some(root.to_path_buf()),
        true,
    );
    let files = package_files(&entry, &DependencyIndexLimits::default())?;
    Ok(PackageIndex::build(&entry, &files, 1)?)
}

#[tokio::test]
async fn same_name_package_versions_keep_distinct_documentation_sources() -> TestResult {
    let directory = tempfile::tempdir()?;
    let service = service(directory.path())?;
    let store = stored(directory.path(), &service).await?;
    let first = tempfile::tempdir()?;
    let second = tempfile::tempdir()?;
    let mut dependencies = DependencyIndex::empty(DependencyIndexLimits::default());
    dependencies.insert(package_index(
        first.path(),
        "1.0.0",
        "# Beacon Kit\n\nShared navigation reference. Release one.\n",
    )?)?;
    dependencies.insert(package_index(
        second.path(),
        "2.0.0",
        "# Beacon Kit\n\nShared navigation reference. Release two.\n",
    )?)?;
    let service = service.with_packages(Arc::new(PackageBranch::from_index(dependencies)));
    let params: SearchParams = serde_json::from_value(json!({
        "query": "shared navigation reference",
        "scope": "global",
        "target": "documentation",
        "include": ["source", "score"],
        "limit": 100
    }))?;
    let result = service.search(
        &params,
        &answer(&store, &service, "shared navigation reference").await?,
    )?;
    assert_eq!(result.results.len(), 2, "{result:#?}");
    let versions = result
        .results
        .iter()
        .map(|hit| {
            let SearchHitTarget::Documentation { documentation } = &hit.hit else {
                return Err("package documentation hit required".into());
            };
            assert_eq!(
                documentation.block.source.source,
                rift_protocol::documentation::DocumentationSourceIdentity::Package {
                    unit: hit.unit.clone().ok_or("package source unit required")?,
                }
            );
            let unit = hit.unit.as_ref().ok_or("package source unit required")?;
            Ok(unit.0.clone())
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(versions.len(), 2);
    assert!(
        versions
            .iter()
            .any(|unit| unit.contains("beacon-kit@1.0.0"))
    );
    assert!(
        versions
            .iter()
            .any(|unit| unit.contains("beacon-kit@2.0.0"))
    );
    Ok(())
}

fn search_with_answer(
    service: &ReadService,
    query: &str,
    target: &str,
    answer: &StoreAnswer,
) -> TestResult<SearchResult> {
    let params: SearchParams = serde_json::from_value(json!({
        "query": query,
        "target": target,
        "include": ["source", "score"],
        "limit": 100
    }))?;
    Ok(service.search(&params, answer)?)
}
