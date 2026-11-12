//! Exercises the public SPARQL route against authored bytes and a real embedded engine.

use std::collections::BTreeSet;
use std::fs;

use crate::workspace_client::{TestResult, query, query_after_population, served_workspace};
use rift_protocol::query::{QueryLanguage, QueryParams};
use serde_json::{Value, json};

const CONFIGURATION: &str = "[search.query]\npage_size=2\n[languages.python.lsp]\nembedded='ty'\n";
const METADATA: &str = "[project]\nname='graph-fixture'\nversion='0.1.0'\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['graph']\n";
const SOURCE: &str = "def graph_root() -> int:\n    return graph_leaf() + graph_branch() + graph_leaf()\n\n\
    def graph_branch() -> int:\n    return graph_mid()\n\n\
    def graph_mid() -> int:\n    return graph_leaf() + graph_root()\n\n\
    def graph_leaf() -> int:\n    return 1\n\n\
    def graph_orphan() -> int:\n    return 0\n";

async fn statement(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    text: impl Into<String>,
) -> TestResult<Value> {
    query_after_population(
        client,
        &QueryParams {
            query: text.into(),
            language: QueryLanguage::Sparql,
        },
    )
    .await
}

async fn identity(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
) -> TestResult<String> {
    let name = oxrdf::Literal::from(name);
    let answer = statement(
        client,
        format!("SELECT ?symbol {{ ?symbol a :Symbol; :name {name} }}"),
    )
    .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("expected one symbol: {answer}").into());
    };
    Ok(query::iri(row, "symbol")?.to_owned())
}

#[tokio::test]
async fn graph_paths_and_physical_usages_retain_exact_pages_after_edit() -> TestResult {
    let (directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", SOURCE)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let entry = identity(&client, "graph_root").await?;
    let middle = identity(&client, "graph_mid").await?;
    let leaf = identity(&client, "graph_leaf").await?;
    let orphan = identity(&client, "graph_orphan").await?;
    let path = statement(
        &client,
        format!("SELECT DISTINCT ?s {{ <{entry}> :calls/:calls/:calls ?s }}"),
    )
    .await?;
    let paths: BTreeSet<_> = query::rows(&path)?
        .iter()
        .map(|row| query::iri(row, "s"))
        .collect::<Result<_, _>>()?;
    assert_eq!(paths, BTreeSet::from([entry.as_str(), leaf.as_str()]));
    let incoming = statement(
        &client,
        format!("SELECT DISTINCT ?s {{ ?s :references <{leaf}> }}"),
    )
    .await?;
    let callers: BTreeSet<_> = query::rows(&incoming)?
        .iter()
        .map(|row| query::iri(row, "s"))
        .collect::<Result<_, _>>()?;
    assert_eq!(callers, BTreeSet::from([entry.as_str(), middle.as_str()]));
    let count = statement(
        &client,
        format!("SELECT (COUNT(?u) AS ?count) {{ <{leaf}> :usages ?u }}"),
    )
    .await?;
    let [row] = query::rows(&count)? else {
        return Err(format!("expected one count: {count}").into());
    };
    assert_eq!(query::literal(row, "count")?, "3");
    let empty = statement(&client, format!("ASK {{ <{orphan}> :usages ?u }}")).await?;
    assert_eq!(empty["answer"], json!({"result":"boolean", "value":false}));

    let first = statement(&client, format!("SELECT ?u ?start ?source {{ <{leaf}> :usages ?u . ?u :range/:start ?start; :source ?source }} ORDER BY ?start")).await?;
    assert_eq!(first["pagination"]["total_pages"], 2);
    let continuation = first["pagination"]["next_query"]
        .as_str()
        .ok_or("retained next query")?;
    fs::write(
        directory.path().join("graph.py"),
        format!("# shifted after capture\n{SOURCE}"),
    )?;
    let second = statement(&client, continuation).await?;
    assert_eq!(second["view"], first["view"]);
    let mut ranges = Vec::new();
    let mut usages = BTreeSet::new();
    for row in query::rows(&first)?.iter().chain(query::rows(&second)?) {
        assert_eq!(row["u"]["type"], "blank_node");
        assert!(usages.insert(row["u"]["value"].as_str().ok_or("usage identity")?));
        assert_eq!(query::literal(row, "source")?, "graph_leaf");
        ranges.push(query::literal(row, "start")?.parse::<usize>()?);
    }
    let authored: Vec<_> = SOURCE
        .match_indices("graph_leaf(")
        .map(|(start, _)| start)
        .filter(|start| SOURCE.get(start.saturating_sub(4)..*start) != Some("def "))
        .collect();
    assert_eq!(ranges, authored);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn retrieval_projections_keep_ranked_and_regex_results_and_refuse_missing_coverage()
-> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", SOURCE)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let leaf = identity(&client, "graph_leaf").await?;
    let ranked = statement(&client, "SELECT ?s { SERVICE :search { ?hit :query 'graph_leaf'; :target 'symbol'; :symbol ?s; :index ?index } } ORDER BY ?index LIMIT 1").await?;
    let [row] = query::rows(&ranked)? else {
        return Err(format!("expected one ranked hit: {ranked}").into());
    };
    assert_eq!(query::iri(row, "s")?, leaf);
    let regex = statement(&client, "SELECT ?start ?end { SERVICE :search { ?hit :pattern 'graph_leaf'; :target 'file'; :start ?start; :end ?end } } ORDER BY ?start LIMIT 2").await?;
    let rows = query::rows(&regex)?;
    assert_eq!(rows.len(), 2);
    for row in rows {
        let start = query::literal(row, "start")?.parse::<usize>()?;
        let end = query::literal(row, "end")?.parse::<usize>()?;
        assert_eq!(&SOURCE[start..end], "graph_leaf");
    }
    for text in [
        "ASK { SERVICE :search { ?hit :query 'graph_leaf'; :scope 'global' } }",
        "SELECT (COUNT(?hit) AS ?n) { SERVICE :search { ?hit :query 'graph_leaf'; :scope 'global' } }",
    ] {
        let unavailable = statement(&client, text).await?;
        assert_eq!(unavailable["outcome"], "unavailable", "{unavailable}");
        assert_eq!(
            unavailable["reason"], "insufficient_coverage",
            "{unavailable}"
        );
    }
    client.cancel().await?;
    Ok(())
}
