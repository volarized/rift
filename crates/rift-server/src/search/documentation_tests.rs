use std::{error::Error, fs};

use rift_core::{SourceVisibility, TextFileInclusion};
use rift_index::{LexicalIndexLimits, WorkspaceIndexLimits};
use rift_protocol::configuration::{HistoryConfiguration, RankingConfiguration};
use rift_protocol::read::{GetSymbolParams, SearchParams};
use rift_ranking::{ParsedQuery, QueryPhase, RankingWeights};
use rift_search::{RevisionScoped, SearchIndex, SearchIndexLimits};
use serde_json::json;

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
