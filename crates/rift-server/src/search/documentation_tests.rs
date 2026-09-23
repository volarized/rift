use std::{error::Error, fs, path::Path, sync::Arc};

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_dependency::CatalogEntry;
use rift_index::{
    DependencyIndex, DependencyIndexLimits, LexicalIndexLimits, PackageIndex, WorkspaceIndexLimits,
    package_files,
};
use rift_protocol::configuration::{HistoryConfiguration, RankingConfiguration};
use rift_protocol::read::{
    GetSymbolParams, PackageIdentity, ReadWarning, SearchHit, SearchParams, SearchResult,
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

async fn scoped_documentation_search(
    service: &ReadService,
    store: &SearchIndex,
    scope: &str,
    dependency_context: Option<&rift_dependency::DependencyContext>,
) -> TestResult<SearchResult> {
    let query = "shared compass reference";
    let params: SearchParams = serde_json::from_value(json!({
        "query": query,
        "scope": scope,
        "target": "documentation",
        "limit": 100
    }))?;
    let answer = answer(store, service, query).await?;
    Ok(match dependency_context {
        Some(context) => service.search_with_dependency_context(&params, &answer, context)?,
        None => service.search(&params, &answer)?,
    })
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

#[tokio::test]
async fn page_excerpt_budget_respects_utf8_and_coalesces_same_source_warnings() -> TestResult {
    let directory = tempfile::tempdir()?;
    let first = format!("xx compass {}\n", "é".repeat(10_000));
    let second = format!("\nxx compass {}\n", "界".repeat(10_000));
    fs::write(
        directory.path().join("large.md"),
        format!("{first}{second}\n{first}"),
    )?;
    let service = ReadService::build(
        directory.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::new(vec!["selected.txt".to_owned()], 20_013),
        HistoryConfiguration::default(),
    )?;
    let store = stored(directory.path(), &service).await?;
    let params: SearchParams = serde_json::from_value(json!({
        "query":"compass",
        "target":"documentation",
        "include":["source"],
        "limit":100
    }))?;

    let result = service.search(&params, &answer(&store, &service, "compass").await?)?;

    assert_eq!(result.results.len(), 3, "{result:#?}");
    let excerpts = result
        .results
        .iter()
        .filter_map(|hit| hit.source.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(excerpts.len(), 2, "{result:#?}");
    let excerpt_limit = rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
    assert_eq!(
        excerpts.iter().map(|source| source.len()).sum::<usize>(),
        excerpt_limit
    );
    let excerpt = excerpts
        .iter()
        .max_by_key(|source| source.len())
        .ok_or("bounded excerpt required")?;
    assert!(excerpt.len() <= excerpt_limit);
    assert!(excerpt.len() >= excerpt_limit - 2);
    assert!(excerpt.ends_with('é') || excerpt.ends_with('界'));

    let warnings = result
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            ReadWarning::Documentation { warning }
                if warning.kind
                    == rift_protocol::documentation::DocumentationWarningKind::LimitExceeded =>
            {
                Some(warning)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "{result:#?}");
    assert_eq!(warnings[0].count, 3);
    assert!(matches!(
        &warnings[0].source.source,
        rift_protocol::documentation::DocumentationSourceIdentity::Project { path }
            if path.0 == "large.md"
    ));
    assert_eq!(
        warnings[0].stage,
        rift_protocol::documentation::DocumentationStage::Index
    );
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

fn package_index(
    root: &Path,
    name: &str,
    version: &str,
    content: &str,
) -> TestResult<PackageIndex> {
    fs::create_dir_all(root.join("src"))?;
    fs::write(root.join("src/lib.rs"), "pub struct Beacon;\n")?;
    fs::write(root.join("README.md"), content)?;
    let entry = CatalogEntry::dependency(
        PackageIdentity {
            manager: "cargo".to_owned(),
            name: name.to_owned(),
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
    let third = tempfile::tempdir()?;
    let mut dependencies = DependencyIndex::empty(DependencyIndexLimits::default());
    dependencies.insert(package_index(
        first.path(),
        "beacon-kit",
        "1.0.0",
        "# Beacon Kit\n\nShared navigation reference. Release one.\n",
    )?)?;
    dependencies.insert(package_index(
        second.path(),
        "beacon-kit",
        "2.0.0",
        "# Beacon Kit\n\nShared navigation reference. Release two.\n",
    )?)?;
    dependencies.insert(package_index(
        third.path(),
        "navigation-kit",
        "1.0.0",
        "# Navigation Kit\n\nShared navigation reference. Alternate package.\n",
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
    assert_eq!(result.results.len(), 3, "{result:#?}");
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
    assert_eq!(versions.len(), 3);
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
    assert!(
        versions
            .iter()
            .any(|unit| unit.contains("navigation-kit@1.0.0"))
    );
    Ok(())
}

#[tokio::test]
async fn documentation_scope_selects_workspace_and_cached_package_sources() -> TestResult {
    use tracing_subscriber::layer::SubscriberExt as _;

    use crate::packages::tests::{RESOLVE_SPAN, RecordedSpans, context_naming_one_package};

    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("src"))?;
    fs::write(
        directory.path().join("src/lib.rs"),
        "pub struct WorkspaceBeacon;\n",
    )?;
    fs::write(
        directory.path().join("guide.md"),
        "# Workspace guide\n\nShared compass reference.\n",
    )?;
    let workspace = service(directory.path())?;
    let store = stored(directory.path(), &workspace).await?;
    let package_root = tempfile::tempdir()?;
    let mut packages = DependencyIndex::empty(DependencyIndexLimits::default());
    packages.insert(package_index(
        package_root.path(),
        "absent-probe",
        "1.0.0",
        "# Package guide\n\nShared compass reference.\n",
    )?)?;
    let local =
        service(directory.path())?.with_packages(Arc::new(PackageBranch::from_index(packages)));
    let local_context = context_naming_one_package();

    let recorded = RecordedSpans::default();
    let local_result = {
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(recorded.clone()));
        scoped_documentation_search(&local, &store, "local", Some(&local_context)).await?
    };
    assert_eq!(recorded.named(RESOLVE_SPAN), 0, "{recorded:?}");
    assert_eq!(local_result.results.len(), 2, "{local_result:#?}");
    assert_eq!(
        local_result
            .results
            .iter()
            .filter(|hit| hit.unit.is_some())
            .count(),
        1
    );

    let mut packages = DependencyIndex::empty(DependencyIndexLimits::default());
    let second_package_root = tempfile::tempdir()?;
    packages.insert(package_index(
        second_package_root.path(),
        "absent-probe",
        "1.0.0",
        "# Package guide\n\nShared compass reference.\n",
    )?)?;
    let global =
        service(directory.path())?.with_packages(Arc::new(PackageBranch::from_index(packages)));
    let global_result = scoped_documentation_search(&global, &store, "global", None).await?;
    assert_eq!(global_result.results.len(), 1, "{global_result:#?}");
    assert!(global_result.results[0].unit.is_some());

    let mut packages = DependencyIndex::empty(DependencyIndexLimits::default());
    let third_package_root = tempfile::tempdir()?;
    packages.insert(package_index(
        third_package_root.path(),
        "absent-probe",
        "1.0.0",
        "# Package guide\n\nShared compass reference.\n",
    )?)?;
    let all =
        service(directory.path())?.with_packages(Arc::new(PackageBranch::from_index(packages)));
    let all_result = scoped_documentation_search(&all, &store, "all", None).await?;
    assert_eq!(all_result.results.len(), 2, "{all_result:#?}");
    assert_eq!(
        all_result
            .results
            .iter()
            .filter(|hit| hit.unit.is_some())
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn get_symbol_documentation_keeps_scope_hits_and_exact_source_context() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("src"))?;
    fs::write(directory.path().join("src/lib.rs"), "pub struct Beacon;\n")?;
    fs::write(
        directory.path().join("guide.md"),
        "Use `Beacon` for local navigation.\n",
    )?;
    let package_root = tempfile::tempdir()?;
    let mut packages = DependencyIndex::empty(DependencyIndexLimits::default());
    packages.insert(package_index(
        package_root.path(),
        "absent-probe",
        "1.0.0",
        "Use `Beacon` for package navigation.\n",
    )?)?;
    let package_branch = Arc::new(PackageBranch::from_index(packages));
    let service = service(directory.path())?.with_packages(package_branch);

    let local_without: GetSymbolParams = serde_json::from_value(json!({
        "name": "Beacon",
        "scope": "local"
    }))?;
    let local_with: GetSymbolParams = serde_json::from_value(json!({
        "name": "Beacon",
        "scope": "local",
        "include": ["documentation"]
    }))?;
    let without = service.get_symbol(&local_without)?;
    let local = service.get_symbol(&local_with)?;
    assert_eq!(local.hits.len(), without.hits.len());
    assert_eq!(local.hits.len(), 1);
    assert!(local.hits[0].unit.is_none());
    assert_eq!(
        local.hits[0]
            .documentation
            .as_ref()
            .map(|context| context.references.len()),
        Some(1)
    );
    assert!(
        serde_json::to_value(without)?["hits"][0]
            .get("documentation")
            .is_none()
    );

    let global_params: GetSymbolParams = serde_json::from_value(json!({
        "name": "Beacon",
        "scope": "global",
        "include": ["documentation"]
    }))?;
    let global = service.get_symbol(&global_params)?;
    assert_eq!(global.hits.len(), 1);
    assert!(global.hits[0].unit.is_some());
    assert_eq!(
        global.hits[0]
            .documentation
            .as_ref()
            .map(|context| context.references.len()),
        Some(1)
    );

    let all_params: GetSymbolParams = serde_json::from_value(json!({
        "name": "Beacon",
        "scope": "all",
        "include": ["documentation"]
    }))?;
    let all = service.get_symbol(&all_params)?;
    assert_eq!(all.hits.len(), 2);
    assert!(all.hits[0].unit.is_none());
    assert!(all.hits[1].unit.is_some());
    assert!(all.hits.iter().all(|hit| {
        hit.documentation
            .as_ref()
            .is_some_and(|context| context.references.len() == 1)
    }));
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
