//! Exercises the public SPARQL route against authored bytes and a real embedded engine.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::global_api::{FixtureOptions, GlobalFixture};
use crate::workspace_client::{
    TestResult, populated_answer, query, query_after_population, served_workspace,
};
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
    Ok(statement_with_diagnostics(client, text).await?.0)
}

async fn statement_with_diagnostics(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    text: impl Into<String>,
) -> TestResult<(Value, Vec<Value>)> {
    populated_query(
        client,
        &QueryParams {
            include: rift_protocol::query::QueryInclude::default(),
            max_results: 2,
            query: text.into(),
            language: QueryLanguage::Sparql,
            ..Default::default()
        },
    )
    .await
}

async fn populated_query(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    params: &QueryParams,
) -> TestResult<(Value, Vec<Value>)> {
    populated_answer(client, "search", &serde_json::to_value(params)?).await
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
    let (answer, diagnostics) = statement_with_diagnostics(&client, query).await?;
    assert!(
        query::rows(&answer)?
            .iter()
            .any(|row| query::literal(row, "to").is_ok_and(|name| name == "leaf")),
        "{answer}"
    );
    assert_logged_warning(&answer, &diagnostics, "edge_coverage_missing");
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
    let (_directory, client, _server_task) = crate::workspace_client::served_prepared_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", source)],
        Some(CONFIGURATION.to_owned()),
        super::python_runtime_environment::prepare,
    )
    .await?;
    let _value = identity(&client, "value").await?;
    let types = statement(
        &client,
        "SELECT ?from ?to { ?s :hasType ?t . ?s :name ?from . ?t :name ?to FILTER(?from = 'value') }",
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
            assert_edge_provider(&client, "root", "lsp").await?;
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
        include: rift_protocol::query::QueryInclude::default(),
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
async fn anchored_call_usages_keep_exact_invocation_ranges() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", METADATA), ("graph.py", SOURCE)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let entry = identity(&client, "graph_root").await?;
    let leaf = identity(&client, "graph_leaf").await?;
    let orphan = identity(&client, "graph_orphan").await?;
    let usages = format!(
        "<{leaf}> :usages ?u . ?u :kind 'calls'; :of <{entry}>; :source ?source; \
         :range [ :unit ?unit; :start ?start; :end ?end ] ."
    );
    let select = |additional: &str| {
        format!("SELECT ?unit ?start ?end ?source {{ {usages} {additional} }} ORDER BY ?start")
    };
    let direct = statement(&client, select("")).await?;
    let joined = statement(&client, select(&format!("<{entry}> :calls <{leaf}> ."))).await?;
    let rows = query::rows(&direct)?;
    assert_eq!(rows.len(), 2, "{direct}");
    assert_eq!(rows, query::rows(&joined)?, "{joined}");
    for (row, (start, source)) in rows.iter().zip(SOURCE.match_indices("graph_leaf()")) {
        assert_eq!(query::literal(row, "start")?.parse::<usize>()?, start);
        assert_eq!(
            query::literal(row, "end")?.parse::<usize>()?,
            start + source.len(),
        );
        assert_eq!(query::literal(row, "source")?, source);
        assert_eq!(query::iri(row, "unit")?, "rift://source/project/graph.py");
    }
    let limited = statement(&client, format!("{} LIMIT 1", select(""))).await?;
    assert_eq!(query::rows(&limited)?, &rows[..1]);

    let empty = statement(
        &client,
        format!("ASK {{ <{orphan}> :usages ?u . ?u :kind 'calls' }}"),
    )
    .await?;
    let calls = statement(&client, format!("ASK {{ ?caller :calls <{orphan}> }}")).await?;
    assert_eq!(empty["answer"], calls["answer"], "{empty}");
    assert_eq!(empty["reason"], calls["reason"], "{empty}");
    if empty["answer"].is_null() {
        assert_eq!(empty["reason"], "insufficient_coverage", "{empty}");
    } else {
        assert_eq!(empty["answer"], json!({"result":"boolean", "value": false}));
    }

    let uncovered = statement(
        &client,
        "ASK { <rift://symbol/pypi/pypi.org/otherpkg/python/otherpkg/entry> \
         :usages ?u . ?u :kind 'calls' }",
    )
    .await?;
    assert_eq!(uncovered["reason"], "insufficient_coverage", "{uncovered}");
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

#[tokio::test]
async fn static_edge_provider_filters_captured_calls() -> TestResult {
    let files = [(
        "entry.mjs",
        "export function perform() {}\nexport function root() { perform(); }\n",
    )];
    let (_directory, client, task) = served_workspace(&files, None).await?;
    let seed = identity(&client, "root").await?;
    let raw = crate::workspace_client::call_retrying_acceptance(
        &client,
        crate::workspace_client::tool_request(
            "search_retrieval",
            &json!({"traversal": {"seed": seed, "direction": "outgoing", "kinds": ["calls"]}}),
        ),
    )
    .await?;
    assert_eq!(
        raw["results"][0]["traversal_path"][0]["edge"].get("providers"),
        None,
        "{raw}"
    );
    let answer = statement(&client, format!("ASK {{ <{seed}> :calls ?target . ?target :name 'perform' . ?edge a :Edge; :from <{seed}>; :to ?target; :kinds :calls; :provider 'static' }}")).await?;
    assert_eq!(
        answer["answer"],
        json!({"result":"boolean", "value":true}),
        "{answer}"
    );
    let text = format!(
        "SELECT ?edge {{ <{seed}> :calls ?target . ?edge a :Edge; :from <{seed}>; :to ?target; :kinds :calls }}"
    );
    let mut revision = None;
    for included in [false, true, false] {
        let request = rift_protocol::query::QueryParams {
            query: text.clone(),
            include: rift_protocol::query::QueryInclude {
                providers: included,
            },
            revision: revision.clone(),
            ..Default::default()
        };
        let answer = crate::workspace_client::query_after_population(&client, &request).await?;
        let rows = query::rows(&answer)?;
        assert_eq!(rows.len(), 1, "{answer}");
        assert_eq!(
            rows[0]["edge"].get("providers"),
            included.then(|| json!(["static"])).as_ref()
        );
        assert_eq!(answer["pagination"]["total"], 1);
        revision = Some(serde_json::from_value(answer["revision"].clone())?);
    }
    client.cancel().await?;
    task.await?;
    Ok(())
}

#[tokio::test]
async fn embedded_edge_provider_filters_inferred_python_calls() -> TestResult {
    let files = [
        ("pyproject.toml", METADATA),
        (
            "graph.py",
            "class Worker:\n    def perform(self) -> int:\n        return 1\n\ndef direct() -> int:\n    return 0\n\ndef root(workers: list[Worker]) -> int:\n    direct()\n    return workers[0].perform()\n",
        ),
    ];
    let (directory, client, task) =
        served_workspace(&files, Some(CONFIGURATION.to_owned())).await?;
    let captured = rift_server::ReadService::build(
        directory.path(),
        rift_index::WorkspaceIndexLimits::default(),
        &rift_core::SourceVisibility::default(),
        &rift_core::TextFileInclusion::default(),
        rift_protocol::configuration::HistoryConfiguration::default(),
    )?;
    let root = rift_core::SymbolId::new(identity(&client, "root").await?)?;
    let outgoing = captured.edges().outgoing(&root);
    assert!(
        outgoing
            .iter()
            .any(|edge| edge.kind() == rift_protocol::read::EdgeKind::Calls
                && edge.to().as_str().ends_with("/direct")),
        "the static fixture captures the direct call: {outgoing:?}"
    );
    assert!(
        !outgoing
            .iter()
            .any(|edge| edge.kind() == rift_protocol::read::EdgeKind::Calls
                && edge.to().as_str().ends_with("/perform")),
        "the method receiver requires subscript inference: {outgoing:?}"
    );
    assert_edge_provider(&client, "root", "engine").await?;
    client.cancel().await?;
    task.await?;
    Ok(())
}

async fn assert_edge_provider(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    provider: &str,
) -> TestResult {
    let seed = identity(client, name).await?;
    for selected in [provider, "absent"] {
        let answer = statement(client, format!("SELECT DISTINCT ?provider {{ <{seed}> :calls ?target . ?target :name 'perform' . ?edge a :Edge; :from <{seed}>; :to ?target; :kinds :calls; :provider ?provider . FILTER(?provider = '{selected}') }}")).await?;
        let rows = query::rows(&answer)?;
        if selected == provider {
            assert_eq!(rows.len(), 1, "{answer}");
            assert_eq!(rows[0]["provider"]["value"], provider, "{answer}");
        } else {
            assert!(rows.is_empty(), "{answer}");
        }
    }
    Ok(())
}

fn assert_logged_warning(answer: &Value, diagnostics: &[Value], code: &str) {
    assert!(answer.get("warnings").is_none(), "{answer}");
    assert!(
        diagnostics.iter().any(|warning| warning["code"] == code),
        "expected {code} in {diagnostics:?}"
    );
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
    let (inventory, diagnostics) = populated_query(
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
    assert_logged_warning(&inventory, &diagnostics, "framework_context_unresolved");
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
    let (answer, diagnostics) = populated_query(
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
    assert_logged_warning(&answer, &diagnostics, "engine_analysis_unavailable");
    let (count, diagnostics) =
        statement_with_diagnostics(&client, "SELECT (COUNT(?t) AS ?n) WHERE { ?s :calls ?t }")
            .await?;
    assert_eq!(count["outcome"], "unavailable", "{count}");
    assert_eq!(count["reason"], "insufficient_coverage", "{count}");
    assert_logged_warning(&count, &diagnostics, "engine_analysis_unavailable");
    client.cancel().await?;
    Ok(())
}

const SYMBOL_COUNT: &str = "SELECT (COUNT(*) AS ?n) { ?s a :Symbol }";
const SYMBOL_PAGE: &str = "SELECT ?s ?name { ?s a :Symbol; :name ?name } LIMIT 5";
const SYMBOL_EXISTS: &str = "ASK { ?s a :Symbol }";

#[tokio::test]
async fn disabled_local_symbol_inventory_refuses_absence_and_keeps_package_reads() -> TestResult {
    let global = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = '{}'\nattempts = 1\n\n\
         [search]\ndefault_scope = 'global'\n\n\
         [search.query]\nlocal_symbol_inventory = false\n",
        global.endpoint
    );
    let (_directory, client, _server_task) = served_workspace(&[], Some(configuration)).await?;
    let requests_before = global.requests().await.len();
    for text in [
        SYMBOL_COUNT,
        SYMBOL_PAGE,
        SYMBOL_EXISTS,
        "SELECT ?s { ?s a :Symbol; :name 'beacon' }",
        "SELECT ?name { ?s a :Symbol; :name ?name; :contains ?child }",
        "SELECT ?element { ?element a :SyntaxElement; :of ?s . ?s :name 'beacon' }",
        "SELECT ?revision { ?s :introducedIn ?revision . ?revision a :Commit }",
    ] {
        let (answer, diagnostics) = statement_with_diagnostics(&client, text).await?;
        assert_eq!(answer["outcome"], "unavailable", "{text}: {answer}");
        assert_eq!(
            answer["reason"], "insufficient_coverage",
            "{text}: {answer}"
        );
        assert_logged_warning(&answer, &diagnostics, "local_symbol_inventory_unavailable");
        assert!(answer.get("answer").is_none(), "{text}: {answer}");
        assert!(answer.get("pagination").is_none(), "{text}: {answer}");
        assert!(answer.get("view").is_none(), "{text}: {answer}");
    }
    assert_eq!(global.requests().await.len(), requests_before);
    let package = statement(
        &client,
        "SELECT ?name { SERVICE :search { ?hit :scope 'global'; :query 'beacon'; :target 'symbol'; \
         :packages '[{\"manager\":\"cargo\",\"registry\":\"crates.io\",\"name\":\"demo\",\"version\":\"1.0.0\"}]'^^rdf:JSON; \
         :symbol ?symbol . ?symbol :name ?name } }",
    )
    .await?;
    assert!(
        query::rows(&package)?
            .iter()
            .any(|row| query::literal(row, "name").is_ok_and(|name| name == "beacon")),
        "{package}"
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn disabled_local_symbol_inventory_keeps_an_exact_local_symbol() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("pyproject.toml", METADATA),
            ("graph.py", "def beacon() -> int:\n    return 1\n"),
        ],
        Some("[search.query]\nlocal_symbol_inventory = false\n".to_owned()),
    )
    .await?;
    let answer = statement(
        &client,
        "SELECT ?name { <rift://symbol/local/python/graph/beacon> :name ?name }",
    )
    .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("expected the identified local symbol: {answer}").into());
    };
    assert_eq!(query::literal(row, "name")?, "beacon");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn default_local_symbol_inventory_answers_complete_empty_and_populated_captures() -> TestResult
{
    let configuration = "[source]\ninclude = ['**/*.rs']\n\n[search]\ndefault_scope = 'global'\n";
    let cases: &[(&[(&str, &str)], u64)] = &[
        (&[], 0),
        (&[("lib.rs", "// This file declares no symbols.\n")], 0),
        (&[("lib.rs", "pub fn beacon() {}\n")], 1),
    ];
    for &(files, expected) in cases {
        let (_directory, client, _server_task) =
            served_workspace(files, Some(configuration.to_owned())).await?;
        let count = statement(&client, SYMBOL_COUNT).await?;
        let [row] = query::rows(&count)? else {
            return Err(format!("expected a count: {count}").into());
        };
        assert_eq!(query::literal(row, "n")?.parse::<u64>()?, expected);
        let page = statement(&client, SYMBOL_PAGE).await?;
        assert_eq!(page["pagination"]["total"], expected, "{page}");
        let answer = statement(&client, SYMBOL_EXISTS).await?;
        assert_eq!(answer["answer"]["value"], expected > 0, "{answer}");
        client.cancel().await?;
    }
    Ok(())
}

/// A file left out of the index makes the symbol inventory partial: the captured symbols
/// answer and the server logs the missing file. A query that claims absence is unavailable.
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
    let (inventory, diagnostics) = statement_with_diagnostics(
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
    assert_logged_warning(&inventory, &diagnostics, "source_unavailable");
    for absent in [
        "SELECT ?s WHERE { ?s a :Symbol; :name 'held' }",
        "ASK { ?s a :Symbol; :name 'held' }",
        "SELECT (COUNT(?s) AS ?n) WHERE { ?s a :Symbol }",
    ] {
        let (answer, diagnostics) = statement_with_diagnostics(&client, absent).await?;
        assert_eq!(
            answer["reason"], "insufficient_coverage",
            "{absent}: {answer}"
        );
        assert_logged_warning(&answer, &diagnostics, "source_unavailable");
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

/// `def <name>() -> int:` padded with a comment to exactly `bytes` bytes.
fn python_source_of(name: &str, bytes: usize) -> String {
    let mut source = format!("def {name}() -> int:\n    return 1\n#");
    assert!(source.len() < bytes, "fixture too small: {bytes}");
    source.push_str(&"x".repeat(bytes - source.len() - 1));
    source.push('\n');
    source
}

/// One page of `max_results` rows of `text`.
async fn statement_page(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    text: &str,
    max_results: u64,
) -> TestResult<Value> {
    query_after_population(
        client,
        &QueryParams {
            max_results,
            query: text.to_owned(),
            language: QueryLanguage::Sparql,
            ..Default::default()
        },
    )
    .await
}

/// D37 (#717): a file past `[providers.syntax] max_file` is left out of the indexed set the
/// way a file the source policy excludes is. Plain symbol enumeration, `LIMIT`, `COUNT`,
/// `GROUP BY` and an absence all answer over the symbols the index holds, and no answer
/// carries a warning for the left-out file. Sources below and at the bound answer.
#[tokio::test]
async fn an_oversized_file_leaves_symbol_enumeration_counts_and_groups_answering() -> TestResult {
    let bound = SOURCE.len() + 64;
    let configuration = format!("[providers.syntax]\nmax_file = \"{bound}b\"\n");
    let at = python_source_of("at_bound", bound);
    let above = python_source_of("above_bound", bound + 1);
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("pyproject.toml", METADATA),
            ("graph.py", SOURCE),
            ("at.py", &at),
            ("above.py", &above),
        ],
        Some(configuration),
    )
    .await?;
    let enumeration = statement_page(
        &client,
        "SELECT ?name WHERE { ?s a :Symbol; :name ?name } ORDER BY ?name",
        100,
    )
    .await?;
    let names = query::rows(&enumeration)?
        .iter()
        .map(|row| query::literal(row, "name"))
        .collect::<TestResult<BTreeSet<&str>>>()?;
    assert!(
        names.contains("at_bound") && names.contains("graph_leaf"),
        "{enumeration}"
    );
    assert!(!names.contains("above_bound"), "{enumeration}");
    let total = enumeration["pagination"]["total"]
        .as_u64()
        .ok_or("pagination total")?;
    assert_eq!(total, u64::try_from(names.len())?, "{enumeration}");
    let mut answers = vec![enumeration.clone()];

    let limited = statement_page(
        &client,
        "SELECT ?s ?name WHERE { ?s a :Symbol; :name ?name } LIMIT 3",
        10,
    )
    .await?;
    assert_eq!(query::rows(&limited)?.len(), 3, "{limited}");
    answers.push(limited);

    let count = statement_page(
        &client,
        "SELECT (COUNT(*) AS ?n) WHERE { ?s a :Symbol }",
        10,
    )
    .await?;
    let [row] = query::rows(&count)? else {
        return Err(format!("expected one count: {count}").into());
    };
    assert_eq!(query::literal(row, "n")?, total.to_string(), "{count}");
    answers.push(count);

    let grouped = statement_page(
        &client,
        "SELECT ?kind (COUNT(*) AS ?n) WHERE { ?s a :Symbol; :kind ?kind } GROUP BY ?kind",
        100,
    )
    .await?;
    let grouped_total = query::rows(&grouped)?
        .iter()
        .map(|row| query::literal(row, "n").map(|value| value.parse::<u64>().unwrap_or(0)))
        .sum::<TestResult<u64>>()?;
    assert_eq!(grouped_total, total, "{grouped}");
    answers.push(grouped);

    let absent = statement_page(&client, "ASK { ?s a :Symbol; :name 'above_bound' }", 10).await?;
    assert_eq!(
        absent["answer"],
        json!({"result":"boolean", "value":false}),
        "{absent}"
    );
    answers.push(absent);

    for answer in &answers {
        assert_ne!(answer["outcome"], "unavailable", "{answer}");
        assert!(answer.get("warnings").is_none(), "{answer}");
    }
    client.cancel().await?;
    Ok(())
}

/// `functions` Python functions, each calling the next: `chain_0` calls `chain_1`, and the
/// last returns a constant.
fn call_chain(functions: usize) -> TestResult<String> {
    let mut source = String::new();
    for index in 0..functions {
        if index + 1 < functions {
            let next = index + 1;
            write!(
                source,
                "def chain_{index}() -> int:\n    return chain_{next}()\n\n"
            )?;
        } else {
            write!(source, "def chain_{index}() -> int:\n    return 1\n")?;
        }
    }
    Ok(source)
}

/// D37 (#715): a transitive walk past the graph acquisition request budget answers the
/// callees it acquired and pages them instead of refusing. One edge read per function on
/// the chain: a chain the budget covers answers whole, and one past it answers the
/// acquired prefix, `LIMIT` and `COUNT` included, with no warning for the budget.
#[tokio::test]
async fn a_walk_past_the_request_budget_answers_the_acquired_part() -> TestResult {
    const REQUESTS_MAX: usize = 64;
    for (functions, acquired) in [
        (REQUESTS_MAX, REQUESTS_MAX - 1),
        (REQUESTS_MAX + 1, REQUESTS_MAX),
        (REQUESTS_MAX + 16, REQUESTS_MAX),
    ] {
        let source = call_chain(functions)?;
        let (_directory, client, _server_task) = served_workspace(
            &[("pyproject.toml", METADATA), ("graph.py", &source)],
            Some(CONFIGURATION.to_owned()),
        )
        .await?;
        let entry = identity(&client, "chain_0").await?;
        let walk = statement_page(
            &client,
            &format!("SELECT DISTINCT ?t {{ <{entry}> :calls+ ?t }}"),
            10,
        )
        .await?;
        assert_ne!(
            walk["outcome"], "unavailable",
            "functions={functions}: {walk}"
        );
        assert_eq!(
            walk["pagination"]["total"],
            json!(acquired),
            "functions={functions}: {walk}"
        );
        assert_eq!(walk["pagination"]["pages"], json!(acquired.div_ceil(10)));
        assert!(walk.get("warnings").is_none(), "{walk}");
        let limited = statement_page(
            &client,
            &format!("SELECT DISTINCT ?t {{ <{entry}> :calls+ ?t }} LIMIT 5"),
            10,
        )
        .await?;
        assert_eq!(query::rows(&limited)?.len(), 5, "{limited}");
        let count = statement_page(
            &client,
            &format!("SELECT (COUNT(DISTINCT ?t) AS ?n) {{ <{entry}> :calls+ ?t }}"),
            10,
        )
        .await?;
        let [row] = query::rows(&count)? else {
            return Err(format!("expected one count: {count}").into());
        };
        assert_eq!(query::literal(row, "n")?, acquired.to_string(), "{count}");
        client.cancel().await?;
    }
    Ok(())
}
