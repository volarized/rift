//! Public SPARQL relationship queries through a live MCP client.
//!
//! Covers incoming and outgoing paths, repeated and recursive physical usages,
//! retrieval joins, required-fact refusals, retained dependency source reads, and
//! changed files delivered to the configured engines.
//!
//! Most fixtures select the embedded `ty` engine over a Python workspace. Scripted
//! `sh` engines cover readiness, cancellation and invalid source positions; live
//! rust-analyzer and TypeScript suites use `RIFT_ENGINE_LIVE`.

#[cfg(unix)]
mod fake_engine;

#[cfg(unix)]
#[path = "announced_work/mod.rs"]
mod announced_work;
#[allow(
    dead_code,
    reason = "shared fixture global API exposes helpers this suite does not use"
)]
mod global_api;
mod hermetic_search;
#[path = "../../rift-lsp/tests/live_engine_gate.rs"]
mod live_engine_gate;
#[path = "python_runtime/environment.rs"]
mod python_runtime_environment;
use workspace_client::query;
#[cfg(unix)]
#[path = "../../rift-lsp/tests/typescript_install.rs"]
mod typescript_install;
// `served_relative_workspace` and its `relative_spelling` helper are part of
// `workspace_client`'s shared surface; this binary serves its root the plain way.
#[allow(dead_code)]
mod workspace_client;

use rift_protocol::error::{ErrorCode, RetryDirective};
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, captured_seed, failed_call, served_workspace,
    tool_request,
};

/// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`:
///
/// ```text
/// root --calls--> branch_a --calls--> leaf
/// root --calls--> branch_b --calls--> leaf
/// ```
const GRAPH_METADATA: &str = "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['graph']\n";
const OUTGOING_METADATA: &str = "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['graph','extra']\n";
const RUST_LIBRARY: &str =
    "[package]\nname='walk_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n";

const CALL_GRAPH_FILES: &[(&str, &str)] = &[
    ("pyproject.toml", GRAPH_METADATA),
    (
        "graph.py",
        "def leaf() -> int:\n    return 1\n\n\
     def branch_a() -> int:\n    return leaf()\n\n\
     def branch_b() -> int:\n    return leaf()\n\n\
     def root() -> int:\n    return branch_a() + branch_b()\n",
    ),
];

/// The engine that resolves this workspace's references, embedded in the binary.
const ENGINE: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

/// A workspace with no configured engine, so no lane resolves a reference in it.
const ENGINELESS_FILES: &[(&str, &str)] = &[
    ("Cargo.toml", RUST_LIBRARY),
    ("lib.rs", "pub fn beacon() {}\n"),
];

const PHYSICAL_CALLS_QUERY: &str = "SELECT ?symbol ?name ?unit ?start ?end ?derivation { \
    ?caller a :Symbol; :name 'root'; :calls ?symbol . \
    ?symbol :name ?name; :declarations ?declaration . \
    ?declaration :unit ?unit; :range ?range . ?range :start ?start; :end ?end . \
    ?relationship a :Relationship; :from ?caller; :to ?symbol; :facets :calls; :derivation ?derivation \
    } ORDER BY ?name";

#[tokio::test]
async fn physical_calls_keep_the_admitted_runtime_source() -> TestResult {
    let configuration =
        format!("[source]\nexclude = [\"rift.toml\", \"pyproject.toml\"]\n\n{ENGINE}");
    let (directory, client, _server_task) = workspace_client::served_prepared_workspace(
        &[
            ("pyproject.toml", GRAPH_METADATA),
            ("graph.py", "def root():\n    return len([])\n"),
        ],
        Some(configuration),
        python_runtime_environment::prepare,
    )
    .await?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &json!({
        "query":"SELECT ?caller ?symbol ?name ?unit { ?caller a :Symbol; :name 'root'; :calls ?symbol . ?symbol :name ?name; :declarations ?declaration . ?declaration :unit ?unit }"
    }))).await?;
    let rows = query::rows(&answer)?;
    assert_eq!(rows.len(), 1, "{answer}");
    assert_eq!(rows[0]["caller"]["type"], "blank_node");
    assert_eq!(query::literal(&rows[0], "name")?, "len");
    let unit = query::iri(&rows[0], "unit")?;
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(unit)?;
    assert!(matches!(
        owner,
        Some(rift_protocol::identity::SymbolOwner::Runtime { .. })
    ));
    let params = json!({"unit":unit,"view":answer["view"]["id"]});
    let original = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(params.clone())?)?)?,
        ),
    )
    .await?;
    let [original_source] = query::rows(&original)? else {
        return Err(format!("one original runtime source: {original}").into());
    };
    assert_eq!(query::literal(original_source, "outcome")?, "found");
    assert_eq!(
        query::literal(original_source, "source_view")?,
        answer["view"]["id"].as_str().ok_or("runtime source view")?
    );
    assert_eq!(
        query::literal(original_source, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("runtime source expiry")?
    );
    assert!(query::literal(original_source, "text")?.contains("def len("));
    std::fs::write(
        directory.path().join("graph.py"),
        "# changed after capture\n",
    )?;
    let held = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(params.clone())?)?)?,
        ),
    )
    .await?;
    assert_eq!(query::rows(&held)?, query::rows(&original)?);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_query_refuses_unserved_records_in_the_complete_domain() -> TestResult {
    let source = "def leaf():\n    return 1\n\ndef root():\n    return leaf()\n";
    let (_directory, client, _server_task) =
        served_workspace(&[("graph.py", source)], Some(ENGINE.to_owned())).await?;
    let selected = call_retrying_acceptance(&client, tool_request("search", &json!({
        "query":"SELECT ?unit { ?symbol a :Symbol; :name 'dependencies'; :declarations ?declaration . ?declaration :unit ?unit }"
    }))).await?;
    let rows = query::rows(&selected)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        query::iri(&rows[0], "unit")?,
        "rift://source/project/rift.toml"
    );
    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &json!({"query":PHYSICAL_CALLS_QUERY}),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::TemporarilyUnavailable);
    assert!(
        failure
            .message
            .contains("no captured declaration name position"),
        "{failure:?}"
    );
    client.cancel().await?;
    Ok(())
}

/// A Python file without namespace metadata keeps physical declaration bindings through
/// actual engine calls. Held source and result identity survive a later file edit.
#[tokio::test]
async fn search_query_local_physical_calls_keep_held_source() -> TestResult {
    let source = "def leaf():\n    return 1\n\ndef root():\n    return leaf()\n";
    let (directory, client, _server_task) = served_workspace(
        &[("graph.py", source)],
        Some(format!("[source]\nexclude=['rift.toml']\n{ENGINE}")),
    )
    .await?;
    let statement = PHYSICAL_CALLS_QUERY;
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let rows = query::rows(&answer)?;
    assert_eq!(rows.len(), 1, "one original physical callee: {answer}");
    let row = &rows[0];
    assert_eq!(query::literal(row, "name")?, "leaf");
    let symbol: rift_protocol::query::RdfTerm = serde_json::from_value(row["symbol"].clone())?;
    assert!(matches!(
        symbol,
        rift_protocol::query::RdfTerm::BlankNode { .. }
    ));
    assert_eq!(
        query::iri(row, "derivation")?,
        format!("{}resolution", rift_search::sparql::VOCABULARY)
    );
    let unit = query::iri(row, "unit")?;
    assert_eq!(unit, "rift://source/project/graph.py");
    let start = query::literal(row, "start")?.parse::<usize>()?;
    let end = query::literal(row, "end")?.parse::<usize>()?;
    let expected = source.get(start..end).ok_or("original declaration range")?;
    assert!(expected.starts_with("def leaf()"));
    let view = answer["view"]["id"].as_str().ok_or("held physical query")?;
    std::fs::write(
        directory.path().join("graph.py"),
        "def changed():\n    return 2\n",
    )?;
    let held = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":unit,"range":{"start":start,"end":end},"view":view}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&held)? else {
        return Err(format!("one original source result: {held}").into());
    };
    assert_eq!(query::literal(source_row, "text")?, expected);
    let replay = format!(
        "SELECT ?symbol ?name ?unit ?start ?end ?derivation {{ SERVICE <{}{view}> {{}} }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
    );
    let retained =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":replay}))).await?;
    assert_eq!(retained["answer"], answer["answer"]);
    assert_eq!(retained["view"]["expires_at"], answer["view"]["expires_at"]);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_neighbors_depth_one_reaches_the_direct_callers() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let parameters = serde_json::to_value(query::path(&seed, "^:references")?)?;
    let request = tool_request("search", &parameters);
    let structured = call_retrying_acceptance(&client, request).await?;
    assert_eq!(projected_names(&structured)?, ["branch_a", "branch_b"]);
    let parameters = serde_json::to_value(query::usages(&seed)?)?;
    let request = tool_request("search", &parameters);
    let usages = call_retrying_acceptance(&client, request).await?;
    assert_eq!(query::rows(&usages)?.len(), 2);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_impact_incoming_depth_two_reaches_every_caller() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let parameters = serde_json::to_value(query::path(
        &seed,
        "(^:references|^:references/^:references)",
    )?)?;
    let request = tool_request("search", &parameters);
    let structured = call_retrying_acceptance(&client, request).await?;
    assert_eq!(
        projected_names(&structured)?,
        ["branch_a", "branch_b", "root"]
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_to_keeps_only_the_path_query_target() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let target = captured_seed(&client, "root", "graph.py").await?;
    let statement = format!(
        "SELECT ?symbol ?name {{ <{seed}> ^:references/^:references ?symbol . ?symbol :name ?name . FILTER(?symbol = <{target}>) }} ORDER BY ?name"
    );
    let request = tool_request("search", &json!({"query":statement}));
    let structured = call_retrying_acceptance(&client, request).await?;
    assert_eq!(projected_names(&structured)?, ["root", "root"]);
    for row in query::rows(&structured)? {
        assert_eq!(query::iri(row, "symbol")?, target);
    }
    client.cancel().await?;
    Ok(())
}

fn projected_names(structured: &Value) -> TestResult<Vec<String>> {
    query::rows(structured)?
        .iter()
        .map(|row| query::literal(row, "name").map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn search_query_counts_and_pages_more_than_ten_thousand_occurrences() -> TestResult {
    let source = format!("def leaf(): pass\n{}", "leaf()\n".repeat(10_001));
    let (directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", GRAPH_METADATA), ("graph.py", &source)],
        Some(ENGINE.to_owned()),
    )
    .await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let parameters = serde_json::to_value(query::usages(&seed)?)?;
    let started = std::time::Instant::now();
    eprintln!("10001 occurrences: query started");
    let first = call_retrying_acceptance(&client, tool_request("search", &parameters)).await?;
    eprintln!("10001 occurrences: query {:?}", started.elapsed());
    assert_eq!(query::rows(&first)?.len(), 1_000);
    assert_eq!(first["pagination"]["total_pages"], 11);
    let view = first["view"]["id"]
        .as_str()
        .ok_or("retained occurrence execution")?;
    let service = format!("{}{view}", rift_search::sparql::RESULT_SERVICE_PREFIX);
    std::fs::write(
        directory.path().join("graph.py"),
        "# changed after capture\n",
    )?;
    let count = format!("SELECT (COUNT(*) AS ?count) {{ SERVICE <{service}> {{}} }}");
    let started = std::time::Instant::now();
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":count}))).await?;
    assert_eq!(query::literal(&query::rows(&answer)?[0], "count")?, "10001");
    eprintln!("10001 occurrences: count {:?}", started.elapsed());
    let mut occurrences = std::collections::BTreeSet::new();
    let mut offsets = Vec::new();
    let started = std::time::Instant::now();
    let mut answer = first;
    for page in 0..11 {
        assert_eq!(answer["pagination"]["page_index"], page);
        for row in query::rows(&answer)? {
            assert_eq!(row["usage"]["type"], "blank_node");
            assert_eq!(query::iri(row, "unit")?, "rift://source/project/graph.py");
            assert!(
                occurrences.insert(
                    row["usage"]["value"]
                        .as_str()
                        .ok_or("retained occurrence identity")?
                        .to_owned()
                )
            );
            let start: usize = query::literal(row, "start")?.parse()?;
            let end: usize = query::literal(row, "end")?.parse()?;
            assert_eq!(&source[start..end], "leaf");
            offsets.push(start);
        }
        let next = answer["pagination"]["next_query"].as_str();
        if page == 10 {
            assert!(next.is_none(), "{answer}");
        } else {
            let statement = next.ok_or("retained occurrence continuation")?.to_owned();
            let page_started = std::time::Instant::now();
            answer = call_retrying_acceptance(
                &client,
                tool_request("search", &json!({"query":statement})),
            )
            .await?;
            eprintln!(
                "10001 occurrences: continuation {} {:?}",
                page + 1,
                page_started.elapsed()
            );
        }
    }
    eprintln!(
        "10001 occurrences: all continuations {:?}",
        started.elapsed()
    );
    assert_eq!(occurrences.len(), 10_001);
    let expected = source
        .match_indices("leaf")
        .skip(1)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    assert_eq!(offsets, expected);
    // A separate SPARQL query evaluates ordering and slicing over the full result.
    let statement = format!(
        "SELECT ?usage ?unit ?start ?end {{ SERVICE <{service}> {{}} }} ORDER BY ?start LIMIT 2 OFFSET 9000"
    );
    let started = std::time::Instant::now();
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let selected = query::rows(&answer)?
        .iter()
        .map(|row| {
            let start: usize = query::literal(row, "start")?.parse()?;
            Ok(start)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(selected, expected[9000..9002]);
    eprintln!(
        "10001 occurrences: ordered offset control {:?}",
        started.elapsed()
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_query_preserves_recursive_and_file_level_occurrences() -> TestResult {
    let source = "def recur() -> int:\n    return recur() + recur()\n\nrecur()\n";
    let (_directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", GRAPH_METADATA), ("graph.py", source)],
        Some(ENGINE.to_owned()),
    )
    .await?;
    let seed = captured_seed(&client, "recur", "graph.py").await?;
    let parameters = serde_json::to_value(query::usages(&seed)?)?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &parameters)).await?;
    let rows = query::rows(&answer)?;
    assert_eq!(rows.len(), 3, "{answer}");
    let mut actual = Vec::new();
    for row in rows {
        assert_eq!(query::iri(row, "unit")?, "rift://source/project/graph.py");
        let start: usize = query::literal(row, "start")?.parse()?;
        let end: usize = query::literal(row, "end")?.parse()?;
        assert_eq!(&source[start..end], "recur");
        actual.push(start);
    }
    let expected = source
        .match_indices("recur")
        .skip(1)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    let parameters = serde_json::to_value(query::path(&seed, "^:references")?)?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &parameters)).await?;
    assert_eq!(projected_names(&answer)?, ["recur"]);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_query_inverse_calls_preserves_recursive_and_module_call_sites() -> TestResult {
    let source = "def recur():\r\n    return recur() + recur()\r\n\r\ndef caller():\r\n    '😀'; recur(); recur()\r\n\r\nrecur()\r\n";
    let (directory, client, _server_task) = served_workspace(
        &[("pyproject.toml", GRAPH_METADATA), ("graph.py", source)],
        Some(ENGINE.to_owned()),
    )
    .await?;
    let seed = captured_seed(&client, "recur", "graph.py").await?;
    let answer = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::path(&seed, "^:calls")?)?,
        ),
    )
    .await?;
    assert_eq!(projected_names(&answer)?, ["caller", "recur"]);
    let statement = format!(
        "SELECT DISTINCT ?usage ?unit ?start ?end {{ <{seed}> ^:calls ?caller . <{seed}> :usages ?usage . ?usage :facets :calls; :unit ?unit; :start ?start; :end ?end }} ORDER BY ?start"
    );
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let starts = query::rows(&answer)?
        .iter()
        .map(|row| {
            assert_eq!(query::iri(row, "unit")?, "rift://source/project/graph.py");
            let start: usize = query::literal(row, "start")?.parse()?;
            let end: usize = query::literal(row, "end")?.parse()?;
            assert_eq!(&source[start..end], "recur");
            Ok(start)
        })
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(
        starts,
        source
            .match_indices("recur")
            .skip(1)
            .map(|(start, _)| start)
            .collect::<Vec<_>>()
    );
    let view = answer["view"]["id"]
        .as_str()
        .ok_or("retained incoming calls")?;
    std::fs::write(
        directory.path().join("graph.py"),
        "# changed after capture\n",
    )?;
    let held = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":"rift://source/project/graph.py","view":view}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&held)? else {
        return Err(format!("one original source result: {held}").into());
    };
    assert_eq!(query::literal(source_row, "text")?, source);
    client.cancel().await?;
    Ok(())
}

/// Retrieval results join graph facts through their canonical symbol identities.
#[tokio::test]
async fn search_query_and_traversal_merge_carry_both_matched_by_entries_and_the_walked_path()
-> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let statement = format!(
        "SELECT DISTINCT ?symbol ?name {{ SERVICE :search {{ ?hit :query 'branch_a'; :target 'symbol'; :symbol ?symbol }} <{seed}> ^:references ?symbol . ?symbol :name ?name FILTER(?name = 'branch_a') }} ORDER BY ?name"
    );
    let request = serde_json::from_value(json!({"query":statement}))?;
    let answer = workspace_client::search_after_population(&client, &request).await?;
    assert_eq!(projected_names(&answer)?, ["branch_a"]);
    client.cancel().await?;
    Ok(())
}

/// Every requested relationship must have complete provider coverage before evaluation.
#[tokio::test]
async fn search_traversal_over_an_unproduced_facet_serves_the_coverage_warning() -> TestResult {
    let configuration =
        format!("[source]\nexclude = [\"rift.toml\", \"pyproject.toml\"]\n\n{ENGINE}");
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(configuration)).await?;
    let request = json!({"query":
        "SELECT ?symbol ?name { ?seed a :Symbol; :name 'leaf'; :path 'graph.py' . \
         ?seed (^:implements|^:references) ?symbol . ?symbol :name ?name } ORDER BY ?name"
    });
    required_facts_unavailable(&client, &request, ErrorCode::CapabilityUnavailable).await?;
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_over_only_unproduced_facets_refuses() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let request = serde_json::to_value(query::path(&seed, "^:implements")?)?;
    required_facts_unavailable(&client, &request, ErrorCode::CapabilityUnavailable).await?;
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_over_a_produced_facet_serves_no_coverage_warning() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "leaf", "graph.py").await?;
    let request = serde_json::to_value(query::path(&seed, "^:references")?)?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(projected_names(&answer)?, ["branch_a", "branch_b"]);
    assert!(answer["warnings"].is_null(), "{answer}");
    client.cancel().await?;
    Ok(())
}

/// Removed selectors fail request validation; graph bindings are expressed in SPARQL.
#[tokio::test]
async fn search_traversal_without_a_seed_refuses() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;
    assert_eq!(
        refusal_code(
            &client,
            &json!({"query":"ASK {}", "traversal":{"direction":"incoming"}})
        )
        .await?,
        ErrorCode::InvalidRequest
    );
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_without_an_engine_refuses_capability_unavailable() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;
    let seed = captured_seed(&client, "beacon", "lib.rs").await?;
    let request = serde_json::to_value(query::path(&seed, "^:references")?)?;
    required_facts_unavailable(&client, &request, ErrorCode::CapabilityUnavailable).await?;
    client.cancel().await?;
    Ok(())
}

/// An unavailable change input prevents evaluation of a graph join over that input.
#[tokio::test]
async fn search_traversal_beside_a_change_refuses_capability_unavailable() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;
    let seed = captured_seed(&client, "beacon", "lib.rs").await?;
    let statement = format!(
        "SELECT ?symbol {{ SERVICE :search {{ ?hit :change '{{\"base\":\"baseline\"}}'^^rdf:JSON; :symbol ?symbol }} <{seed}> :calls ?symbol }}"
    );
    assert_eq!(
        refusal_code(&client, &json!({"query":statement})).await?,
        ErrorCode::CapabilityUnavailable
    );
    client.cancel().await?;
    Ok(())
}

/// A `sh` engine that answers `initialize`, announces work it never ends, and
/// answers every later request with no location, so the session reads analyzing on
/// every attempt. Each start appends one line to the file its first argument names.
#[cfg(unix)]
const ANALYZING_ENGINE: &str = r#"echo start >> "$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// An incoming walk whose engine still reads analyzing when its wait is
/// spent answers with `engine_analysis_unavailable` and keeps the session, as an outgoing
/// walk does: every later walk meets the same engine process instead of starting a
/// replacement.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_past_the_readiness_timeout_warns_and_keeps_the_engine() -> TestResult {
    incoming_walks_past_the_readiness_timeout(ANALYZING_ENGINE, "")
        .await
        .map(drop)
}

/// A `sh` engine that reads analyzing on every attempt, as [`ANALYZING_ENGINE`] does,
/// answers the first request after each document it opens at once, and holds its answer to
/// the next one until the walk closes that document. It appends `held` to the file its
/// first argument names when it holds a request, `released` when it answers the held one,
/// and `start` when it starts.
///
/// A walk's retry therefore gets no answer while the walk waits for one: the walk ends it
/// only by abandoning it at its deadline, and the engine answers it after the walk has
/// closed its document, so the next exchange on the session reads that late answer.
#[cfg(unix)]
const HELD_RETRY_ENGINE: &str = r#"echo start >> "$1"
asked=0
held=
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      asked=0 ;;
    *'"method":"textDocument/didClose"'*)
      asked=0
      if [ -n "$held" ]; then
        echo released >> "$1"
        frame "{\"jsonrpc\":\"2.0\",\"id\":$held,\"result\":[]}"
        held=
      fi ;;
    *'"id":'*)
      asked=$((asked + 1))
      if [ "$asked" -eq 2 ]; then
        echo held >> "$1"
        held=$id
      else
        frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}"
      fi ;;
  esac
done
"#;

/// The retry table the held-retry suite gives its engine: the wait after an answer is
/// 1 ms, so a walk sends its retry as soon as the running engine answers its first
/// attempt, and no longer wait decides whether that retry still fits before the walk's
/// deadline.
#[cfg(unix)]
const IMMEDIATE_RETRY: &str = "retry = { delay = \"1ms\", delay_limit = \"1ms\" }\n";

/// A retry still in flight when an incoming walk's wait is spent is abandoned there: the
/// walk answers `engine_analysis_unavailable` inside `[server] readiness_timeout` instead
/// of holding the request past its deadline, and the next walk meets the same engine
/// process, which discards the held answer it reads first.
#[cfg(unix)]
// Traversal timeout failure: https://github.com/volarized/rift/issues/436
#[tokio::test]
async fn search_traversal_abandons_a_retry_in_flight_past_the_readiness_timeout() -> TestResult {
    let log = incoming_walks_past_the_readiness_timeout(HELD_RETRY_ENGINE, IMMEDIATE_RETRY).await?;
    assert!(
        log.lines().any(|line| line == "released"),
        "a walk after the first sends its retry to the held answer, and the engine answers it \
         once the walk has closed its document: {log}"
    );
    Ok(())
}

/// Runs three incoming walks under a 1 s `[server] readiness_timeout` over the `sh` engine
/// `script_source`, whose `[languages.rust.lsp]` table also holds `lsp_keys`, asserts each
/// answers `engine_analysis_unavailable` and that all three meet the one engine process, and
/// returns the log the engine appended to by the end of the third walk.
///
/// The engine's paused-clock unit owns the retry deadline. This integration covers the LSP
/// exchange and the live session after that retry is abandoned.
///
/// The first walk starts the engine, so whatever that start costs falls on it alone: the
/// walks after it spend their wait on a running engine. The second walk's closing
/// notification reaches the engine before the third walk's first request, so its lines
/// are in the returned log.
#[cfg(unix)]
async fn incoming_walks_past_the_readiness_timeout(
    script_source: &str,
    lsp_keys: &str,
) -> TestResult<String> {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, script_source)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n{lsp_keys}",
        script.display(),
        log.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("beacon"))).await?;
    let started_lines = || {
        std::fs::read_to_string(&log).map_or(0, |text| {
            text.lines().filter(|line| *line == "start").count()
        })
    };
    let mut starts_after = Vec::new();
    for _walk in 0..3 {
        let request = path_arguments(
            &captured_seed(&client, "beacon", "lib.rs").await?,
            "incoming",
        )?;
        let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
        assert_eq!(
            failure.code,
            ErrorCode::TemporarilyUnavailable,
            "{failure:?}"
        );
        assert_eq!(failure.retry, RetryDirective::SameRequest, "{failure:?}");
        starts_after.push(started_lines());
    }
    let log = std::fs::read_to_string(&log)?;
    eprintln!("incoming: engine starts after each walk={starts_after:?} log={log:?}");
    assert_eq!(
        starts_after,
        [1, 1, 1],
        "every walk meets the one engine process"
    );

    client.cancel().await?;
    Ok(log)
}

/// A fake engine that registers one `**/*.rs` file watcher once initialized and appends
/// every `workspace/didChangeWatchedFiles` body it receives to the log named by `$1`. It
/// answers every request with an empty list.
const WATCHING_ENGINE: &str = r#"log="$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}" ;;
    *'"method":"initialized"'*)
      frame '{"jsonrpc":"2.0","id":"watch","method":"client/registerCapability","params":{"registrations":[{"id":"watch-rust","method":"workspace/didChangeWatchedFiles","registerOptions":{"watchers":[{"globPattern":"**/*.rs"}]}}]}}' ;;
    *'"method":"workspace/didChangeWatchedFiles"'*)
      printf '%s\n' "$body" >> "$log" ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// A publication's changed files reach the live engine session as one classified
/// `workspace/didChangeWatchedFiles` batch before the next walk asks it anything. The
/// edit touches files no walk opens: one modified, one created, one deleted.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_tells_the_engine_of_files_changed_outside_the_walk() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let notified = engine.path().join("notified.log");
    std::fs::write(&script, WATCHING_ENGINE)?;
    let configuration = format!(
        "[languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n\
         retry = {{ attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }}\n",
        script.display(),
        notified.display()
    );
    let (directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
            ("other.rs", "pub fn other_marker() {}\n"),
            ("gone.rs", "pub fn gone_marker() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let seed = captured_seed(&client, "beacon", "lib.rs").await?;
    let request = tool_request("search", &path_arguments(&seed, "incoming")?);
    let walk = || async {
        let failed = failed_call(client.call_tool(request.clone()).await)?;
        assert_eq!(failed.code, ErrorCode::TemporarilyUnavailable, "{failed:?}");
        Ok::<_, Box<dyn std::error::Error>>(())
    };
    walk().await?;
    assert!(!notified.exists(), "a new session is told nothing");

    std::fs::write(
        directory.path().join("other.rs"),
        "pub fn other_marker() {}\n\npub fn other_marker_two() {}\n",
    )?;
    std::fs::write(
        directory.path().join("added.rs"),
        "pub fn added_marker() {}\n",
    )?;
    std::fs::remove_file(directory.path().join("gone.rs"))?;
    let edited = std::time::Instant::now();
    for (name, present) in [
        ("other_marker_two", true),
        ("added_marker", true),
        ("gone_marker", false),
    ] {
        published_holds(&client, name, present, edited).await?;
    }
    let published = edited.elapsed();
    walk().await?;
    let fed = edited.elapsed();

    let text = std::fs::read_to_string(&notified)?;
    let mut changes: Vec<(String, u64)> = Vec::new();
    for line in text.lines() {
        let body: Value = serde_json::from_str(line)?;
        for change in body["params"]["changes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let uri = change["uri"].as_str().unwrap_or_default();
            let name = uri.rsplit('/').next().unwrap_or_default().to_owned();
            changes.push((name, change["type"].as_u64().unwrap_or_default()));
        }
    }
    eprintln!(
        "feed: published after {published:?}, walk answered after {fed:?}, \
         batches={} changes={changes:?}",
        text.lines().count()
    );
    changes.sort();
    assert_eq!(
        changes,
        vec![
            ("added.rs".to_owned(), 1),
            ("gone.rs".to_owned(), 3),
            ("other.rs".to_owned(), 2),
        ],
        "created, deleted, and changed reach the engine once each: {text}"
    );

    client.cancel().await?;
    Ok(())
}

/// The rift-lsp live Rust fixture, served with rust-analyzer 1.98 through rustup.
const LIVE_RUST_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../../rift-lsp/tests/fixtures/rust/Cargo.toml"),
    ),
    (
        "lib.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/lib.rs"),
    ),
    (
        "hub.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/hub.rs"),
    ),
    (
        "walk.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/walk.rs"),
    ),
    (
        "caller.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/caller.rs"),
    ),
];

const LIVE_RUST_ENGINE: &str =
    "[languages.rust.lsp]\ncommand = [\"rustup\", \"run\", \"1.98\", \"rust-analyzer\"]\n";

/// Walks from `seed` in `direction` until the reached names equal `expected`, at most
/// [`LIVE_WALK_ATTEMPTS_MAX`] times, printing each answer with its wall time. Returns
/// the attempt that matched and its answer.
async fn live_walk(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    label: &str,
    seed: &str,
    direction: &str,
    expected: &[&str],
) -> TestResult<(usize, Value)> {
    for attempt in 1..=LIVE_WALK_ATTEMPTS_MAX {
        let started = std::time::Instant::now();
        let structured = call_retrying_acceptance(
            client,
            tool_request("search", &path_arguments(seed, direction)?),
        )
        .await?;
        let reached = symbol_names(&structured);
        eprintln!(
            "live {label} attempt {attempt}: elapsed={:?} reached={reached:?} warnings={}",
            started.elapsed(),
            structured["warnings"]
        );
        if reached == expected {
            return Ok((attempt, structured));
        }
    }
    Err(format!("{label} never answered {expected:?}").into())
}

/// Reads the established seed from the captured fixture before the first engine walk.
async fn live_seed(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    language: &str,
) -> TestResult<String> {
    let answer =
        call_retrying_acceptance(client, tool_request("search", &name_arguments(name))).await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("one exact fixture seed: {answer}").into());
    };
    let identity = query::iri(row, "symbol")?;
    let parsed = rift_protocol::identity::SymbolIdentity::parse(identity)?;
    assert_eq!(parsed.owner(), &rift_protocol::identity::SymbolOwner::Local);
    assert!(
        identity.starts_with(&format!("rift://symbol/local/{language}/")),
        "{identity}"
    );
    assert_eq!(parsed.wire_identity(), identity);
    Ok(identity.to_owned())
}

/// Walks one live answer may take while rust-analyzer loads the fixture.
const LIVE_WALK_ATTEMPTS_MAX: usize = 10;

/// The first walk on a cold rust-analyzer answers the callee on its first request, since
/// the walk waits out the load instead of taking the empty prepare rust-analyzer answers
/// while loading. rust-analyzer runs in client watcher mode, so it learns of a file no read
/// opens only through the server's feed. `beacon` becomes `beacon2` in hub.rs, below a
/// new `helper`, and caller.rs gains `again`. The outgoing walk from `larger` opens only
/// walk.rs, and the incoming read of `beacon2` opens only hub.rs.
#[tokio::test]
async fn live_rust_analyzer_walks_answer_a_rename_in_an_unopened_file() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let initial_walk = format!(
        "{}\npub fn local(value: i32) -> i32 {{ crate::hub::beacon(value) }}\n",
        include_str!("../../rift-lsp/tests/fixtures/rust/walk.rs")
    );
    let files = LIVE_RUST_FILES
        .iter()
        .map(|(path, source)| {
            (
                *path,
                if *path == "walk.rs" {
                    initial_walk.as_str()
                } else {
                    *source
                },
            )
        })
        .collect::<Vec<_>>();
    let (directory, client, _server_task) =
        served_workspace(&files, Some(LIVE_RUST_ENGINE.to_owned())).await?;
    let larger = live_seed(&client, "larger", "rust").await?;
    let beacon = live_seed(&client, "beacon", "rust").await?;
    let request = path_arguments(&larger, "outgoing")?;
    let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
    assert_eq!(
        failure.code,
        ErrorCode::TemporarilyUnavailable,
        "{failure:?}"
    );
    assert_eq!(failure.retry, RetryDirective::SameRequest, "{failure:?}");
    assert!(
        failure.message.starts_with(
            "workspace index is unavailable during query relationships: detail the outgoing target has no admitted source owner: uri file://"
        ) && failure.message.contains("/library/core/src/cmp.rs, served root ")
            && failure.message.ends_with("; resend the same request after a short delay"),
        "the unadmitted sysroot target refuses the complete query: {failure:?}"
    );
    assert!(failure.text.contains("rift.server.read_unavailable"));
    let local = live_seed(&client, "local", "rust").await?;
    let (cold, _) = live_walk(&client, "outgoing before", &local, "outgoing", &["beacon"]).await?;
    assert_eq!(cold, 1);
    live_walk(
        &client,
        "incoming before",
        &beacon,
        "incoming",
        &["larger", "local", "total"],
    )
    .await?;
    eprintln!("live: warm after {:?}", started.elapsed());

    std::fs::write(
        directory.path().join("hub.rs"),
        "pub fn helper() -> i32 {\n    0\n}\n\npub fn beacon2(value: i32) -> i32 {\n    value\n}\n",
    )?;
    std::fs::write(
        directory.path().join("walk.rs"),
        initial_walk.replace("beacon", "beacon2"),
    )?;
    std::fs::write(
        directory.path().join("caller.rs"),
        "use crate::hub::beacon2;\n\npub fn total() -> i32 {\n    beacon2(2)\n}\n\n\
         pub fn again() -> i32 {\n    beacon2(3)\n}\n",
    )?;
    let edited = std::time::Instant::now();
    for (name, present) in [("beacon2", true), ("again", true), ("beacon", false)] {
        published_holds(&client, name, present, edited).await?;
    }
    eprintln!("live: published after {:?}", edited.elapsed());
    let (outgoing, _) =
        live_walk(&client, "outgoing after", &local, "outgoing", &["beacon2"]).await?;
    eprintln!("live: outgoing fresh after {:?}", edited.elapsed());
    let beacon2 = live_seed(&client, "beacon2", "rust").await?;
    let (incoming, _) = live_walk(
        &client,
        "incoming after",
        &beacon2,
        "incoming",
        &["again", "larger", "local", "total"],
    )
    .await?;
    eprintln!("live: incoming fresh after {:?}", edited.elapsed());
    assert_eq!(
        (outgoing, incoming),
        (1, 1),
        "both answer the edit on the first walk"
    );

    client.cancel().await?;
    Ok(())
}

/// The rift-lsp live TypeScript fixture: `larger` in walk.ts calls `beacon` from hub.ts and
/// `Math.max` from TypeScript's own lib, and `total` and the TSX `Banner` call `beacon` too.
#[cfg(unix)]
const LIVE_TYPESCRIPT_FILES: &[(&str, &str)] = &[
    (
        "tsconfig.json",
        include_str!("../../rift-lsp/tests/fixtures/typescript/tsconfig.json"),
    ),
    (
        "hub.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/hub.ts"),
    ),
    (
        "caller.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/caller.ts"),
    ),
    (
        "view.tsx",
        include_str!("../../rift-lsp/tests/fixtures/typescript/view.tsx"),
    ),
    (
        "walk.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/walk.ts"),
    ),
];

/// The typescript-language-server the fixture's lockfile pins, run from the workspace's
/// own `node_modules` for both dialects, with that folder left out of the index.
#[cfg(unix)]
fn live_typescript_engine() -> String {
    format!(
        "[source]\nexclude = [\"node_modules/**\"]\n\n\
         [languages.typescript]\nlsp = \"typescript\"\n\n\
         [languages.\"typescript:tsx\"]\nlsp = \"typescript\"\n\n\
         [lsp.typescript]\ncommand = [\"{}\", \"--stdio\"]\n\n\
         [lsp.typescript.initialization_options.tsserver]\nuseSyntaxServer = \"never\"\n",
        typescript_install::LANGUAGE_SERVER_PROGRAM
    )
}

#[cfg(unix)]
fn install_captured_typescript_engine(root: &std::path::Path) {
    typescript_install::install_typescript_engine(root);
    let path = root.join("package.json");
    let mut metadata: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("installed package metadata"))
            .expect("captured package metadata");
    metadata["version"] = json!("1.0.0");
    metadata["exports"] = json!({
        "./hub": "./hub.ts",
        "./walk": "./walk.ts",
        "./caller": "./caller.ts",
        "./view": "./view.tsx"
    });
    std::fs::write(
        path,
        serde_json::to_vec(&metadata).expect("package metadata bytes"),
    )
    .expect("captured package entry points");
}

/// typescript-language-server answers both walk directions through `search`, each on its
/// first walk, since the walk waits out the project load. The outgoing walk from `larger`
/// reaches `beacon` and the physically admitted `Math.max` declaration in the installed
/// `typescript` package. Its original source remains readable without global naming.
/// The incoming walk to `beacon` reaches `larger`, `total`, and the TSX `Banner`.
#[cfg(unix)]
#[tokio::test]
async fn live_typescript_language_server_walks_both_directions() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut files = typescript_install::typescript_package_files().to_vec();
    files.extend_from_slice(LIVE_TYPESCRIPT_FILES);
    let (directory, client, _server_task) = workspace_client::served_prepared_workspace(
        &files,
        Some(live_typescript_engine()),
        install_captured_typescript_engine,
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("beacon"))).await?;
    let larger = live_seed(&client, "larger", "typescript").await?;
    let beacon = live_seed(&client, "beacon", "typescript").await?;

    let total = live_seed(&client, "total", "typescript").await?;
    let (outgoing, _) = live_walk(
        &client,
        "typescript outgoing",
        &total,
        "outgoing",
        &["beacon"],
    )
    .await?;
    let (incoming, answer) = live_walk(
        &client,
        "typescript incoming",
        &beacon,
        "incoming",
        &["Banner", "larger", "total"],
    )
    .await?;
    eprintln!("live typescript: incoming after {:?}", started.elapsed());
    assert!(answer["warnings"].is_null(), "{answer}");
    assert_eq!(
        (outgoing, incoming),
        (1, 1),
        "both walks answer on the first request"
    );
    assert_captured_typescript_call(&client, directory.path(), &larger).await?;

    client.cancel().await?;
    Ok(())
}

/// Reads a physical package callee from one query response and its original captured source.
#[cfg(unix)]
async fn assert_captured_typescript_call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    root: &std::path::Path,
    seed: &str,
) -> TestResult {
    let seed = oxigraph::model::NamedNodeRef::new(seed)?;
    let statement = format!(
        "SELECT ?symbol ?name ?unit ?source ?start ?end ?full_source {{ \
         {seed} :calls ?symbol . ?symbol :name ?name; :declarations ?declaration . \
         ?declaration :unit ?unit; :source ?source; :range ?range . \
         ?range :start ?start; :end ?end . ?unit :source ?full_source }} ORDER BY ?name"
    );
    let answer =
        call_retrying_acceptance(client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(projected_names(&answer)?, ["beacon", "max"]);
    let row = callee_hit(&answer, "max")?;
    assert!(
        matches!(row["symbol"]["type"].as_str(), Some("iri" | "blank_node")),
        "the original physical endpoint remains an RDF resource: {row}"
    );
    let unit = query::iri(&row, "unit")?;
    let (owner, path) = rift_protocol::identity::parse_released_source_identity(unit)?;
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(
        root.join("node_modules/typescript/package.json"),
    )?)?;
    assert!(
        matches!(owner, rift_protocol::identity::SymbolOwner::Package { manager, registry, name, version }
            if manager == "npm" && registry == "npmjs.org" && name == "typescript"
                && manifest["version"].as_str() == Some(version.as_str())),
        "the physical declaration retains its installed package owner: {row}"
    );
    let source_path = root.join("node_modules/typescript").join(&path);
    let original = std::fs::read_to_string(&source_path)?;
    assert_eq!(query::literal(&row, "full_source")?, original);
    let start: usize = query::literal(&row, "start")?.parse()?;
    let end: usize = query::literal(&row, "end")?.parse()?;
    assert_eq!(
        query::literal(&row, "source")?,
        original
            .get(start..end)
            .ok_or("original package declaration range")?
    );
    std::fs::write(source_path, "// changed after the captured query\n")?;
    let held = call_retrying_acceptance(
        client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":unit,"view":answer["view"]["id"]}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&held)? else {
        return Err(format!("one original physical source: {held}").into());
    };
    assert_eq!(query::literal(source_row, "outcome")?, "found");
    assert_eq!(query::literal(source_row, "complete")?, "true");
    assert_eq!(query::iri(source_row, "unit")?, unit);
    assert_eq!(query::literal(source_row, "start")?.parse::<u64>()?, 0);
    assert_eq!(
        query::literal(source_row, "end")?.parse::<usize>()?,
        original.len()
    );
    assert_eq!(query::literal(source_row, "text")?, original);
    assert_eq!(
        query::literal(source_row, "source_view")?,
        answer["view"]["id"]
            .as_str()
            .ok_or("physical source view")?
    );
    assert_eq!(
        query::literal(source_row, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("physical source expiry")?
    );
    Ok(())
}

/// Waits until the current publication holds `name` as a symbol, or no longer does,
/// searching again [`PUBLICATION_POLL`] after each answer until [`PUBLICATION_WAIT`] has
/// passed since `edited`, the instant the files were written.
///
/// The wait is bounded by elapsed time, not by a count of searches: a search takes longer
/// on a slower runner, and a count of them could outlast the harness's deadline before
/// this message prints.
async fn published_holds(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    present: bool,
    edited: std::time::Instant,
) -> TestResult {
    let mut searches = 0_u32;
    let declaration = oxigraph::model::Literal::from(format!("fn {name}("));
    let statement = format!(
        "ASK {{ ?unit a :SourceUnitId; :source ?source FILTER(CONTAINS(?source, {declaration})) }}"
    );
    loop {
        let structured =
            call_retrying_acceptance(client, tool_request("search", &json!({"query":statement})))
                .await?;
        searches += 1;
        if structured["answer"]["result"] != "boolean" {
            return Err(format!("publication source query must answer ASK: {structured}").into());
        }
        if structured["answer"]["value"].as_bool() == Some(present) {
            return Ok(());
        }
        if edited.elapsed() >= PUBLICATION_WAIT {
            return Err(format!(
                "the publication did not settle on {name} present={present} within \
                 {PUBLICATION_WAIT:?} of the edit, after {searches} searches"
            )
            .into());
        }
        tokio::time::sleep(PUBLICATION_POLL).await;
    }
}

/// How long an edit's publication may take to become current, from the edit: a fifth of
/// nextest's 60 s deadline, so a publication that never lands fails with its own message.
const PUBLICATION_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The pause between two searches while [`published_holds`] waits.
const PUBLICATION_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The embedded `ty` engine under the default retry table and `settle_delay`: it declares
/// itself ready at its start, so a walk takes its first answer.
const OUTGOING_ENGINE: &str = "[languages.python.lsp]\nembedded = \"ty\"\n";

/// [`CALL_GRAPH_FILES`] plus a module that calls into the standard library and holds a
/// class whose body makes a call.
const OUTGOING_FILES: &[(&str, &str)] = &[
    ("pyproject.toml", OUTGOING_METADATA),
    CALL_GRAPH_FILES[1],
    (
        "extra.py",
        "from graph import leaf\n\nLIMIT = 3\n\n\
         def counted() -> int:\n    return len([leaf()]) + LIMIT\n\n\
         class Holder:\n    size = counted()\n\n    def grow(self) -> int:\n        return root_call()\n\n\
         def root_call() -> int:\n    return 1\n",
    ),
];

#[tokio::test]
async fn search_traversal_outgoing_depth_one_reaches_the_direct_callees() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "root", "graph.py").await?;
    let answer = graph(&client, &seed, ":calls").await?;
    assert_eq!(projected_names(&answer)?, ["branch_a", "branch_b"]);
    assert!(answer["warnings"].is_null(), "{answer}");
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_outgoing_depth_two_reaches_the_callees_of_the_callees() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "root", "graph.py").await?;
    let answer = graph(&client, &seed, "(:calls|:calls/:calls)").await?;
    assert_eq!(projected_names(&answer)?, ["branch_a", "branch_b", "leaf"]);
    let leaf = captured_seed(&client, "leaf", "graph.py").await?;
    let statement = format!(
        "SELECT ?symbol ?name {{ <{seed}> :calls/:calls ?symbol . ?symbol :name ?name . FILTER(?symbol = <{leaf}>) }} ORDER BY ?name"
    );
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(projected_names(&answer)?, ["leaf", "leaf"]);
    client.cancel().await?;
    Ok(())
}

/// Retrieval joins calls; unsupported required relationships refuse the complete query.
#[tokio::test]
async fn search_traversal_outgoing_merges_with_query_and_filters_on_calls() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "root", "graph.py").await?;
    let statement = format!(
        "SELECT DISTINCT ?symbol ?name {{ SERVICE :search {{ ?hit :query 'branch_a'; :target 'symbol'; :symbol ?symbol }} <{seed}> :calls ?symbol . ?symbol :name ?name FILTER(?name = 'branch_a') }} ORDER BY ?name"
    );
    let request = serde_json::from_value(json!({"query":statement}))?;
    let answer = workspace_client::search_after_population(&client, &request).await?;
    assert_eq!(projected_names(&answer)?, ["branch_a"]);
    let request = serde_json::to_value(query::path(&seed, "(:calls|:implements)")?)?;
    required_facts_unavailable(&client, &request, ErrorCode::CapabilityUnavailable).await?;
    let request = serde_json::to_value(query::path(&seed, ":implements")?)?;
    required_facts_unavailable(&client, &request, ErrorCode::CapabilityUnavailable).await?;
    client.cancel().await?;
    Ok(())
}

/// A missing required runtime contribution refuses; class calls still use their own body.
#[tokio::test]
async fn search_traversal_outgoing_drops_a_standard_library_callee_and_walks_a_class_body()
-> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(OUTGOING_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;
    let seed = captured_seed(&client, "counted", "extra.py").await?;
    let request = serde_json::to_value(query::path(&seed, ":calls")?)?;
    let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
    assert_eq!(
        failure.code,
        ErrorCode::TemporarilyUnavailable,
        "{failure:?}"
    );
    assert_eq!(failure.retry, RetryDirective::SameRequest, "{failure:?}");
    assert!(
        failure.message.contains("dependency query source")
            && failure
                .message
                .contains("the selected runtime source has no admitted owner"),
        "the missing captured runtime owner refuses the complete query: {failure:?}"
    );
    assert!(failure.text.contains("rift.server.read_unavailable"));
    let seed = captured_seed(&client, "Holder", "extra.py").await?;
    let answer = graph(&client, &seed, ":calls").await?;
    assert_eq!(projected_names(&answer)?, ["counted"]);
    client.cancel().await?;
    Ok(())
}

fn name_arguments(name: &str) -> Value {
    let name = oxigraph::model::Literal::from(name);
    json!({"query":format!("SELECT ?symbol ?name {{ ?symbol :name ?name FILTER(?name = {name}) }} ORDER BY ?symbol")})
}

fn path_arguments(seed: &str, direction: &str) -> TestResult<Value> {
    let expression = match direction {
        "incoming" => "^:references",
        "outgoing" => ":calls",
        _ => return Err("fixture direction must be incoming or outgoing".into()),
    };
    Ok(serde_json::to_value(query::path(seed, expression)?)?)
}

async fn graph(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    seed: &str,
    expression: &str,
) -> TestResult<Value> {
    let request = serde_json::to_value(query::path(seed, expression)?)?;
    call_retrying_acceptance(client, tool_request("search", &request)).await
}

/// Once the engine reads ready, an empty prepare at the seed refuses, naming the seed's
/// kind; the incoming walk's other refusals stay: a walk beside `change` refuses.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_from_a_seed_without_a_call_hierarchy_item_refuses() -> TestResult
{
    let (_engine, configuration) = fake_engine::rust_engine(fake_engine::READY_UNPREPARED_ENGINE)?;
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            ("lib.rs", "pub struct Beacon;\n\npub fn caller() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("Beacon"))).await?;

    let request = path_arguments(
        &captured_seed(&client, "Beacon", "lib.rs").await?,
        "outgoing",
    )?;
    let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
    assert_eq!(
        failure.code,
        ErrorCode::TemporarilyUnavailable,
        "{failure:?}"
    );
    assert!(
        failure.text.contains("prepared no call hierarchy item"),
        "{failure:?}"
    );
    client.cancel().await?;
    Ok(())
}

/// The `PROGRESS` step of an engine that begins and ends its work at start, so the session
/// reads it ready once quiet.
#[cfg(unix)]
const ANNOUNCED_WORK: &str = announced_work::ANNOUNCED_WORK;

/// A line past the end of `lib.rs`, which no served byte holds.
#[cfg(unix)]
const UNSERVED_LINES: fake_engine::ScriptedLines = fake_engine::ScriptedLines {
    callee: 50,
    caller: 50,
};

/// An engine that announces no work reads unconfirmed, so a walk in either direction takes
/// its answer once the session read it quiet past `settle_delay`, and the answer names the
/// engine in `engine_readiness_unconfirmed`.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_over_an_engine_that_announces_nothing_warns_readiness_unconfirmed()
-> TestResult {
    let (_engine, (_directory, client, _server_task)) = fake_engine::scripted_calls_workspace(
        fake_engine::UNANNOUNCED_WORK,
        fake_engine::SERVED_LINES,
    )
    .await?;
    for (seed, direction, reached) in [
        (
            captured_seed(&client, "caller", "lib.rs").await?,
            "outgoing",
            "beacon",
        ),
        (
            captured_seed(&client, "beacon", "lib.rs").await?,
            "incoming",
            "caller",
        ),
    ] {
        let structured = call_retrying_acceptance(
            &client,
            tool_request("search", &path_arguments(&seed, direction)?),
        )
        .await?;
        assert_eq!(symbol_names(&structured), [reached], "{structured}");
        let warnings = structured["warnings"]
            .as_array()
            .ok_or("an unconfirmed engine warns")?;
        assert_eq!(warnings.len(), 1, "{structured}");
        assert_eq!(
            warnings[0]["code"],
            json!("engine_readiness_unconfirmed"),
            "{structured}"
        );
        assert_eq!(warnings[0]["processes"], json!(["rust"]), "{structured}");
    }

    client.cancel().await?;
    Ok(())
}

/// An engine answer naming a line the served `lib.rs` does not hold drops the engine's
/// contribution with `engine_analysis_unavailable`, in either direction. The indexed store
/// holds no edge, and the walk still answers with the warning instead of refusing.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_over_an_unmappable_engine_answer_warns_in_both_directions() -> TestResult
{
    let (_engine, (_directory, client, _server_task)) =
        fake_engine::scripted_calls_workspace(ANNOUNCED_WORK, UNSERVED_LINES).await?;
    for (seed, direction) in [
        (
            captured_seed(&client, "caller", "lib.rs").await?,
            "outgoing",
        ),
        (
            captured_seed(&client, "beacon", "lib.rs").await?,
            "incoming",
        ),
    ] {
        let request = path_arguments(&seed, direction)?;
        let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
        assert!(
            failure
                .text
                .contains("position line 50 is outside document line count 6"),
            "{failure:?}"
        );
    }

    client.cancel().await?;
    Ok(())
}

/// A `sh` engine that reads analyzing on every attempt, as [`ANALYZING_ENGINE`] does, and
/// holds its answer to the first request after its first `didOpen` for 1.2 s: past a 1 s
/// `[server] readiness_timeout` for the walk that sent it, and over before the next walk
/// needs an answer. It appends `start` at each start, `open` and `close` per document
/// notification, and `late` when it reads the held request, to the file its first
/// argument names.
#[cfg(unix)]
const LATE_FIRST_ANSWER_ENGINE: &str = r#"echo start >> "$1"
opens=0
asked=0
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"method":"textDocument/didOpen"'*)
      opens=$((opens + 1))
      echo open >> "$1" ;;
    *'"method":"textDocument/didClose"'*)
      echo close >> "$1" ;;
    *'"id":'*)
      asked=$((asked + 1))
      if [ "$opens" -eq 1 ] && [ "$asked" -eq 1 ]; then
        echo late >> "$1"
        sleep 1.2
      fi
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// A walk whose engine answers only after the request deadline is refused with
/// `temporarily_unavailable` and `retry: same_request`, and keeps its engine session:
/// the next walk is served by the same engine process, which reads `didClose` for the
/// document the refused walk left open before it reads that document's next `didOpen`,
/// and the refused walk's late answer is discarded on the way.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_refused_at_the_request_deadline_keeps_the_engine() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, LATE_FIRST_ANSWER_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        log.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("beacon"))).await?;
    let walk = tool_request(
        "search",
        &path_arguments(
            &captured_seed(&client, "beacon", "lib.rs").await?,
            "incoming",
        )?,
    );

    let started = std::time::Instant::now();
    let refused = failed_call(client.call_tool(walk.clone()).await)?;
    assert_eq!(
        refused.code,
        ErrorCode::TemporarilyUnavailable,
        "{refused:?}"
    );
    assert_eq!(refused.retry, RetryDirective::SameRequest, "{refused:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
    let refused = failed_call(client.call_tool(walk).await)?;
    assert_eq!(
        refused.code,
        ErrorCode::TemporarilyUnavailable,
        "{refused:?}"
    );
    client.cancel().await?;
    let recorded = std::fs::read_to_string(&log)?;
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        lines.iter().filter(|line| **line == "start").count(),
        1,
        "both walks meet the one engine process: {lines:?}"
    );
    assert_eq!(
        lines.get(1..5),
        Some(&["open", "late", "close", "open"][..]),
        "the next walk closes the refused walk's document before it opens it again"
    );
    Ok(())
}

/// A `sh` engine that takes 1.2 s to start, past a 1 s `[server] readiness_timeout`, and
/// then reads analyzing on every attempt, as [`ANALYZING_ENGINE`] does. Each start appends
/// `start` to the file its first argument names.
#[cfg(unix)]
const SLOW_START_ENGINE: &str = r#"echo start >> "$1"
sleep 1.2
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// A walk whose engine is still starting when `[server] readiness_timeout` is spent is
/// refused with `temporarily_unavailable` and `retry: same_request`, and the start
/// keeps running: the next walk meets the one engine process that start brought up.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_refused_while_the_engine_starts_keeps_the_start() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let log = engine.path().join("engine.log");
    std::fs::write(&script, SLOW_START_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        log.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("beacon"))).await?;
    let walk = tool_request(
        "search",
        &path_arguments(
            &captured_seed(&client, "beacon", "lib.rs").await?,
            "incoming",
        )?,
    );

    let refused = failed_call(client.call_tool(walk.clone()).await)?;
    assert_eq!(
        refused.code,
        ErrorCode::TemporarilyUnavailable,
        "{refused:?}"
    );
    assert_eq!(refused.retry, RetryDirective::SameRequest, "{refused:?}");

    let refused = failed_call(client.call_tool(walk).await)?;
    assert_eq!(
        refused.code,
        ErrorCode::TemporarilyUnavailable,
        "{refused:?}"
    );
    client.cancel().await?;
    let recorded = std::fs::read_to_string(&log)?;
    assert_eq!(
        recorded.lines().filter(|line| *line == "start").count(),
        1,
        "both walks meet the one engine process: {recorded:?}"
    );
    Ok(())
}

/// A `sh` engine serving references and call hierarchy that announces work it never
/// ends; each start appends one line to the file its first argument names.
#[cfg(unix)]
const ANALYZING_CALL_HIERARCHY_ENGINE: &str = r#"echo start >> "$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// An outgoing walk whose engine still reads analyzing when its wait is spent
/// answers with `engine_analysis_unavailable` and keeps the session: the second walk
/// meets the same engine process instead of starting a replacement.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_past_the_readiness_timeout_warns_and_keeps_the_engine()
-> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let starts = engine.path().join("starts.log");
    std::fs::write(&script, ANALYZING_CALL_HIERARCHY_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        starts.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("caller"))).await?;
    let started_lines = || std::fs::read_to_string(&starts).map_or(0, |text| text.lines().count());
    let mut starts_after = Vec::new();
    for _walk in 0..2 {
        let started = std::time::Instant::now();
        let request = path_arguments(
            &captured_seed(&client, "caller", "lib.rs").await?,
            "outgoing",
        )?;
        let failure = failed_call(client.call_tool(tool_request("search", &request)).await)?;
        let elapsed = started.elapsed();
        assert_eq!(
            failure.code,
            ErrorCode::TemporarilyUnavailable,
            "{failure:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
        starts_after.push(started_lines());
    }
    assert_eq!(
        starts_after,
        [1, 1],
        "both walks meet the one engine process"
    );

    client.cancel().await?;
    Ok(())
}

/// The relationship graph serves the project alone, and package facts come from the
/// global index, so a `global` search leaves a walk nothing to run over and refuses naming
/// `traversal`.
#[tokio::test]
async fn a_global_search_with_traversal_refuses_invalid_request() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;
    call_retrying_acceptance(&client, tool_request("search", &name_arguments("beacon"))).await?;

    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &json!({
                    "query": "beacon",
                    "scope": "global",
                    "traversal": {"seed": captured_seed(&client, "beacon", "lib.rs").await?}
                }),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest, "{}", failure.text);
    assert!(
        failure.message.contains("query") && failure.message.contains("language"),
        "the refusal names the accepted query fields: {}",
        failure.message
    );

    client.cancel().await?;
    Ok(())
}

/// The `[global]` table pointing at `endpoint`, with bounds a loopback fixture meets.
fn global_table(endpoint: &str) -> String {
    format!(
        "[global]\nenabled = true\nendpoint = \"{endpoint}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"5s\"\n"
    )
}

/// The fixture global API holding the Python collection the callee walks name.
async fn python_global_api() -> TestResult<global_api::GlobalFixture> {
    Ok(
        global_api::GlobalFixture::start_with(global_api::FixtureOptions {
            python_collection: true,
            ..global_api::FixtureOptions::default()
        })
        .await?,
    )
}

/// The declaration requests the fixture received.
async fn declaration_requests(fixture: &global_api::GlobalFixture) -> Vec<Value> {
    fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.ends_with("/declarations"))
        .filter_map(|request| request.body)
        .collect()
}

/// The projected row naming one reached canonical declaration.
fn callee_hit(structured: &Value, name: &str) -> TestResult<Value> {
    query::rows(structured)?
        .iter()
        .find(|row| query::literal(row, "name").ok() == Some(name))
        .cloned()
        .ok_or_else(|| format!("{name} must be a projected symbol: {structured}").into())
}

async fn read_dependency_hit(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    answer: &Value,
    name: &str,
) -> TestResult<Value> {
    let row = callee_hit(answer, name)?;
    let identity = query::iri(&row, "symbol")?;
    let selected = &answer["view"];
    let symbol = call_retrying_acceptance(
        client,
        tool_request(
            "get_symbol",
            &json!({
                "id":identity,"view":selected["id"],"include":["source"]
            }),
        ),
    )
    .await?;
    assert_eq!(symbol["outcome"], "found", "{symbol}");
    assert_eq!(symbol["view"], *selected, "{symbol}");
    let unit = symbol["declarations"][0]["unit"]
        .as_str()
        .ok_or("dependency declaration source unit")?;
    let source = call_retrying_acceptance(
        client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":unit,"view":selected["id"]}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&source)? else {
        return Err(format!("one original dependency source: {source}").into());
    };
    assert_eq!(query::literal(source_row, "outcome")?, "found");
    assert_eq!(
        query::literal(source_row, "source_view")?,
        selected["id"].as_str().ok_or("dependency source view")?
    );
    assert_eq!(
        query::literal(source_row, "source_expiry")?,
        selected["expires_at"]
            .as_str()
            .ok_or("dependency source expiry")?
    );
    assert_eq!(
        query::literal(source_row, "text")?,
        global_api::GREETING_SOURCE
    );
    Ok(symbol)
}

async fn held_dependency_graph(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    original: &Value,
) -> TestResult {
    let view = original["view"]["id"]
        .as_str()
        .ok_or("retained dependency execution")?;
    let statement = format!(
        "SELECT ?symbol ?name {{ SERVICE <{}{view}> {{}} }} ORDER BY ?name",
        rift_search::sparql::RESULT_SERVICE_PREFIX
    );
    let held =
        call_retrying_acceptance(client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(held["answer"], original["answer"]);
    assert_eq!(held["view"]["expires_at"], original["view"]["expires_at"]);
    read_dependency_hit(client, original, "greet").await?;
    Ok(())
}

/// A standard library callee retains its resolved runtime owner, typeshed source unit
/// and exact declaration. Admitted source supplies its name without remote declaration lookup.
#[tokio::test]
async fn search_traversal_outgoing_names_a_standard_library_callee_by_its_global_id() -> TestResult
{
    let fixture = python_global_api().await?;
    let configuration = format!("{}\n{OUTGOING_ENGINE}", global_table(&fixture.endpoint));
    let (_directory, client, _server_task) = workspace_client::served_prepared_workspace(
        OUTGOING_FILES,
        Some(configuration),
        python_runtime_environment::prepare,
    )
    .await?;

    let counted = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &path_arguments(
                &captured_seed(&client, "counted", "extra.py").await?,
                "outgoing",
            )?,
        ),
    )
    .await?;
    assert_eq!(symbol_names(&counted), ["leaf", "len"], "{counted}");
    let len = callee_hit(&counted, "len")?;
    let id = global_api::named_identity(&global_api::NAMED_POSITIONS[0]);
    assert_eq!(query::iri(&len, "symbol")?, id);
    let requests_before_exact = fixture.requests().await.len();
    let exact = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({
                "id":id,"view":counted["view"]["id"],"include":["source"]
            }),
        ),
    )
    .await?;
    assert_eq!(
        fixture.requests().await.len(),
        requests_before_exact,
        "the original captured runtime read sends no new HTTP request"
    );
    assert_eq!(exact["symbol"]["kind"], "function");
    assert_eq!(
        exact["symbol"]["origin"],
        json!({
            "location":"stdlib","source_kind":"authored",
            "runtime":{"runtime":"cpython","version":global_api::python_runtime::VERSION}
        })
    );
    assert_eq!(
        exact["declarations"][0]["unit"],
        global_api::named_unit(&global_api::NAMED_POSITIONS[0])
    );
    assert_eq!(exact["view"], counted["view"], "{exact}");
    let original = global_api::python_runtime::builtins();
    let declaration = &exact["declarations"][0];
    let start = usize::try_from(
        declaration["range"]["start"]
            .as_u64()
            .ok_or("original runtime declaration start")?,
    )?;
    let end = usize::try_from(
        declaration["range"]["end"]
            .as_u64()
            .ok_or("original runtime declaration end")?,
    )?;
    assert_eq!(
        declaration["source"].as_str(),
        Some(
            original
                .get(start..end)
                .ok_or("original runtime declaration range")?
        ),
        "{exact}"
    );
    assert!(counted["warnings"].is_null(), "{counted}");

    assert!(declaration_requests(&fixture).await.is_empty());

    let source_requests: Vec<Value> = fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.ends_with("/source"))
        .filter_map(|request| request.body)
        .collect();
    assert!(
        source_requests.is_empty(),
        "the original captured stub serves source without a network read: {source_requests:?}"
    );

    client.cancel().await?;
    Ok(())
}

/// `greeting` 1.0.0 as `uv sync` installs it into the workspace's own `.venv`, with the
/// lockfile naming it and a module calling both its functions.
#[cfg(unix)]
const INSTALLED_FILES: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"greeting\"]\n[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n",
    ),
    (
        "uv.lock",
        "version = 1\n\n\
         [[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n\n\
         [[package]]\nname = \"greeting\"\nversion = \"1.0.0\"\n\
         source = { registry = \"https://pypi.org/simple\" }\n",
    ),
    (
        ".venv/pyvenv.cfg",
        "home = /usr/bin\nversion_info = 3.12.4\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting/__init__.py",
        "from greeting.core import greet, quiet\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting/core.py",
        "def greet(name: str) -> str:\n    return name\n\n\ndef quiet() -> None:\n    return None\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting-1.0.0.dist-info/RECORD",
        "greeting/__init__.py,,\ngreeting/core.py,,\ngreeting-1.0.0.dist-info/RECORD,,\n",
    ),
    (
        "app.py",
        "from greeting.core import greet, quiet\n\n\n\
         def hello() -> str:\n    quiet()\n    return greet(\"rift\")\n",
    ),
];

/// A capable admitted engine continues beyond a package entry when global operations
/// explicitly report unsupported capabilities.
#[cfg(unix)]
#[tokio::test]
async fn search_query_continues_three_calls_through_an_installed_dependency() -> TestResult {
    let fixture = global_api::GlobalFixture::start_with(global_api::FixtureOptions {
        python_collection: true,
        withheld_features: &["declarations", "relationships"],
        ..global_api::FixtureOptions::default()
    })
    .await?;
    let source = "def greet(name: str) -> str:\n    return _helper(name)\n\ndef _helper(name: str) -> str:\n    return _inner(name)\n\ndef _inner(name: str) -> str:\n    return name\n";
    let files = INSTALLED_FILES.iter().map(|(path, original)| (*path, match *path {
        "app.py" => "from greeting.core import greet\n\ndef hello() -> str:\n    return greet('rift')\n",
        ".venv/lib/python3.12/site-packages/greeting/core.py" => source,
        _ => *original,
    })).collect::<Vec<_>>();
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n{OUTGOING_ENGINE}",
        global_table(&fixture.endpoint)
    );
    let (directory, client, _server_task) = served_workspace(&files, Some(configuration)).await?;
    let seed = captured_seed(&client, "hello", "app.py").await?;
    let answer = graph(&client, &seed, ":calls/:calls/:calls").await?;
    assert_eq!(projected_names(&answer)?, ["_inner"]);
    let row = &query::rows(&answer)?[0];
    let identity = query::iri(row, "symbol")?;
    let parsed = rift_protocol::identity::SymbolIdentity::parse(identity)?;
    assert!(
        matches!(parsed.owner(), rift_protocol::identity::SymbolOwner::Package { manager, registry, name, version } if manager == "pypi" && registry == "pypi.org" && name == "greeting" && version == "1.0.0")
    );
    let view = answer["view"]["id"]
        .as_str()
        .ok_or("retained dependency execution")?;
    let exact = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({"id":identity,"view":view,"include":["source"]}),
        ),
    )
    .await?;
    assert_eq!(exact["symbol"]["name"], "_inner");
    let unit = exact["declarations"][0]["unit"]
        .as_str()
        .ok_or("retained dependency source unit")?;
    let path = directory
        .path()
        .join(".venv/lib/python3.12/site-packages/greeting/core.py");
    std::fs::write(&path, source.replace("_inner", "_changed"))?;
    let retained = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":unit,"view":view}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&retained)? else {
        return Err(format!("one original source result: {retained}").into());
    };
    assert_eq!(query::literal(source_row, "text")?, source);
    let changed = graph(&client, &seed, ":calls/:calls/:calls").await?;
    assert_eq!(projected_names(&changed)?, ["_changed"]);
    assert!(declaration_requests(&fixture).await.is_empty());
    client.cancel().await?;
    Ok(())
}

/// Admitted installed source supplies both callees, including the declaration the remote
/// fixture does not name. Exact reads retain the original package owner and bytes.
#[cfg(unix)]
#[tokio::test]
async fn search_query_keeps_installed_callees_without_remote_declaration_names() -> TestResult {
    let fixture = python_global_api().await?;
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n{OUTGOING_ENGINE}",
        global_table(&fixture.endpoint)
    );
    let (_directory, client, _server_task) =
        served_workspace(INSTALLED_FILES, Some(configuration.clone())).await?;
    let request = path_arguments(
        &captured_seed(&client, "hello", "app.py").await?,
        "outgoing",
    )?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(projected_names(&answer)?, ["greet", "quiet"]);
    for name in ["greet", "quiet"] {
        let exact = read_dependency_hit(&client, &answer, name).await?;
        assert_eq!(exact["symbol"]["name"], name);
        assert_eq!(exact["symbol"]["origin"]["package"]["version"], "1.0.0");
    }
    assert!(declaration_requests(&fixture).await.is_empty());
    client.cancel().await?;

    let files = INSTALLED_FILES.iter().map(|(path, source)| (*path, if *path == "app.py" {
        "from greeting.core import greet\n\ndef hello() -> str:\n    return greet('rift')\n"
    } else { *source })).collect::<Vec<_>>();
    let (_directory, client, _server_task) = served_workspace(&files, Some(configuration)).await?;
    let seed = captured_seed(&client, "hello", "app.py").await?;
    let answer = graph(&client, &seed, ":calls").await?;
    assert_eq!(projected_names(&answer)?, ["greet"]);
    let row = callee_hit(&answer, "greet")?;
    assert_eq!(
        query::iri(&row, "symbol")?,
        global_api::named_identity(&global_api::NAMED_POSITIONS[1])
    );
    let exact = read_dependency_hit(&client, &answer, "greet").await?;
    assert_eq!(exact["symbol"]["kind"], "function");
    assert_eq!(exact["symbol"]["origin"]["location"], "dependency");
    assert_eq!(exact["symbol"]["origin"]["package"]["version"], "1.0.0");
    held_dependency_graph(&client, &answer).await?;
    client.cancel().await?;
    Ok(())
}

/// A stale engine URI cannot supply a declaration when its installed source is absent
/// before the first acquisition. A remote name does not substitute for missing source.
#[cfg(unix)]
#[tokio::test]
async fn search_query_refuses_an_installed_callee_without_admitted_source() -> TestResult {
    let fixture = python_global_api().await?;
    let (engine, configuration) = utf16_calls_engine(&fixture.endpoint)?;
    let files = INSTALLED_FILES
        .iter()
        .copied()
        .filter(|(path, _)| *path != ".venv/lib/python3.12/site-packages/greeting/core.py")
        .collect::<Vec<_>>();
    let (directory, client, _server_task) = served_workspace(&files, Some(configuration)).await?;
    let missing = ".venv/lib/python3.12/site-packages/greeting/core.py";
    assert!(!directory.path().join(missing).exists());
    let request = path_arguments(
        &captured_seed(&client, "hello", "app.py").await?,
        "outgoing",
    )?;
    required_facts_unavailable(&client, &request, ErrorCode::TemporarilyUnavailable).await?;
    let log = std::fs::read_to_string(engine.path().join("engine.sh.log"))?;
    let requests = log
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        requests
            .iter()
            .any(|request| request["method"] == "callHierarchy/outgoingCalls"),
        "the supplied target must reach source admission"
    );
    let supplied = std::fs::read_to_string(engine.path().join("engine.sh.targets"))?;
    let caller = requests
        .iter()
        .find(|request| request["method"] == "callHierarchy/outgoingCalls")
        .and_then(|request| request["params"]["item"]["uri"].as_str())
        .and_then(|uri| uri.strip_suffix("app.py"))
        .ok_or("original caller source URI")?;
    assert!(!supplied.is_empty());
    assert!(
        supplied
            .lines()
            .all(|uri| uri.strip_prefix(caller) == Some(missing))
    );
    assert!(declaration_requests(&fixture).await.is_empty());
    client.cancel().await?;
    Ok(())
}

/// `greeting` 1.0.0 installed into `.venv` after the first walk, as `uv sync` installs
/// it from the unchanged `uv.lock`: the files `INSTALLED_FILES` holds below
/// `site-packages`.
#[cfg(unix)]
fn installed_after(path: &str) -> bool {
    path.starts_with(".venv/lib/python3.12/site-packages/greeting")
}

/// A distribution `uv sync` installs into the existing `.venv` without touching `uv.lock`
/// reaches the next walk: the engine resolves the import, and the dependency context
/// locates the package it lands in, so the callee is named rather than dropped.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_names_a_callee_installed_after_the_first_walk() -> TestResult {
    let fixture = python_global_api().await?;
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n{OUTGOING_ENGINE}",
        global_table(&fixture.endpoint)
    );
    let mut before: Vec<(&str, &str)> = INSTALLED_FILES
        .iter()
        .map(|(path, source)| (*path, if *path == "app.py" { "from greeting.core import greet\n\ndef hello() -> str:\n    return greet('rift')\n" } else { *source }))
        .filter(|(path, _)| !installed_after(path))
        .collect();
    before.push((".venv/lib/python3.12/site-packages/_virtualenv.py", ""));
    let (directory, client, _server_task) = served_workspace(&before, Some(configuration)).await?;
    let walk = path_arguments(
        &captured_seed(&client, "hello", "app.py").await?,
        "outgoing",
    )?;

    let first = call_retrying_acceptance(&client, tool_request("search", &walk)).await?;
    assert!(
        symbol_names(&first).is_empty(),
        "nothing installed resolves: {first}"
    );

    for (path, content) in INSTALLED_FILES
        .iter()
        .filter(|(path, _)| installed_after(path))
    {
        let file = directory.path().join(path);
        std::fs::create_dir_all(file.parent().ok_or("an installed file has a folder")?)?;
        std::fs::write(file, content)?;
    }
    let second = call_retrying_acceptance(&client, tool_request("search", &walk)).await?;
    assert_eq!(symbol_names(&second), ["greet"], "{second}");
    let greet = callee_hit(&second, "greet")?;
    assert_eq!(
        query::iri(&greet, "symbol")?,
        global_api::named_identity(&global_api::NAMED_POSITIONS[1])
    );
    let exact = read_dependency_hit(&client, &second, "greet").await?;
    assert_eq!(exact["symbol"]["origin"]["location"], "dependency");

    client.cancel().await?;
    Ok(())
}

/// An unavailable global API does not discard an independently admitted physical runtime source.
#[tokio::test]
async fn search_query_uses_admitted_runtime_when_global_api_is_unavailable() -> TestResult {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let configuration = format!(
        "{}\n{OUTGOING_ENGINE}",
        global_table(&format!("http://127.0.0.1:{port}/rift/rest"))
    );
    let (_directory, client, _server_task) = workspace_client::served_prepared_workspace(
        OUTGOING_FILES,
        Some(configuration),
        python_runtime_environment::prepare,
    )
    .await?;

    let request = path_arguments(
        &captured_seed(&client, "counted", "extra.py").await?,
        "outgoing",
    )?;
    let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(projected_names(&answer)?, ["leaf", "len"]);
    let row = callee_hit(&answer, "len")?;
    let id = global_api::named_identity(&global_api::NAMED_POSITIONS[0]);
    assert_eq!(query::iri(&row, "symbol")?, id);
    let exact = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({"id":id,"view":answer["view"]["id"],"include":["source"]}),
        ),
    )
    .await?;
    assert_eq!(exact["outcome"], "found");
    assert_eq!(exact["symbol"]["id"], id);
    assert_eq!(exact["declarations"].as_array().map(Vec::len), Some(1));
    assert_eq!(exact["declarations"][0]["source_complete"], true);
    assert!(exact["declarations"][0]["path"].is_null());
    assert_eq!(exact["symbol"]["name"], "len");
    assert_eq!(
        exact["symbol"]["origin"],
        json!({
            "location":"stdlib","source_kind":"authored",
            "runtime":{"runtime":"cpython","version":global_api::python_runtime::VERSION}
        })
    );
    let declaration = &exact["declarations"][0];
    assert_eq!(
        declaration["unit"],
        global_api::named_unit(&global_api::NAMED_POSITIONS[0])
    );
    let start = usize::try_from(
        declaration["range"]["start"]
            .as_u64()
            .ok_or("runtime range start")?,
    )?;
    let end = usize::try_from(
        declaration["range"]["end"]
            .as_u64()
            .ok_or("runtime range end")?,
    )?;
    let original = global_api::python_runtime::builtins();
    assert!(
        declaration["source"]
            .as_str()
            .is_some_and(|source| source.contains("def len("))
    );
    assert_eq!(
        declaration["source"].as_str(),
        Some(
            original
                .get(start..end)
                .ok_or("original runtime declaration range")?
        )
    );
    assert_eq!(exact["view"], answer["view"]);
    assert_admitted_runtime_source(&client, &answer, declaration, &original).await?;

    client.cancel().await?;
    Ok(())
}

async fn assert_admitted_runtime_source(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    answer: &Value,
    declaration: &Value,
    original: &str,
) -> TestResult {
    let source = call_retrying_acceptance(
        client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&serde_json::from_value(
                json!({"unit":declaration["unit"],"view":answer["view"]["id"]}),
            )?)?)?,
        ),
    )
    .await?;
    let [source_row] = query::rows(&source)? else {
        return Err(format!("one original physical source: {source}").into());
    };
    assert_eq!(query::literal(source_row, "outcome")?, "found");
    assert_eq!(query::literal(source_row, "complete")?, "true");
    assert_eq!(
        query::iri(source_row, "unit")?,
        declaration["unit"].as_str().ok_or("runtime source unit")?
    );
    assert_eq!(query::literal(source_row, "start")?.parse::<u64>()?, 0);
    assert_eq!(
        query::literal(source_row, "end")?.parse::<usize>()?,
        original.len()
    );
    assert_eq!(query::literal(source_row, "text")?, original);
    assert_eq!(
        query::literal(source_row, "source_view")?,
        answer["view"]["id"]
            .as_str()
            .ok_or("physical source view")?
    );
    assert_eq!(
        query::literal(source_row, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("physical source expiry")?
    );
    Ok(())
}

/// Explicitly unsupported global operations permit the independently admitted runtime
/// source and its capable local engine. No declaration request is sent.
#[tokio::test]
async fn search_query_uses_admitted_runtime_when_global_operations_are_unsupported() -> TestResult {
    let fixture = global_api::GlobalFixture::start_with(global_api::FixtureOptions {
        python_collection: true,
        withheld_features: &["declarations", "relationships"],
        ..global_api::FixtureOptions::default()
    })
    .await?;
    let configuration = format!("{}\n{OUTGOING_ENGINE}", global_table(&fixture.endpoint));
    let (_directory, client, _server_task) = workspace_client::served_prepared_workspace(
        OUTGOING_FILES,
        Some(configuration),
        python_runtime_environment::prepare,
    )
    .await?;

    let counted = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &path_arguments(
                &captured_seed(&client, "counted", "extra.py").await?,
                "outgoing",
            )?,
        ),
    )
    .await?;
    assert_eq!(projected_names(&counted)?, ["leaf", "len"]);
    let row = callee_hit(&counted, "len")?;
    let identity = rift_protocol::identity::SymbolIdentity::parse(query::iri(&row, "symbol")?)?;
    assert!(
        matches!(identity.owner(), rift_protocol::identity::SymbolOwner::Runtime { runtime, .. } if runtime == "cpython")
    );
    assert!(declaration_requests(&fixture).await.is_empty());

    client.cancel().await?;
    Ok(())
}

/// A `sh` engine serving Python call hierarchy that negotiates no position encoding, so
/// it counts in UTF-16. It begins and ends its work at start, prepares `hello` in the
/// requested document, and names one callee: `greet`, in the `greeting` package
/// [`INSTALLED_FILES`] installs, at its installed path below the workspace.
#[cfg(unix)]
const UTF16_CALLS_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  printf '%s\n' "$body" >> "$0.log"
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  uri=$(printf '%s' "$body" | grep -o '"uri":"[^"]*"' | head -1 | cut -d'"' -f4)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}' ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"hello\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":3,\"character\":0},\"end\":{\"line\":5,\"character\":24}},\"selectionRange\":{\"start\":{\"line\":3,\"character\":4},\"end\":{\"line\":3,\"character\":9}}}]}" ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      core="${uri%/app.py}/.venv/lib/python3.12/site-packages/greeting/core.py"
      printf '%s\n' "$core" >> "$0.targets"
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"greet\",\"kind\":12,\"uri\":\"$core\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":1,\"character\":15}},\"selectionRange\":{\"start\":{\"line\":0,\"character\":4},\"end\":{\"line\":0,\"character\":9}}},\"fromRanges\":[]}]}" ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

#[cfg(unix)]
fn utf16_calls_engine(endpoint: &str) -> TestResult<(tempfile::TempDir, String)> {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    std::fs::write(&script, UTF16_CALLS_ENGINE)?;
    let command = serde_json::to_string(&script)?;
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n\
         [languages.python.lsp]\ncommand = [\"sh\", {command}]\nsettle_delay = \"10ms\"\n",
        global_table(endpoint),
    );
    Ok((engine, configuration))
}

/// An engine omitting positionEncoding uses UTF-16. Its original prepared position,
/// dependency declaration, source range and held bytes survive local acquisition.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_asks_a_utf16_engines_dependency_callee_in_utf16() -> TestResult {
    let fixture = python_global_api().await?;
    let (engine, configuration) = utf16_calls_engine(&fixture.endpoint)?;
    let script = engine.path().join("engine.sh");
    let (_directory, client, _server_task) =
        served_workspace(INSTALLED_FILES, Some(configuration)).await?;

    let hello = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &path_arguments(
                &captured_seed(&client, "hello", "app.py").await?,
                "outgoing",
            )?,
        ),
    )
    .await?;
    assert_eq!(symbol_names(&hello), ["greet"], "{hello}");
    let greet = callee_hit(&hello, "greet")?;
    assert_eq!(
        query::iri(&greet, "symbol")?,
        global_api::named_identity(&global_api::NAMED_POSITIONS[1]),
        "{greet}"
    );
    assert!(declaration_requests(&fixture).await.is_empty());
    let exact = read_dependency_hit(&client, &hello, "greet").await?;
    let declaration = &exact["declarations"][0];
    let start = usize::try_from(
        declaration["range"]["start"]
            .as_u64()
            .ok_or("declaration start")?,
    )?;
    let end = usize::try_from(
        declaration["range"]["end"]
            .as_u64()
            .ok_or("declaration end")?,
    )?;
    assert_eq!(
        declaration["source"]
            .as_str()
            .ok_or("dependency declaration source")?,
        &global_api::GREETING_SOURCE[start..end]
    );
    let log = std::fs::read_to_string(format!("{}.log", script.display()))?;
    let requests = log
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    let prepared = requests
        .iter()
        .filter(|request| request["method"] == "textDocument/prepareCallHierarchy")
        .collect::<Vec<_>>();
    assert!(
        !prepared.is_empty(),
        "engine receives the original name position"
    );
    for request in prepared {
        assert_eq!(
            request["params"]["position"],
            json!({"line":3,"character":4})
        );
    }
    let outgoing = requests
        .iter()
        .filter(|request| request["method"] == "callHierarchy/outgoingCalls")
        .collect::<Vec<_>>();
    assert!(
        !outgoing.is_empty(),
        "engine receives its original prepared item"
    );
    for request in outgoing {
        assert_eq!(
            request["params"]["item"]["selectionRange"],
            json!({"start":{"line":3,"character":4},"end":{"line":3,"character":9}})
        );
    }

    client.cancel().await?;
    Ok(())
}

/// Required facts may be refused at provider acquisition or by captured selection coverage.
async fn required_facts_unavailable(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: &Value,
    expected_wire_code: ErrorCode,
) -> TestResult {
    let result = client.call_tool(tool_request("search", arguments)).await?;
    if result.is_error == Some(true) {
        let failure = workspace_client::tool_failure(&result)?;
        assert_eq!(failure.code, expected_wire_code, "{failure:?}");
        return Ok(());
    }
    let body = result
        .structured_content
        .ok_or("typed query unavailability")?;
    let answer: rift_protocol::query::QueryReadResult = serde_json::from_value(body.clone())?;
    let rift_protocol::query::QueryReadResult::Unavailable(
        rift_protocol::search_read::SearchUnavailable::Unavailable { reason, .. },
    ) = answer
    else {
        return Err(format!("required facts must refuse complete query: {body}").into());
    };
    assert_eq!(
        reason,
        rift_protocol::symbol_read::SymbolUnavailableReason::InsufficientCoverage,
        "{body}"
    );
    Ok(())
}

/// The `code` the server refuses `arguments` with.
async fn refusal_code(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: &Value,
) -> TestResult<ErrorCode> {
    Ok(failed_call(client.call_tool(tool_request("search", arguments)).await)?.code)
}

fn symbol_names(structured: &Value) -> Vec<String> {
    projected_names(structured).expect("fixture query projects endpoint names")
}

/// Missing offline Cargo metadata leaves local facts usable and preserves its typed
/// failure when the engine requires a foreign source owner.
#[cfg(unix)]
#[tokio::test]
async fn search_query_keeps_local_rust_facts_when_cargo_metadata_is_unavailable() -> TestResult {
    use std::fmt::Write as _;

    let progress = r#"frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"ready","value":{"kind":"begin","title":"analysis"}}}'
frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"ready","value":{"kind":"end"}}}'"#;
    let script = fake_engine::SCRIPTED_CALLS_ENGINE
        .replace("PROGRESS", progress)
        .replace("CALLEE_LINE", "0")
        .replace("CALLER_LINE", "3");
    let (engine, mut configuration) = fake_engine::rust_engine(&script)?;
    let tools = engine.path().join("bin");
    std::fs::create_dir(&tools)?;
    for (name, target) in [
        ("sh", "/bin/sh"),
        ("dd", "/bin/dd"),
        ("grep", "/usr/bin/grep"),
        ("head", "/usr/bin/head"),
        ("tr", "/usr/bin/tr"),
        ("cut", "/usr/bin/cut"),
        ("cat", "/bin/cat"),
    ] {
        std::os::unix::fs::symlink(target, tools.join(name))?;
    }
    let marker = engine.path().join("caller-uri");
    std::fs::write(&marker, "")?;
    let script = script.replace(
        "caller_uri=${CALLER_URI:-$uri}",
        &format!(
            "caller_uri=$(cat '{}')\n  caller_uri=${{caller_uri:-$uri}}",
            marker.display()
        ),
    );
    std::fs::write(engine.path().join("engine.sh"), script)?;
    write!(
        configuration,
        "[languages.rust.lsp.environment]\nPATH={}\n",
        serde_json::to_string(&tools)?
    )?;
    let lock = "version=4\n[[package]]\nname='helper'\nversion='1.0.0'\nsource='registry+https://github.com/rust-lang/crates.io-index'\n";
    let source = "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n";
    let (_directory, client, _server_task) = served_workspace(
        &[
            ("Cargo.toml", RUST_LIBRARY),
            ("Cargo.lock", lock),
            ("lib.rs", source),
        ],
        Some(configuration),
    )
    .await?;
    let seed = captured_seed(&client, "beacon", "lib.rs").await?;
    let arguments = path_arguments(&seed, "incoming")?;
    let local = call_retrying_acceptance(&client, tool_request("search", &arguments)).await?;
    assert_eq!(projected_names(&local)?, ["caller"]);
    std::fs::write(&marker, "file:///unadmitted/lib.rs")?;
    let failure = failed_call(client.call_tool(tool_request("search", &arguments)).await)?;
    assert_eq!(failure.code, ErrorCode::TemporarilyUnavailable);
    assert!(
        failure.text.contains("Cargo metadata failed"),
        "{failure:?}"
    );
    assert!(
        failure.text.contains("dependency query source"),
        "{failure:?}"
    );
    client.cancel().await?;
    Ok(())
}
