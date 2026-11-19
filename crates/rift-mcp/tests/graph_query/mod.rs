//! Exercises the public SPARQL route against authored bytes and a real embedded engine.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::workspace_client::{TestResult, query, query_after_population, served_workspace};
use rift_protocol::query::{QueryLanguage, QueryParams};
use serde_json::{Value, json};

const CONFIGURATION: &str = "[search.query]\n[languages.python.lsp]\nembedded='ty'\n";
const METADATA: &str = "[project]\nname='graph-fixture'\nversion='0.1.0'\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['graph']\n";
const SOURCE: &str = "def graph_root() -> int:\n    return graph_leaf() + graph_branch() + graph_leaf()\n\n\
    def graph_branch() -> int:\n    return graph_mid()\n\n\
    def graph_mid() -> int:\n    return graph_leaf() + graph_root()\n\n\
    def graph_leaf() -> int:\n    return 1\n\n\
    def graph_orphan() -> int:\n    return 0\n";

#[tokio::test]
async fn engine_inventory_resolves_inferred_calls_past_sixty_four_declarations() -> TestResult {
    let mut source = String::from(
        "class Worker:\n    def perform(self) -> int:\n        return 1\n\ndef worker() -> Worker:\n    return Worker()\n",
    );
    for index in 0..80 {
        write!(
            source,
            "\ndef caller_{index}() -> int:\n    return worker().perform()\n"
        )?;
    }
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", &source)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let answer = query_after_population(
        &client,
        &QueryParams {
            query: "SELECT ?from ?to WHERE { ?s :calls ?t . ?s :name ?from . ?t :name ?to FILTER(?to = 'perform') }".to_owned(),
            max_results: 100,
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(named_pairs(&answer, "from", "to")?.len(), 80, "{answer}");
    let occurrences = query_after_population(&client, &QueryParams {
        query: "SELECT ?caller ?u { ?caller :calls ?target . ?target :name 'perform'; :usages ?u . ?u :of ?caller; :kind 'calls' }".to_owned(),
        max_results: 100,
        ..Default::default()
    }).await?;
    assert_eq!(occurrences["pagination"]["total"], 80, "{occurrences}");
    client.cancel().await?;
    Ok(())
}

async fn statement(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    text: impl Into<String>,
) -> TestResult<Value> {
    query_after_population(
        client,
        &QueryParams {
            max_results: 2,
            query: text.into(),
            language: QueryLanguage::Sparql,
            ..Default::default()
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
async fn an_anchored_call_read_keeps_local_edges_beside_unaddressed_callees() -> TestResult {
    let source = "def leaf() -> int:\n    return 1\n\ndef root() -> int:\n    print('root')\n    return leaf()\n";
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", source)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let root = identity(&client, "root").await?;
    let query = format!("SELECT ?to {{ <{root}> :calls ?target . ?target :name ?to }}");
    let answer = statement(&client, query).await?;
    assert!(
        query::rows(&answer)?
            .iter()
            .any(|row| query::literal(row, "to").is_ok_and(|name| name == "leaf")),
        "{answer}"
    );
    assert!(
        warning_codes(&answer).contains("edge_coverage_missing"),
        "{answer}"
    );
    let count = statement(
        &client,
        format!("SELECT (COUNT(?t) AS ?n) {{ <{root}> :calls ?t }}"),
    )
    .await?;
    assert_eq!(count["reason"], "insufficient_coverage", "{count}");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn engine_calls_cover_value_initializers_and_module_statements() -> TestResult {
    let source = "class Worker:\n    def perform(self) -> int:\n        return 1\n\ndef worker() -> Worker:\n    return Worker()\n\nresult = worker().perform()\nworker().perform()\n";
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", source)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let calls = statement(
        &client,
        "SELECT ?from ?to { ?s :calls ?t . ?s :name ?from . ?t :name ?to; :name 'perform' }",
    )
    .await?;
    assert_eq!(calls["pagination"]["total"], 2, "{calls}");
    assert!(
        named_pairs(&calls, "from", "to")?.contains(&("result", "perform")),
        "{calls}"
    );
    let result = identity(&client, "result").await?;
    let anchored = statement(
        &client,
        format!("ASK {{ <{result}> :calls ?target . ?target :name 'perform' }}"),
    )
    .await?;
    assert_eq!(
        anchored["answer"],
        json!({"result":"boolean", "value":true}),
        "{anchored}"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn engine_inventory_follows_source_and_configuration_changes() -> TestResult {
    let source = "class First:\n    pass\n\nclass Second:\n    pass\n\ndef make() -> First:\n    return First()\n\nvalue = make()\n";
    let (directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", source)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let query = "SELECT ?type { ?s :name 'value'; :hasType ?t . ?t :name ?type }";
    let first = statement(&client, query).await?;
    assert_eq!(
        query::literal(&query::rows(&first)?[0], "type")?,
        "First",
        "{first}"
    );
    std::fs::write(
        directory.path().join("graph.py"),
        source
            .replace("-> First", "-> Second")
            .replace("return First()", "return Second()"),
    )?;
    let second = statement(&client, query).await?;
    assert_eq!(
        query::literal(&query::rows(&second)?[0], "type")?,
        "Second",
        "{second}"
    );
    let configuration = std::fs::read_to_string(directory.path().join("rift.toml"))?;
    std::fs::write(
        directory.path().join("rift.toml"),
        configuration.replace("[languages.python.lsp]\nembedded='ty'\n", ""),
    )?;
    let without_engine = statement(&client, query).await?;
    assert!(query::rows(&without_engine)?.is_empty(), "{without_engine}");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn engine_inventory_keeps_inferred_types_and_declared_heritage() -> TestResult {
    let source = "class Parent:\n    pass\n\nclass Child(Parent):\n    pass\n\nclass Grandchild(Child):\n    pass\n\ndef make() -> Child:\n    return Child()\n\nvalue = make()\n";
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", source)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let _value = identity(&client, "value").await?;
    let types = statement(
        &client,
        "SELECT ?from ?to { ?s :hasType ?t . ?s :name ?from . ?t :name ?to }",
    )
    .await?;
    assert!(
        named_pairs(&types, "from", "to")?.contains(&("value", "Child")),
        "{types}"
    );
    let child = identity(&client, "Child").await?;
    let incoming = statement(
        &client,
        format!("ASK {{ ?value :name 'value'; :hasType <{child}> }}"),
    )
    .await?;
    assert_eq!(
        incoming["answer"],
        json!({"result":"boolean", "value":true}),
        "{incoming}"
    );
    let heritage = statement(
        &client,
        "SELECT ?from ?to { ?s :extends ?t . ?s :name ?from . ?t :name ?to }",
    )
    .await?;
    assert!(
        named_pairs(&heritage, "from", "to")?.contains(&("Child", "Parent")),
        "{heritage}"
    );
    assert!(
        !named_pairs(&heritage, "from", "to")?.contains(&("Grandchild", "Parent")),
        "{heritage}"
    );
    assert!(
        !named_pairs(&heritage, "from", "to")?.contains(&("value", "Parent")),
        "{heritage}"
    );
    let value = identity(&client, "value").await?;
    let parent = identity(&client, "Parent").await?;
    let absent = statement(&client, format!("ASK {{ <{value}> :extends <{parent}> }}")).await?;
    assert_eq!(
        absent["answer"],
        json!({"result":"boolean", "value":false}),
        "{absent}"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn rust_analyzer_supplies_a_call_on_an_inferred_receiver() -> TestResult {
    if !super::live_engine_gate::engine_live() {
        return Ok(());
    }
    let files = [
        ("Cargo.toml", super::RUST_LIBRARY),
        (
            "Cargo.lock",
            "# This file is automatically @generated by Cargo.\n\
             # It is not intended for manual editing.\n\
             version = 4\n\n[[package]]\nname = \"walk_fixture\"\nversion = \"0.1.0\"\n",
        ),
        (
            "lib.rs",
            "pub struct Worker;\n\
             impl Worker { pub fn perform(&self) {} }\n\
             pub fn direct() {}\n\
             pub fn root() { direct(); let worker = Worker; worker.perform(); }\n",
        ),
    ];
    for configuration in [None, Some(super::LIVE_RUST_ENGINE.to_owned())] {
        let engine_enabled = configuration.is_some();
        let (directory, client, _server_task) = served_workspace(&files, configuration).await?;
        let root = identity(&client, "root").await?;
        let direct = identity(&client, "direct").await?;
        let captured = rift_server::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )?;
        let seed = rift_core::SymbolId::new(root.as_str())?;
        let outgoing = captured.edges().outgoing(&seed);
        assert!(
            outgoing
                .iter()
                .any(|edge| edge.to().as_str() == direct.as_str()),
            "captured static edge from {root} to {direct}: {outgoing:?}"
        );
        let exact = statement(&client, format!("ASK {{ <{root}> :calls <{direct}> }}")).await?;
        assert_eq!(
            exact["answer"],
            json!({"result":"boolean", "value":true}),
            "{exact}"
        );
        let static_call = statement(
            &client,
            format!("ASK {{ <{root}> :calls ?target . ?target :name 'direct' }}"),
        )
        .await?;
        assert_eq!(
            static_call["answer"],
            json!({"result":"boolean", "value":true}),
            "{static_call}"
        );
        let composed = statement(
            &client,
            format!("ASK {{ <{root}> :calls ?target . ?target :name 'perform' }}"),
        )
        .await?;
        if engine_enabled {
            assert_eq!(
                composed["answer"],
                json!({"result":"boolean", "value":true}),
                "{composed}"
            );
        } else {
            assert_eq!(composed["outcome"], "unavailable", "{composed}");
            assert_eq!(composed["reason"], "insufficient_coverage", "{composed}");
        }
        client.cancel().await?;
    }
    Ok(())
}

#[tokio::test]
async fn graph_paths_and_physical_usages_select_exact_pages() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
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
    assert_eq!(first["pagination"]["pages"], 2);
    let parameters = QueryParams {
        query: format!(
            "SELECT ?u ?start ?source {{ <{leaf}> :usages ?u . ?u :range/:start ?start; :source ?source }} ORDER BY ?start"
        ),
        max_results: 2,
        ..Default::default()
    };
    let parameters = query::result_page(&parameters, &first, 2)?;
    let second = query_after_population(&client, &parameters).await?;
    assert_eq!(second["revision"], first["revision"]);
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
    let ranked = statement(&client, "SELECT ?s ?kind { SERVICE :search { ?hit :query 'graph_leaf'; :target 'symbol'; :symbol ?s; :index ?index . ?s :kind ?kind; :kinds :callable } } ORDER BY ?index LIMIT 1").await?;
    let [row] = query::rows(&ranked)? else {
        return Err(format!("expected one ranked hit: {ranked}").into());
    };
    assert_eq!(query::iri(row, "s")?, leaf);
    assert_eq!(query::literal(row, "kind")?, "function");
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

fn warning_codes(answer: &Value) -> BTreeSet<&str> {
    answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|warning| warning["code"].as_str())
        .collect()
}

fn named_pairs<'a>(
    answer: &'a Value,
    from: &str,
    to: &str,
) -> TestResult<BTreeSet<(&'a str, &'a str)>> {
    query::rows(answer)?
        .iter()
        .map(|row| Ok((query::literal(row, from)?, query::literal(row, to)?)))
        .collect()
}

/// An unresolved framework in one package of the workspace leaves every captured
/// declaration in place, so the symbol inventory answers and names the framework
/// condition instead of refusing (#759).
#[tokio::test]
async fn an_unresolved_framework_reports_a_warning_beside_the_symbol_inventory() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("package.json", "{\"name\":\"core\",\"version\":\"1.0.0\"}"),
            (
                "src/core.ts",
                "export function construct_core(): number {\n  return 1;\n}\n",
            ),
            (
                "docs/package.json",
                "{\"name\":\"docs\",\"devDependencies\":{\"tailwindcss\":\"^4.0.0\"}}",
            ),
            (
                "docs/layout.tsx",
                "export function DocsLayout() {\n  return <div className=\"flex\" />;\n}\n",
            ),
        ],
        None,
    )
    .await?;
    let inventory = query_after_population(
        &client,
        &QueryParams {
            query: "SELECT ?name WHERE { ?s a :Symbol; :name ?name }".to_owned(),
            max_results: 100,
            ..Default::default()
        },
    )
    .await?;
    let names: BTreeSet<_> = query::rows(&inventory)?
        .iter()
        .map(|row| query::literal(row, "name"))
        .collect::<Result<_, _>>()?;
    assert!(
        names.contains("construct_core") && names.contains("DocsLayout"),
        "{inventory}"
    );
    assert!(
        warning_codes(&inventory).contains("framework_context_unresolved"),
        "{inventory}"
    );
    client.cancel().await?;
    Ok(())
}

const DEPENDENT_METADATA: &str = "[project]\nname='graph-app'\nversion='0.1.0'\ndependencies=['greeting']\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n";
const DEPENDENT_SOURCE: &str = "from greeting.core import greet\n\n\
    def app_helper() -> str:\n    return 'rift'\n\n\
    def app_caller() -> str:\n    return greet(app_helper())\n";

/// Captured static calls and references answer an inventory query in a project whose
/// dependency context names packages, and what the engine could not add is a warning,
/// not a refusal (#760).
#[tokio::test]
async fn captured_calls_answer_an_inventory_beside_a_dependency_context() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("pyproject.toml", DEPENDENT_METADATA),
            (
                "uv.lock",
                "version = 1\n[[package]]\nname='graph-app'\nversion='0.1.0'\nsource={editable='.'}\n[[package]]\nname='greeting'\nversion='1.0.0'\nsource={registry='https://pypi.org/simple'}\n",
            ),
            ("app.py", DEPENDENT_SOURCE),
        ],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    for (relation, statement) in [
        (
            "calls",
            "SELECT ?from ?to WHERE { ?s :calls ?t . ?s :name ?from . ?t :name ?to }",
        ),
        (
            "references",
            "SELECT ?from ?to WHERE { ?s :references ?t . ?s :name ?from . ?t :name ?to }",
        ),
    ] {
        let answer = query_after_population(
            &client,
            &QueryParams {
                query: statement.to_owned(),
                max_results: 100,
                ..Default::default()
            },
        )
        .await?;
        assert!(
            named_pairs(&answer, "from", "to")?.contains(&("app_caller", "app_helper")),
            "{relation}: {answer}"
        );
    }
    client.cancel().await?;
    Ok(())
}

/// A failed engine keeps the captured call inventory and names the missing analysis (#760).
#[tokio::test]
async fn captured_calls_answer_an_inventory_when_the_engine_is_unavailable() -> TestResult {
    let mut source = String::from("def bound_leaf() -> int:\n    return 1\n");
    for index in 0..80 {
        source.push_str(
            &[
                "\ndef bound_caller_",
                &index.to_string(),
                "() -> int:\n    return bound_leaf()\n",
            ]
            .concat(),
        );
    }
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", &source)],
        Some("[languages.python.lsp]\ncommand = ['rift-test-missing-engine']\n".to_owned()),
    )
    .await?;
    let answer = query_after_population(
        &client,
        &QueryParams {
            query: "SELECT ?from ?to WHERE { ?s :calls ?t . ?s :name ?from . ?t :name ?to }"
                .to_owned(),
            max_results: 100,
            ..Default::default()
        },
    )
    .await?;
    let pairs = named_pairs(&answer, "from", "to")?;
    assert_eq!(pairs.len(), 80, "{answer}");
    assert!(
        pairs.contains(&("bound_caller_79", "bound_leaf")),
        "{answer}"
    );
    assert!(
        warning_codes(&answer).contains("engine_analysis_unavailable"),
        "{answer}"
    );
    let count = statement(&client, "SELECT (COUNT(?t) AS ?n) WHERE { ?s :calls ?t }").await?;
    assert_eq!(count["outcome"], "unavailable", "{count}");
    assert_eq!(count["reason"], "insufficient_coverage", "{count}");
    assert!(
        warning_codes(&count).contains("engine_analysis_unavailable"),
        "{count}"
    );
    client.cancel().await?;
    Ok(())
}

/// A file left out of the index makes the symbol inventory partial: the captured symbols
/// answer with the warning that names the file, and a question whose answer claims an
/// absence refuses with the same warning.
#[tokio::test]
async fn a_left_out_file_makes_the_symbol_inventory_partial_but_answered() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("pyproject.toml", METADATA),
            ("graph.py", SOURCE),
            ("blob.py", "held = 1\0\n"),
        ],
        None,
    )
    .await?;
    let inventory = statement(
        &client,
        "SELECT ?name WHERE { ?s a :Symbol; :name ?name } ORDER BY ?name",
    )
    .await?;
    assert_eq!(
        inventory["pagination"]["total"]
            .as_u64()
            .map(|total| total > 0),
        Some(true),
        "{inventory}"
    );
    assert!(
        warning_codes(&inventory).contains("source_unavailable"),
        "{inventory}"
    );
    for absent in [
        "SELECT ?s WHERE { ?s a :Symbol; :name 'held' }",
        "ASK { ?s a :Symbol; :name 'held' }",
        "SELECT (COUNT(?s) AS ?n) WHERE { ?s a :Symbol }",
    ] {
        let answer = statement(&client, absent).await?;
        assert_eq!(
            answer["reason"], "insufficient_coverage",
            "{absent}: {answer}"
        );
        assert!(
            warning_codes(&answer).contains("source_unavailable"),
            "{absent}: {answer}"
        );
    }
    let witness = statement(&client, "ASK { ?s a :Symbol; :name 'graph_leaf' }").await?;
    assert_eq!(
        witness["answer"],
        json!({"result":"boolean", "value":true}),
        "{witness}"
    );
    client.cancel().await?;
    Ok(())
}
