//! Query forms and retained results through the advertised MCP contract.

#[cfg(unix)]
#[expect(
    dead_code,
    reason = "shared engine fixtures serve several integration suites"
)]
mod fake_engine;
mod hermetic_search;
use workspace_client::query;
#[expect(
    dead_code,
    reason = "shared workspace helpers serve several integration suites"
)]
mod workspace_client;

use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

const FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[package]\nname='query_fixture'\nversion='0.1.0'\nedition='2024'\n",
    ),
    ("src/lib.rs", "pub fn beacon() -> bool { true }\n"),
];

#[tokio::test]
async fn query_worker_stdout_exceeding_acknowledgment_frame_requires_operator_action() -> TestResult
{
    use rmcp::ServiceExt as _;

    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    std::fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let server = rift_mcp::RiftMcp::build(
        directory.path(),
        rift_index::WorkspaceIndexLimits::default(),
    )
    .await?
    .with_query_program(std::env::current_exe()?);
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server.serve(server_transport).await?;
        service.waiting().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let client = ().serve(client_transport).await?;
    let result = client
        .call_tool(tool_request("search", &json!({"query":"ASK {}"})))
        .await?;
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert!(result.structured_content.is_none(), "{result:?}");
    let text = result
        .content
        .first()
        .and_then(|content| content.as_text())
        .ok_or("worker refusal carries text")?;
    assert!(
        text.text
            .contains("capability_unavailable · retry operator_action"),
        "{}",
        text.text
    );
    assert!(
        text.text
            .contains("query protocol acknowledgment exceeds its frame bound"),
        "{}",
        text.text
    );
    assert!(
        text.text.contains("rift.server.read_unsupported"),
        "{}",
        text.text
    );
    client.cancel().await?;
    server_task.await?.map_err(std::io::Error::other)?;
    Ok(())
}

#[tokio::test]
async fn authoritative_query_facts_refuse_supplied_records_and_unknown_views() -> TestResult {
    use rift_search::query::FactRequirement;
    use sha2::Digest as _;

    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    let identity = "rift://symbol/local/rust/query_fixture/beacon";
    let original = call_retrying_acceptance(
        &client,
        tool_request("get_symbol", &json!({"id":identity,"include":[]})),
    )
    .await?;
    assert_eq!(original["outcome"], "found");
    let program = std::path::PathBuf::from(
        std::env::var_os("CARGO_BIN_EXE_rift").ok_or("matching query executable")?,
    );
    let limits = rift_search::query::QueryLimits::try_from(
        &rift_protocol::query::QueryConfiguration::default(),
    )?;
    let prepared = tokio::task::spawn_blocking(move || {
        rift_server::prepare_query(
            &program,
            "SELECT ?symbol ?unit ?text { ?symbol :name 'beacon'; :declarations ?declaration. ?declaration :unit ?unit. ?unit :source ?text }".to_owned(),
            limits,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            &|| false,
        )
    })
    .await??;
    let requirements = prepared.requirements();
    requirements.validate_selection()?;
    assert!(requirements.requires(FactRequirement::Domain));
    assert!(requirements.requires(FactRequirement::Source));
    let request = serde_json::to_value(rift_server::QueryExchange::new(json!({
        "requirements":requirements,"view":null,"requests":1000
    })))?;
    let selected = call_retrying_acceptance(&client, tool_request("query_facts", &request)).await?;
    assert_eq!(selected["payload"]["outcome"], "facts", "{selected}");
    let bodies = selected["payload"]["response"]
        .as_array()
        .ok_or("owned fact bodies")?;
    assert_eq!(bodies.len(), 1, "{selected}");
    let selected = &bodies[0];
    assert_eq!(selected["graph"], rift_search::sparql::LOCAL_GRAPH);
    assert_eq!(selected["requirements"], request["payload"]["requirements"]);
    let facts: rift_search::query::QueryFacts = serde_json::from_value(selected["facts"].clone())?;
    assert!(
        facts
            .symbols
            .iter()
            .any(|item| item.symbol.name == "beacon")
    );
    let digest = data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(FILES[1].1.as_bytes()));
    assert!(facts.sources.iter().any(|source| {
        source.unit.as_str() == "rift://source/project/src/lib.rs"
            && source.text.as_deref() == Some(FILES[1].1)
            && source
                .digest
                .as_ref()
                .is_some_and(|value| value.as_str() == digest)
    }));
    let held = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({
                "id":identity,"view":selected["view"]["id"],"include":[]
            }),
        ),
    )
    .await?;
    assert_eq!(held["symbol"], original["symbol"]);
    assert_eq!(held["view"], selected["view"]);
    assert_query_fact_refusals(&client, request, &selected["view"]["id"]).await?;
    let held_again = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({
                "id":identity,"view":selected["view"]["id"],"include":[]
            }),
        ),
    )
    .await?;
    assert_eq!(held_again["symbol"], held["symbol"]);
    assert_eq!(held_again["view"], held["view"]);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn assert_query_fact_refusals(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    request: Value,
    view: &Value,
) -> TestResult {
    use rift_search::query::{FactRequirement, FactRequirements};

    for requirement in [
        FactRequirement::Source,
        FactRequirement::Nodes,
        FactRequirement::History,
    ] {
        let mut malformed = FactRequirements::default();
        malformed.require(requirement);
        let malformed = serde_json::to_value(rift_server::QueryExchange::new(json!({
            "requirements":malformed,"view":null,"requests":1000
        })))?;
        let refused = client
            .call_tool(tool_request("query_facts", &malformed))
            .await?;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        assert!(refused.structured_content.is_none(), "{refused:?}");
        assert!(
            serde_json::to_string(&refused)?
                .contains("required fact families must include the domain selection")
        );
    }
    let mut supplied = request.clone();
    supplied["payload"]["facts"] = json!({"symbols":[{"name":"changed"}]});
    supplied["payload"]["view"] = view.clone();
    let invalid = client
        .call_tool(tool_request("query_facts", &supplied))
        .await?;
    assert_eq!(invalid.is_error, Some(true), "{invalid:?}");
    assert!(serde_json::to_string(&invalid)?.contains("invalid_request"));
    for version in [0, 8, 9] {
        let mut incompatible = request.clone();
        incompatible["version"] = json!(version);
        let incompatible = client
            .call_tool(tool_request("query_facts", &incompatible))
            .await?;
        assert_eq!(incompatible.is_error, Some(true), "{incompatible:?}");
        let incompatible = serde_json::to_string(&incompatible)?;
        assert!(incompatible.contains("capability_unavailable · retry operator_action"));
        assert!(incompatible.contains("rift.server.read_unsupported"));
    }
    let mut forged = request;
    forged["payload"]["view"] = json!("0".repeat(64));
    let expired = client
        .call_tool(tool_request("query_facts", &forged))
        .await?;
    assert_eq!(expired.is_error, Some(true), "{expired:?}");
    assert!(serde_json::to_string(&expired)?.contains("view_expired"));
    Ok(())
}

#[tokio::test]
async fn captured_view_does_not_authorize_supplied_query_records() -> TestResult {
    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    let identity = "rift://symbol/local/rust/query_fixture/beacon";
    let original = call_retrying_acceptance(
        &client,
        tool_request("get_symbol", &json!({"id":identity,"include":[]})),
    )
    .await?;
    assert_eq!(original["outcome"], "found");
    let mut supplied_symbol = original["symbol"].clone();
    supplied_symbol["name"] = json!("changed");
    let producer: rift_protocol::read::SearchParams = serde_json::from_value(json!({
        "query":"beacon","include":[]
    }))?;
    let result: rift_protocol::read::SearchResult = serde_json::from_value(json!({
        "view":original["view"],
        "results":[{"hit":{"target":"symbol","symbol":supplied_symbol},"view":original["view"]}],
        "pagination":{"page_index":0,"total_pages":1}
    }))?;
    let supplied = rift_search::sparql::QueryServiceData {
        request: producer.clone(),
        targets: vec![None; result.results.len()],
        result,
    };
    let rejected = client
        .call_tool(tool_request(
            "query_candidates",
            &json!({
                "params":query::retrieval(&producer)?,"retrievals":[supplied]
            }),
        ))
        .await?;
    assert_eq!(rejected.is_error, Some(true));
    assert!(
        serde_json::to_string(&rejected)?
            .contains("query evaluation does not accept supplied fact records")
    );
    assert!(
        rejected
            .structured_content
            .as_ref()
            .is_none_or(|value| value.get("view").is_none())
    );
    let held = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({"id":identity,"view":original["view"]["id"],"include":[]}),
        ),
    )
    .await?;
    assert_eq!(held["symbol"], original["symbol"]);
    assert_eq!(held["view"], original["view"]);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn query_children_preserve_source_and_metadata_responses_under_concurrent_queries()
-> TestResult {
    use std::time::{Duration, Instant};

    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    let ready = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query":"ASK { ?symbol :name 'beacon' }"})),
    )
    .await?;
    assert_eq!(ready["answer"]["value"], true);
    let statement = json!({"query":"SELECT ?number { VALUES ?number { 1 2 3 } }"});
    let source = serde_json::to_value(query::source(&serde_json::from_value(json!({
        "unit":"rift://source/project/src/lib.rs"
    }))?)?)?;
    let started = Instant::now();
    let work = async {
        tokio::join!(
            call_retrying_acceptance(&client, tool_request("search", &statement)),
            call_retrying_acceptance(&client, tool_request("search", &statement)),
            async {
                let started = Instant::now();
                let result =
                    call_retrying_acceptance(&client, tool_request("search", &source)).await;
                (result, started.elapsed())
            },
            client.list_all_tools(),
        )
    };
    let (left, right, (source, source_elapsed), tools) =
        tokio::time::timeout(Duration::from_secs(30), work).await?;
    assert_eq!(query::rows(&left?)?.len(), 3);
    assert_eq!(query::rows(&right?)?.len(), 3);
    let source = source?;
    let [source_row] = query::rows(&source)? else {
        return Err(format!("source query must return one original result: {source}").into());
    };
    assert_eq!(query::literal(source_row, "outcome")?, "found");
    assert!(query::literal(source_row, "text")?.contains("pub fn beacon"));
    assert_eq!(query::literal(source_row, "complete")?, "true");
    assert!(tools?.iter().any(|tool| tool.name == "search"));
    let followup =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":"ASK {}"})))
            .await?;
    assert_eq!(followup["answer"]["value"], true);
    eprintln!(
        "concurrent query elapsed_ms={} source elapsed_ms={}",
        started.elapsed().as_millis(),
        source_elapsed.as_millis(),
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn query_forms_validate_against_live_schemas_and_preserve_exact_identity() -> TestResult {
    let (_engine, configuration) = fake_engine::rust_engine(DESCRIBE_ENGINE)?;
    let (_directory, client, server_task) = served_workspace(FILES, Some(configuration)).await?;
    let tool = client
        .list_all_tools()
        .await?
        .into_iter()
        .find(|tool| tool.name == "search")
        .ok_or("search must be advertised")?;
    let input = Value::Object(tool.input_schema.as_ref().clone());
    let output = Value::Object(
        tool.output_schema
            .as_deref()
            .ok_or("search output schema")?
            .clone(),
    );
    let input = jsonschema::validator_for(&input)?;
    let output = jsonschema::validator_for(&output)?;
    let mut forms = std::collections::BTreeSet::new();
    for statement in [
        "SELECT ?symbol { ?symbol :name 'beacon' }",
        "ASK { ?symbol :name 'beacon' }",
        "CONSTRUCT { ?symbol :name ?name } WHERE { ?symbol :name ?name FILTER(?name = 'beacon') }",
        "DESCRIBE ?symbol WHERE { ?symbol :name 'beacon' }",
    ] {
        let request = json!({"query":statement});
        assert!(input.is_valid(&request), "request must validate: {request}");
        let answer = call_retrying_acceptance(&client, tool_request("search", &request))
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
        assert!(output.is_valid(&answer), "answer must validate: {answer}");
        forms.insert(
            answer["answer"]["result"]
                .as_str()
                .ok_or("answer result form")?
                .to_owned(),
        );
        if statement.starts_with("SELECT") {
            let identity = query::iri(&query::rows(&answer)?[0], "symbol")?;
            let request = json!({"id":identity,"view":answer["view"]["id"],"include":[]});
            let exact =
                call_retrying_acceptance(&client, tool_request("get_symbol", &request)).await?;
            assert_eq!(exact["symbol"]["id"], identity);
            let description = call_retrying_acceptance(&client,tool_request("search",&json!({
                "query":format!("DESCRIBE <{identity}> WHERE {{ <{identity}> :calls* ?target }}")
            }))).await?;
            assert_eq!(description["answer"]["result"], "graph");
            assert!(
                !description["answer"]["triples"]
                    .as_array()
                    .ok_or("complete engine description")?
                    .is_empty()
            );
        }
    }
    assert_eq!(
        forms,
        std::collections::BTreeSet::from([
            "bindings".to_owned(),
            "boolean".to_owned(),
            "graph".to_owned()
        ])
    );
    for invalid in [
        json!({"query":"ASK {}","scope":"local"}),
        json!({"query":"ASK {}","traversal":{}}),
        json!({"query":"ASK {}","language":"cypher"}),
    ] {
        assert!(
            !input.is_valid(&invalid),
            "unsupported request must fail served schema: {invalid}"
        );
        let response = client.call_tool(tool_request("search", &invalid)).await;
        assert!(
            response.is_err() || response.is_ok_and(|response| response.is_error == Some(true))
        );
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[cfg(unix)]
const DESCRIBE_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  uri=$(printf '%s' "$body" | grep -o '"uri":"[^"]*"' | head -1 | cut -d'"' -f4)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fixture","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fixture","value":{"kind":"end"}}}' ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"beacon\",\"kind\":12,\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":0},\"end\":{\"line\":0,\"character\":32}},\"selectionRange\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}}]}" ;;
    *'"method":"textDocument/references"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"uri\":\"$uri\",\"range\":{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}}]}" ;;
    *'"method":"callHierarchy/outgoingCalls"'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
    *'"method":"exit"'*) exit 0 ;;
    *'"id":'[0-9]*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

#[tokio::test]
async fn describe_uses_captured_facts_and_requires_complete_semantic_where() -> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let discovered = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "query":"SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }"
            }),
        ),
    )
    .await?;
    let identity = query::iri(&query::rows(&discovered)?[0], "symbol")?.to_owned();
    let mut selected = None;
    for statement in [
        "DESCRIBE ?symbol WHERE { ?symbol :name 'beacon' }".to_owned(),
        format!("DESCRIBE <{identity}>"),
    ] {
        let answer =
            call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
                .await?;
        assert_eq!(answer["answer"]["result"], "graph", "{answer}");
        assert!(
            !answer["answer"]["triples"]
                .as_array()
                .ok_or("description triples")?
                .is_empty()
        );
        selected = Some(answer);
    }
    let request = tool_request(
        "search",
        &json!({"query": format!("DESCRIBE <{identity}> WHERE {{ <{identity}> :calls* ?target }}")}),
    );
    let result = client.call_tool(request).await;
    assert!(result.is_err() || result.is_ok_and(|result| result.is_error == Some(true)));
    let selected = selected.ok_or("captured description")?;
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )?;
    let view = selected["view"]["id"]
        .as_str()
        .ok_or("captured description view")?;
    let held = call_retrying_acceptance(&client,tool_request("search",&json!({
        "query":format!("CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <urn:rift:result:{view}> {{ ?s ?p ?o }} }}")
    }))).await?;
    assert_eq!(held["answer"], selected["answer"]);
    assert_eq!(held["view"]["expires_at"], selected["view"]["expires_at"]);
    let exact = call_retrying_acceptance(
        &client,
        tool_request(
            "get_symbol",
            &json!({
                "id":identity,"view":view,"include":["source"]
            }),
        ),
    )
    .await?;
    assert_eq!(exact["symbol"]["name"], "beacon");
    assert!(
        exact["declarations"][0]["source"]
            .as_str()
            .is_some_and(|source| source.contains("beacon"))
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retained_query_keeps_original_rows_and_generated_blank_nodes_after_publication()
-> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let statement =
        "SELECT ?symbol ?name (BNODE() AS ?blank) { ?symbol :name ?name; :kind 'function' }";
    let first =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let original = query::rows(&first)?.to_vec();
    assert_eq!(original.len(), 1);
    assert_eq!(query::literal(&original[0], "name")?, "beacon");
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )?;
    let latest =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(
        query::literal(&query::rows(&latest)?[0], "name")?,
        "changed"
    );
    let view = first["view"]["id"]
        .as_str()
        .ok_or("retained execution id")?;
    let continuation = format!(
        "SELECT ?symbol ?name ?blank {{ SERVICE <{}{view}> {{}} }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX
    );
    let retained = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query":continuation})),
    )
    .await?;
    assert_eq!(query::rows(&retained)?, original);
    assert_eq!(retained["view"]["expires_at"], first["view"]["expires_at"]);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn unknown_datasets_refuse_before_negative_evaluation() -> TestResult {
    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    for statement in [
        "ASK FROM <urn:unaccepted:graph> { ?subject ?predicate ?object }",
        "ASK { GRAPH <urn:unaccepted:graph> { ?subject ?predicate ?object } }",
    ] {
        let request = json!({"query": statement});
        let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
        assert_eq!(answer["outcome"], "unavailable");
        assert_eq!(answer["reason"], "view_context");
        assert!(answer.get("answer").is_none());
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retained_service_joins_original_source_facts_after_publication() -> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let statement = "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }";
    let request = json!({"query": statement});
    let first = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    let identity = query::iri(&query::rows(&first)?[0], "symbol")?.to_owned();
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )?;
    let latest_request = json!({"query": "ASK { ?symbol a :Symbol; :name 'changed' }"});
    let latest = call_retrying_acceptance(&client, tool_request("search", &latest_request)).await?;
    assert_eq!(latest["answer"]["value"], true);
    let view = first["view"]["id"]
        .as_str()
        .ok_or("retained execution id")?;
    let statement = format!(
        "SELECT ?symbol ?name {{ SERVICE <{}{view}> {{}} ?symbol :name ?name }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX
    );
    let request = json!({"query": statement});
    let retained = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    let rows = query::rows(&retained)?;
    assert_eq!(rows.len(), 1);
    assert_eq!(query::iri(&rows[0], "symbol")?, identity);
    assert_eq!(query::literal(&rows[0], "name")?, "beacon");
    assert_eq!(retained["view"]["expires_at"], first["view"]["expires_at"]);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn incompatible_retained_publications_refuse_before_join() -> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let request = json!({"query": "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }"});
    let first = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )?;
    let request = json!({"query": "SELECT ?symbol { ?symbol a :Symbol; :name 'changed' }"});
    let second = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    let left = first["view"]["id"].as_str().ok_or("first execution id")?;
    let right = second["view"]["id"].as_str().ok_or("second execution id")?;
    let prefix = rift_search::sparql::RESULT_SERVICE_PREFIX;
    let statement = format!(
        "SELECT ?symbol {{ SERVICE <{prefix}{left}> {{}} SERVICE <{prefix}{right}> {{}} }}"
    );
    let request = json!({"query": statement});
    let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(answer["outcome"], "unavailable");
    assert_eq!(answer["reason"], "view_context");
    assert!(answer.get("answer").is_none());
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn incomplete_retrieval_cannot_answer_ask_or_count() -> TestResult {
    let files = [
        ("Cargo.toml", FILES[0].1),
        (
            "src/lib.rs",
            "pub fn beacon() {}\npub fn beacon_helper() {}\n",
        ),
    ];
    let configuration = Some("[search]\nresults=1\n".to_owned());
    let (_directory, client, server_task) = served_workspace(&files, configuration).await?;
    let ready = json!({"query": "ASK { ?symbol a :Symbol; :name 'beacon' }"});
    let ready = call_retrying_acceptance(&client, tool_request("search", &ready)).await?;
    assert_eq!(ready["answer"]["value"], true);
    for statement in [
        "ASK { SERVICE :search { ?hit :query 'beacon'; :symbol ?symbol } FILTER(false) }",
        "SELECT (COUNT(*) AS ?count) { SERVICE :search { ?hit :query 'beacon'; :symbol ?symbol } }",
    ] {
        let request = json!({"query": statement});
        let answer = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
        assert_eq!(answer["outcome"], "unavailable");
        assert!(answer.get("answer").is_none());
        let warnings = answer["warnings"]
            .as_array()
            .ok_or("required acquisition warnings")?;
        assert!(
            warnings
                .iter()
                .any(|warning| warning["code"] == "results_truncated")
        );
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retained_pages_cover_ten_thousand_and_one_rows_and_count_whole_execution() -> TestResult {
    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    let values = (0..10_001)
        .map(|number| number.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let statement = format!("SELECT ?number {{ VALUES ?number {{ {values} }} }} ORDER BY ?number");
    let request = json!({"query": statement});
    let first = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(query::rows(&first)?.len(), 1_000);
    assert_eq!(first["pagination"]["total_pages"], 11);
    let mut current = first.clone();
    let mut numbers = Vec::new();
    for page in 0..11 {
        assert_eq!(current["pagination"]["page_index"], page);
        for row in query::rows(&current)? {
            numbers.push(query::literal(row, "number")?.parse::<u64>()?);
        }
        if let Some(statement) = current["pagination"]["next_query"].as_str() {
            let request = json!({"query": statement});
            current = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
        } else {
            assert_eq!(page, 10);
            break;
        }
    }
    assert_eq!(numbers, (0..10_001).collect::<Vec<_>>());
    let view = first["view"]["id"]
        .as_str()
        .ok_or("retained execution id")?;
    let statement = format!(
        "SELECT (COUNT(*) AS ?count) {{ SERVICE <{}{view}> {{}} }}",
        rift_search::sparql::RESULT_SERVICE_PREFIX
    );
    let request = json!({"query": statement});
    let count = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    assert_eq!(query::literal(&query::rows(&count)?[0], "count")?, "10001");
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn byte_bounded_pages_preserve_every_row_and_refuse_one_large_row() -> TestResult {
    let configuration = Some("[search.query]\npage_limit='1kb'\n".to_owned());
    let (_directory, client, server_task) = served_workspace(FILES, configuration).await?;
    let values = (0..20)
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let text = "x".repeat(100);
    let statement = format!(
        "SELECT ?number ?text {{ VALUES ?number {{ {values} }} BIND('{text}' AS ?text) }} ORDER BY ?number"
    );
    let mut current = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query": statement})),
    )
    .await?;
    assert!(query::rows(&current)?.len() < 20);
    let mut numbers = Vec::new();
    for _ in 0..20 {
        assert!(serde_json::to_vec(&current)?.len() <= 1_024);
        for row in query::rows(&current)? {
            numbers.push(query::literal(row, "number")?.parse::<u64>()?);
        }
        let Some(statement) = current["pagination"]["next_query"].as_str() else {
            break;
        };
        current = call_retrying_acceptance(
            &client,
            tool_request("search", &json!({"query": statement})),
        )
        .await?;
    }
    assert_eq!(numbers, (0..20).collect::<Vec<_>>());
    let statement = format!("SELECT ('{}' AS ?text) {{}}", "x".repeat(2048));
    let response = client
        .call_tool(tool_request("search", &json!({"query": statement})))
        .await;
    assert!(response.is_err() || response.is_ok_and(|response| response.is_error == Some(true)));
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query": "ASK {}"})))
            .await?;
    assert_eq!(answer["answer"]["value"], true);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn graph_pages_retain_original_blank_nodes_and_view_after_source_edit() -> TestResult {
    let configuration = Some("[search.query]\npage_size=2\n".to_owned());
    let (directory, client, server_task) = served_workspace(FILES, configuration).await?;
    let statement = "CONSTRUCT { ?blank :name ?number } WHERE { VALUES ?number { 1 2 3 4 5 } BIND(BNODE() AS ?blank) }";
    let first = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query": statement})),
    )
    .await?;
    assert_eq!(first["pagination"]["total_pages"], 3);
    std::fs::write(directory.path().join("src/lib.rs"), "pub fn changed() {}\n")?;
    let mut current = first.clone();
    let mut triples = Vec::new();
    for page in 0..3 {
        assert_eq!(current["view"], first["view"]);
        assert_eq!(current["pagination"]["page_index"], page);
        triples.extend(
            current["answer"]["triples"]
                .as_array()
                .ok_or("graph triples")?
                .iter()
                .cloned(),
        );
        if let Some(statement) = current["pagination"]["next_query"].as_str() {
            current = call_retrying_acceptance(
                &client,
                tool_request("search", &json!({"query": statement})),
            )
            .await?;
        }
    }
    assert_eq!(triples.len(), 5);
    let subjects = triples
        .iter()
        .map(|triple| triple["subject"].clone())
        .collect::<Vec<_>>();
    assert!(subjects.iter().all(|term| term["type"] == "blank_node"));
    for (index, subject) in subjects.iter().enumerate() {
        assert!(!subjects[..index].contains(subject));
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retrieval_count_acquires_every_transport_page_before_evaluation() -> TestResult {
    let files = [
        ("Cargo.toml", FILES[0].1),
        (
            "src/lib.rs",
            "pub fn beacon() {}\npub fn beacon_helper() {}\npub fn beacon_tail() {}\n",
        ),
    ];
    let (_directory, client, server_task) =
        served_workspace(&files, Some("[search]\nresults=10\n".to_owned())).await?;
    let ready = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query":"ASK { ?symbol a :Symbol; :name 'beacon' }"}),
        ),
    )
    .await?;
    assert_eq!(ready["answer"]["value"], true);
    let statement = "SELECT (COUNT(*) AS ?count) { SERVICE :search { ?hit :query 'beacon'; :target :symbol; :limit 1; :symbol ?symbol } }";
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(query::literal(&query::rows(&answer)?[0], "count")?, "3");
    let statement = "SELECT ?symbol { SERVICE :search { ?hit :query 'beacon'; :target :symbol; :limit 1; :symbol ?symbol } } LIMIT 1";
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(query::rows(&answer)?.len(), 1);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn direct_query_source_reads_keep_original_utf8_bytes_and_ranges_after_edit() -> TestResult {
    use rift_protocol::read::TextRange;
    use rift_protocol::source_read::GetSourceParams;

    let original = "pub fn beacon() -> &'static str { \"café ☕\" }\n";
    let files = [("Cargo.toml", FILES[0].1), ("src/lib.rs", original)];
    let (directory, client, server_task) = served_workspace(&files, None).await?;
    let request = json!({"query": "SELECT ?symbol ?unit ?full_source { ?symbol :name 'beacon'; :declarations ?declaration . ?declaration :unit ?unit . ?unit :source ?full_source }"});
    let first = call_retrying_acceptance(&client, tool_request("search", &request)).await?;
    let rows = query::rows(&first)?;
    assert_eq!(rows.len(), 1, "{first}");
    assert_eq!(query::literal(&rows[0], "full_source")?, original);
    let params = GetSourceParams {
        unit: serde_json::from_value(json!(query::iri(&rows[0], "unit")?))?,
        range: None,
        view: Some(serde_json::from_value(first["view"]["id"].clone())?),
        rev: None,
    };
    std::fs::write(directory.path().join("src/lib.rs"), "pub fn changed() {}\n")?;
    let latest = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query": "ASK { ?symbol :name 'changed' }"}),
        ),
    )
    .await?;
    assert_eq!(latest["answer"]["value"], true);
    let start = original.find("café ☕").ok_or("original UTF-8 range")?;
    let range = TextRange {
        start: u64::try_from(start)?,
        end: u64::try_from(start + "café ☕".len())?,
    };
    for deleted in [false, true] {
        if deleted {
            std::fs::remove_file(directory.path().join("src/lib.rs"))?;
        }
        for selected in [None, Some(range.clone())] {
            let params = GetSourceParams {
                range: selected,
                ..params.clone()
            };
            let answer = call_retrying_acceptance(
                &client,
                tool_request("search", &serde_json::to_value(query::source(&params)?)?),
            )
            .await?;
            let [source] = query::rows(&answer)? else {
                return Err(format!("original source must remain readable: {answer}").into());
            };
            assert_eq!(query::literal(source, "outcome")?, "found");
            assert_eq!(query::iri(source, "unit")?, params.unit.as_str());
            assert_eq!(
                query::literal(source, "source_view")?,
                first["view"]["id"].as_str().ok_or("original source view")?
            );
            assert_eq!(
                query::literal(source, "source_expiry")?,
                first["view"]["expires_at"]
                    .as_str()
                    .ok_or("original source expiry")?
            );
            let expected = params.range.unwrap_or(TextRange {
                start: 0,
                end: u64::try_from(original.len())?,
            });
            assert_eq!(
                query::literal(source, "start")?.parse::<u64>()?,
                expected.start
            );
            assert_eq!(query::literal(source, "end")?.parse::<u64>()?, expected.end);
            let start = usize::try_from(expected.start)?;
            let end = usize::try_from(expected.end)?;
            assert_eq!(query::literal(source, "text")?, &original[start..end]);
            assert_eq!(query::literal(source, "complete")?, "true");
        }
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn original_overload_files() -> [(&'static str, &'static str); 3] {
    let implementation = "/** Implementation. */\nexport function open(path) {}\n";
    let overloads = "/** First overload. */\nexport declare function open(path: string): void;\n/** Second overload. */\nexport declare function open(path: number): void;\n";
    [
        (
            "package.json",
            r#"{"name":"beacon","version":"1.0.0","main":"index.js","types":"index.d.ts"}"#,
        ),
        ("index.js", implementation),
        ("index.d.ts", overloads),
    ]
}

fn original_overload_excerpt(
    declaration: &rift_protocol::symbol_read::SymbolDeclaration,
    files: &[(&str, &str)],
) -> TestResult<&'static str> {
    let path = declaration
        .path
        .as_ref()
        .ok_or("original declaration path")?;
    let original = files
        .iter()
        .find(|(file, _)| *file == path.0.as_str())
        .map(|(_, source)| *source)
        .ok_or("original implementation or overload path")?;
    let start = usize::try_from(declaration.range.start)?;
    let end = usize::try_from(declaration.range.end)?;
    let range = original
        .get(start..end)
        .ok_or("original declaration range")?;
    assert!(!range.is_empty());
    let mut excerpts = [
        (
            "index.js",
            "/** Implementation. */\nexport function open(path) {}",
        ),
        (
            "index.d.ts",
            "/** First overload. */\nexport declare function open(path: string): void;",
        ),
        (
            "index.d.ts",
            "/** Second overload. */\nexport declare function open(path: number): void;",
        ),
    ]
    .into_iter()
    .filter_map(|(file, excerpt)| {
        let offset = original.find(excerpt)?;
        (file == path.0.as_str() && offset <= start && end <= offset + excerpt.len())
            .then_some(excerpt)
    });
    let excerpt = excerpts.next().ok_or("original declaration excerpt")?;
    assert!(
        excerpts.next().is_none(),
        "one original declaration excerpt"
    );
    Ok(excerpt)
}

async fn original_overload_pages(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    mut request: rift_protocol::symbol_read::GetSymbolParams,
    expected_view: &rift_protocol::symbol_read::CapturedView,
) -> TestResult<Vec<rift_protocol::symbol_read::GetSymbolResult>> {
    use rift_protocol::symbol_read::GetSymbolResult;

    assert_eq!(request.declaration_limit, 1);
    assert!(request.declaration_cursor.is_none());
    let mut original = Vec::new();
    for page in 0..3 {
        let answer = call_retrying_acceptance(
            client,
            tool_request("get_symbol", &serde_json::to_value(&request)?),
        )
        .await?;
        let result: GetSymbolResult = serde_json::from_value(answer.clone())?;
        let GetSymbolResult::Found {
            declarations,
            declaration_cursor,
            view,
            ..
        } = &result
        else {
            return Err(format!("original overloads must remain readable: {answer}").into());
        };
        assert_eq!(view, expected_view);
        assert_eq!(declarations.len(), 1);
        assert_eq!(declaration_cursor.is_some(), page < 2);
        request.declaration_cursor.clone_from(declaration_cursor);
        original.push(result);
    }
    Ok(original)
}

#[tokio::test]
async fn direct_query_symbol_declaration_cursors_keep_original_overloads_after_edit() -> TestResult
{
    use rift_protocol::read::GetSymbolInclude;
    use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};

    let files = original_overload_files();
    let (directory, client, server_task) = served_workspace(&files, None).await?;
    let first = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query": "SELECT ?symbol (COUNT(?declaration) AS ?count) { ?symbol :name 'open'; :declarations ?declaration . ?declaration :source ?source } GROUP BY ?symbol"})),
    )
    .await?;
    let rows = query::rows(&first)?;
    assert_eq!(rows.len(), 1, "{first}");
    assert_eq!(query::literal(&rows[0], "count")?, "3", "{first}");
    let mut request = GetSymbolParams {
        id: serde_json::from_value(json!(query::iri(&rows[0], "symbol")?))?,
        include: vec![GetSymbolInclude::Source],
        rev: None,
        view: Some(serde_json::from_value(first["view"]["id"].clone())?),
        declaration_limit: 1,
        declaration_cursor: None,
    };
    let expected_view =
        serde_json::from_value::<rift_protocol::symbol_read::CapturedView>(first["view"].clone())?;
    let original = original_overload_pages(&client, request.clone(), &expected_view).await?;
    std::fs::write(
        directory.path().join("index.js"),
        "export function replacement(path) {}\n",
    )?;
    std::fs::write(
        directory.path().join("index.d.ts"),
        "export declare function replacement(path: boolean): void;\n",
    )?;
    let latest = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query": "ASK { ?symbol :name 'replacement' }"}),
        ),
    )
    .await?;
    assert_eq!(latest["answer"]["value"], true);
    let mut declarations_seen = std::collections::BTreeSet::new();
    for (page, expected) in original.iter().enumerate() {
        let answer = call_retrying_acceptance(
            &client,
            tool_request("get_symbol", &serde_json::to_value(&request)?),
        )
        .await?;
        let result: GetSymbolResult = serde_json::from_value(answer.clone())?;
        let GetSymbolResult::Found {
            symbol,
            declarations,
            declaration_cursor,
            view,
            ..
        } = result
        else {
            return Err(format!("original overloads must remain readable: {answer}").into());
        };
        assert_eq!(view, expected_view);
        assert_eq!(symbol.id.as_ref(), Some(&request.id));
        let GetSymbolResult::Found {
            symbol: original_symbol,
            declarations: original_declarations,
            ..
        } = expected
        else {
            return Err("original overload symbol must remain readable".into());
        };
        assert_eq!(&symbol, original_symbol);
        assert_eq!(&declarations, original_declarations);
        assert_eq!(symbol.signatures.len(), 3);
        assert_eq!(symbol.documentation.len(), 3);
        assert_eq!(declarations.len(), 1);
        let declaration = &declarations[0];
        assert_eq!(
            declaration.source.as_deref(),
            Some(original_overload_excerpt(declaration, &files[1..])?)
        );
        assert!(declaration.source_complete);
        assert!(declarations_seen.insert((
            declaration.path.clone(),
            declaration.range.start,
            declaration.range.end
        )));
        assert_eq!(declaration_cursor.is_some(), page < 2);
        request.declaration_cursor = declaration_cursor;
    }
    assert_eq!(declarations_seen.len(), 3);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retained_empty_query_counts_only_original_rows_after_edit() -> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let statement = "SELECT ?symbol { ?symbol :name 'changed' }";
    let first =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert!(query::rows(&first)?.is_empty(), "{first}");
    let view: rift_protocol::symbol_read::CapturedView =
        serde_json::from_value(first["view"].clone())?;
    let original_answer: rift_protocol::query::QueryAnswer =
        serde_json::from_value(first["answer"].clone())?;
    std::fs::write(
        directory.path().join("src/lib.rs"),
        "pub fn changed() -> bool { false }\n",
    )?;
    let latest =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(query::rows(&latest)?.len(), 1, "{latest}");
    let service = format!(
        "{}{}",
        rift_search::sparql::RESULT_SERVICE_PREFIX,
        view.id.as_str()
    );
    let count = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query":format!("SELECT (COUNT(*) AS ?count) {{ SERVICE <{service}> {{}} }}")}),
        ),
    )
    .await?;
    let rows = query::rows(&count)?;
    assert_eq!(rows.len(), 1, "{count}");
    assert_eq!(query::literal(&rows[0], "count")?, "0", "{count}");
    assert_eq!(count["view"]["expires_at"], first["view"]["expires_at"]);
    let continuation = rift_search::sparql::continuation_query(&original_answer, &view.id, 0)?;
    let held = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"query":continuation})),
    )
    .await?;
    assert_eq!(held, first, "original empty page and view remain unchanged");
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn retained_anonymous_subject_joins_keep_old_rows_after_edit() -> TestResult {
    let (directory, client, server_task) =
        served_workspace(&[("member.json", "{\"beacon\":\"old\"}\n")], None).await?;
    let mut original = Vec::new();
    for variable in ["symbol", "renamed"] {
        let statement = format!("SELECT ?{variable} {{ ?{variable} a :Symbol; :name 'beacon' }}");
        let first =
            call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
                .await?;
        let rows = query::rows(&first)?;
        assert_eq!(rows.len(), 1, "{first}");
        assert_eq!(rows[0][variable]["type"], "blank_node", "{first}");
        original.push((variable, rows[0][variable].clone(), first));
    }
    std::fs::write(
        directory.path().join("member.json"),
        "{\"replacement\":true}\n",
    )?;
    let current = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query":"ASK { ?symbol :name 'replacement' }"}),
        ),
    )
    .await?;
    assert_eq!(current["answer"]["value"], true);
    for (variable, resource, first) in original {
        let view = first["view"]["id"]
            .as_str()
            .ok_or("original retained view")?;
        let service = format!(
            "SERVICE <{}{view}> {{}}",
            rift_search::sparql::RESULT_SERVICE_PREFIX
        );
        for pattern in [
            format!("{service} ?{variable} :name ?name"),
            format!("?{variable} :name ?name {service}"),
            format!("{service} OPTIONAL {{ ?{variable} :name ?name }}"),
            format!(
                "{service} ?{variable} :name ?name OPTIONAL {{ ?{variable} :missing ?optional }}"
            ),
        ] {
            let statement = format!("SELECT ?{variable} ?name ?optional {{ {pattern} }}");
            let retained = call_retrying_acceptance(
                &client,
                tool_request("search", &json!({"query":statement})),
            )
            .await?;
            let rows = query::rows(&retained)?;
            assert_eq!(rows.len(), 1, "{retained}");
            assert_eq!(rows[0][variable], resource);
            assert_eq!(query::literal(&rows[0], "name")?, "beacon");
            assert!(rows[0].get("optional").is_none());
            assert_eq!(retained["view"]["expires_at"], first["view"]["expires_at"]);
        }
        for pattern in [
            format!("OPTIONAL {{ {service} }} ?{variable} :name ?name"),
            format!("{service} ?other :name ?name"),
            format!("{service} ?{variable} :references ?caller"),
        ] {
            let statement = format!("SELECT ?{variable} ?other ?name ?caller {{ {pattern} }}");
            let unavailable = call_retrying_acceptance(
                &client,
                tool_request("search", &json!({"query":statement})),
            )
            .await?;
            assert_eq!(unavailable["outcome"], "unavailable", "{unavailable}");
            assert_eq!(
                unavailable["reason"], "insufficient_coverage",
                "{unavailable}"
            );
            assert!(unavailable.get("answer").is_none());
        }
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn query_view_nodes_keep_original_source_and_semantic_identity_after_removal() -> TestResult {
    assert_original_query_nodes("SELECT ?symbol { SERVICE :search { ?hit :query 'beacon'; :target :symbol; :limit 1; :symbol ?symbol } ?symbol a :Symbol }", true).await
}

#[tokio::test]
async fn narrow_symbol_query_view_nodes_keep_original_source_after_removal() -> TestResult {
    assert_original_query_nodes(
        "SELECT ?symbol { ?symbol a :Symbol; :name 'beacon' }",
        false,
    )
    .await
}

async fn assert_original_query_nodes(statement: &str, observe_unit: bool) -> TestResult {
    let (directory, client, server_task) = served_workspace(FILES, None).await?;
    let selected =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let (unit, statement, original) =
        original_query_node_rows(&client, &selected, observe_unit).await?;
    let unit = unit.as_str();
    let original_rows = query::rows(&original)?;
    let view = selected["view"]["id"]
        .as_str()
        .ok_or("original query view")?;
    std::fs::write(directory.path().join("src/lib.rs"), "pub fn changed() {}\n")?;
    let changed = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query":"ASK { ?symbol :name 'changed' }"}),
        ),
    )
    .await?;
    assert_eq!(changed["answer"]["value"], true);
    std::fs::remove_file(directory.path().join("src/lib.rs"))?;
    let removed = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"query":"ASK { ?symbol :name 'changed' }"}),
        ),
    )
    .await?;
    assert_eq!(removed["answer"]["value"], false);
    let held =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    assert_eq!(
        query::rows(&held)
            .map_err(|error| format!("held Node read unit={unit} view={view}: {error}"))?,
        original_rows,
        "original node identity, unit, range and bytes survive source and index removal"
    );
    let source = rift_protocol::source_read::GetSourceParams {
        unit: rift_protocol::read::SourceUnitId::parse(unit)?,
        range: None,
        view: Some(rift_protocol::symbol_read::CapturedViewId::parse(view)?),
        rev: None,
    };
    let mut source_query = query::source(&source)?;
    source_query.query = source_query.query.replacen(
        "{ SERVICE :source",
        &format!("{{ SERVICE <urn:rift:result:{view}> {{}} SERVICE :source"),
        1,
    );
    let held_source = call_retrying_acceptance(
        &client,
        tool_request("search", &serde_json::to_value(source_query)?),
    )
    .await?;
    let [source_row] = query::rows(&held_source)
        .map_err(|error| format!("held Source read unit={unit} view={view}: {error}"))?
    else {
        return Err(format!("one captured source: {held_source}").into());
    };
    assert_eq!(query::literal(source_row, "outcome")?, "found");
    assert_eq!(query::iri(source_row, "unit")?, unit);
    assert_eq!(query::literal(source_row, "text")?, FILES[1].1);
    assert_eq!(query::literal(source_row, "complete")?, "true");
    assert_eq!(query::literal(source_row, "source_view")?, view);
    assert_eq!(
        query::literal(source_row, "source_expiry")?,
        selected["view"]["expires_at"]
            .as_str()
            .ok_or("original expiry")?
    );
    let unknown = rift_protocol::source_read::GetSourceParams {
        unit: rift_protocol::read::SourceUnitId::parse("rift://source/project/missing.rs")?,
        ..source
    };
    let unknown = client
        .call_tool(tool_request(
            "search",
            &serde_json::to_value(query::source(&unknown)?)?,
        ))
        .await?;
    assert_eq!(unknown.is_error, Some(true), "{unknown:?}");
    assert!(unknown.structured_content.is_none(), "{unknown:?}");
    let refusal = serde_json::to_string(&unknown)?;
    assert!(
        refusal.contains("rift.server.read_unavailable"),
        "{refusal}"
    );
    assert!(
        refusal.contains("retained query material is unavailable"),
        "{refusal}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn original_query_node_rows(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    selected: &Value,
    observe_unit: bool,
) -> TestResult<(String, String, Value)> {
    let [row] =
        query::rows(selected).map_err(|error| format!("initial query selection: {error}"))?
    else {
        return Err(format!("one original declaration: {selected}").into());
    };
    let identity = query::iri(row, "symbol")?;
    let view = selected["view"]["id"]
        .as_str()
        .ok_or("original query view")?;
    let observed = call_retrying_acceptance(
        client,
        tool_request(
            "get_symbol",
            &json!({"id":identity,"view":view,"include":["source"]}),
        ),
    )
    .await?;
    assert_eq!(observed["outcome"], "found", "{observed}");
    assert_eq!(observed["view"], selected["view"]);
    let unit = if observe_unit {
        let statement = format!(
            "SELECT ?unit {{ SERVICE <urn:rift:result:{view}> {{}} ?node a :Node; :symbol <{identity}>; :unit ?unit }}"
        );
        let observed_nodes =
            call_retrying_acceptance(client, tool_request("search", &json!({"query":statement})))
                .await?;
        let [node] = query::rows(&observed_nodes)
            .map_err(|error| format!("Node unit observation view={view}: {error}"))?
        else {
            return Err(format!("one original semantic node unit: {observed_nodes}").into());
        };
        query::iri(node, "unit")?.to_owned()
    } else if row.get("unit").is_some() {
        query::iri(row, "unit")?.to_owned()
    } else {
        let path = observed["declarations"][0]["path"]
            .as_str()
            .ok_or("original Local declaration path")?;
        rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("project")?,
            rift_core::SourcePath::new(path)?,
        )?
        .to_string()
    };
    let unit_text = unit;
    let unit = unit_text.as_str();
    let statement = format!(
        "SELECT ?id ?unit ?start ?end ?text {{ SERVICE <urn:rift:result:{view}> {{}} ?node a :Node; :symbol <{identity}>; :unit <{unit}>, ?unit; :id ?id; :range [ :start ?start; :end ?end ] . <{unit}> :source ?text }} ORDER BY ?start ?end"
    );
    let original =
        call_retrying_acceptance(client, tool_request("search", &json!({"query":statement})))
            .await?;
    let original_rows = query::rows(&original)
        .map_err(|error| format!("original Node read unit={unit} view={view}: {error}"))?;
    let [node] = original_rows else {
        return Err(format!("one original semantic declaration node: {original}").into());
    };
    assert!(!query::literal(node, "id")?.is_empty());
    assert_eq!(query::iri(node, "unit")?, unit);
    assert_eq!(query::literal(node, "text")?, FILES[1].1);
    let start = query::literal(node, "start")?.parse::<usize>()?;
    let end = query::literal(node, "end")?.parse::<usize>()?;
    let excerpt = FILES[1]
        .1
        .get(start..end)
        .ok_or("original node byte range")?;
    let exact = call_retrying_acceptance(
        client,
        tool_request(
            "get_symbol",
            &json!({"id":identity,"view":view,"include":["source"]}),
        ),
    )
    .await?;
    assert_eq!(exact["outcome"], "found", "{exact}");
    assert_eq!(exact["symbol"]["id"], identity);
    assert_eq!(exact["view"], selected["view"]);
    assert_eq!(exact["declarations"][0]["source"], excerpt);
    assert_eq!(
        exact["declarations"][0]["range"],
        json!({"start":start,"end":end})
    );
    Ok((unit_text, statement, original))
}

#[tokio::test]
async fn source_queries_read_empty_and_symbolless_files_without_declarations() -> TestResult {
    use rift_protocol::source_read::GetSourceParams;
    let files = [
        ("Cargo.toml", FILES[0].1),
        ("src/lib.rs", FILES[1].1),
        ("src/comment.rs", "// λ only\n"),
        ("src/empty.rs", ""),
    ];
    let (directory, client, server_task) = served_workspace(&files, None).await?;
    for (path, original) in &files[2..] {
        let request = GetSourceParams {
            unit: rift_protocol::read::SourceUnitId::parse(&format!(
                "rift://source/project/{path}"
            ))?,
            range: None,
            view: None,
            rev: None,
        };
        let answer = call_retrying_acceptance(
            &client,
            tool_request("search", &serde_json::to_value(query::source(&request)?)?),
        )
        .await?;
        let [row] = query::rows(&answer)? else {
            return Err(format!("one complete source without declarations: {answer}").into());
        };
        assert_eq!(query::literal(row, "outcome")?, "found");
        assert_eq!(query::iri(row, "unit")?, request.unit.as_str());
        assert_eq!(query::literal(row, "text")?, *original);
        assert_eq!(query::literal(row, "start")?, "0");
        assert_eq!(
            query::literal(row, "end")?.parse::<usize>()?,
            original.len()
        );
        assert_eq!(query::literal(row, "complete")?, "true");
        let source_view = query::literal(row, "source_view")?.to_owned();
        let expiry = query::literal(row, "source_expiry")?.to_owned();
        let outer = answer["view"].clone();
        let chain = GetSourceParams {
            unit: request.unit.clone(),
            range: None,
            view: Some(rift_protocol::symbol_read::CapturedViewId::parse(
                outer["id"].as_str().ok_or("source query outer view")?,
            )?),
            rev: None,
        };
        let held = GetSourceParams {
            view: Some(rift_protocol::symbol_read::CapturedViewId::parse(
                &source_view,
            )?),
            ..request
        };
        std::fs::write(directory.path().join(path), "pub fn replacement() {}\n")?;
        std::fs::remove_file(directory.path().join(path))?;
        let answer = call_retrying_acceptance(
            &client,
            tool_request("search", &serde_json::to_value(query::source(&held)?)?),
        )
        .await?;
        let [row] = query::rows(&answer)? else {
            return Err(format!("one retained symbolless source: {answer}").into());
        };
        assert_eq!(query::iri(row, "unit")?, held.unit.as_str());
        assert_eq!(query::literal(row, "text")?, *original);
        assert_eq!(query::literal(row, "source_view")?, source_view);
        assert_eq!(query::literal(row, "source_expiry")?, expiry);
        assert_eq!(query::literal(row, "complete")?, "true");
        let (chain, parent) = assert_source_query_chain(&client, chain, outer, original).await?;
        if !original.is_empty() {
            assert_source_query_ranges(&client, chain, &parent, held).await?;
        }
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn assert_source_query_chain(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    mut chain: rift_protocol::source_read::GetSourceParams,
    mut parent: Value,
    original: &str,
) -> TestResult<(rift_protocol::source_read::GetSourceParams, Value)> {
    for _ in 0..2 {
        let answer = call_retrying_acceptance(
            client,
            tool_request("search", &serde_json::to_value(query::source(&chain)?)?),
        )
        .await?;
        let [row] = query::rows(&answer)? else {
            return Err(format!("one source from its own outer query view: {answer}").into());
        };
        assert_eq!(query::iri(row, "unit")?, chain.unit.as_str());
        assert_eq!(query::literal(row, "text")?, original);
        assert_eq!(query::literal(row, "start")?, "0");
        assert_eq!(
            query::literal(row, "end")?.parse::<usize>()?,
            original.len()
        );
        assert_eq!(
            query::literal(row, "source_view")?,
            parent["id"].as_str().ok_or("parent id")?
        );
        assert_eq!(
            query::literal(row, "source_expiry")?,
            parent["expires_at"].as_str().ok_or("parent expiry")?
        );
        assert_eq!(query::literal(row, "complete")?, "true");
        parent = answer["view"].clone();
        chain.view = Some(rift_protocol::symbol_read::CapturedViewId::parse(
            parent["id"]
                .as_str()
                .ok_or("next source query outer view")?,
        )?);
    }
    Ok((chain, parent))
}

async fn assert_source_query_ranges(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    chain: rift_protocol::source_read::GetSourceParams,
    parent: &Value,
    held: rift_protocol::source_read::GetSourceParams,
) -> TestResult {
    use rift_protocol::source_read::GetSourceParams;
    let chain_range = GetSourceParams {
        range: Some(rift_protocol::read::TextRange { start: 3, end: 5 }),
        ..chain
    };
    let answer = call_retrying_acceptance(
        client,
        tool_request(
            "search",
            &serde_json::to_value(query::source(&chain_range)?)?,
        ),
    )
    .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("one UTF-8 range from chained source view: {answer}").into());
    };
    assert_eq!(query::iri(row, "unit")?, chain_range.unit.as_str());
    assert_eq!(query::literal(row, "text")?, "λ");
    assert_eq!(query::literal(row, "start")?, "3");
    assert_eq!(query::literal(row, "end")?, "5");
    assert_eq!(
        query::literal(row, "source_view")?,
        parent["id"].as_str().ok_or("range parent id")?
    );
    let partial = GetSourceParams {
        range: Some(rift_protocol::read::TextRange { start: 3, end: 5 }),
        ..held.clone()
    };
    let answer = call_retrying_acceptance(
        client,
        tool_request("search", &serde_json::to_value(query::source(&partial)?)?),
    )
    .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("one exact UTF-8 range: {answer}").into());
    };
    assert_eq!(query::literal(row, "text")?, "λ");
    assert_eq!(query::literal(row, "start")?, "3");
    assert_eq!(query::literal(row, "end")?, "5");
    let malformed = GetSourceParams {
        range: Some(rift_protocol::read::TextRange { start: 3, end: 4 }),
        ..held
    };
    for silent in [false, true] {
        let mut request = query::source(&malformed)?;
        if silent {
            request.query = request
                .query
                .replacen("SERVICE :source", "SERVICE SILENT :source", 1);
        }
        let refused = client
            .call_tool(tool_request("search", &serde_json::to_value(request)?))
            .await?;
        assert_eq!(refused.is_error, Some(true), "{refused:?}");
        assert!(refused.structured_content.is_none());
        assert!(
            serde_json::to_string(&refused)?
                .contains("source range must contain complete UTF-8 bytes")
        );
    }
    Ok(())
}

#[tokio::test]
async fn source_service_and_domain_patterns_keep_one_local_graph() -> TestResult {
    let (_directory, client, server_task) = served_workspace(FILES, None).await?;
    let statement = "SELECT ?name ?text { ?symbol a :Symbol; :name 'beacon', ?name . SERVICE :source { ?read :unit <rift://source/project/src/lib.rs>; :outcome 'found'; :source [ :text ?text ] } }";
    let answer =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let [row] = query::rows(&answer)? else {
        return Err(format!("one domain row with exact source: {answer}").into());
    };
    assert_eq!(query::literal(row, "name")?, "beacon");
    assert_eq!(query::literal(row, "text")?, FILES[1].1);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn captured_source_requests_allow_subsets_and_reordered_service_groups() -> TestResult {
    let a = "// a λ\n";
    let b = "// b λ\n";
    let files = [
        ("Cargo.toml", FILES[0].1),
        ("src/lib.rs", FILES[1].1),
        ("src/a.rs", a),
        ("src/b.rs", b),
    ];
    let (directory, client, server_task) = served_workspace(&files, None).await?;
    let statement = "SELECT ?unit_a ?text_a ?view_a ?expiry_a ?unit_b ?text_b ?view_b ?expiry_b { SERVICE :source { ?a :unit <rift://source/project/src/a.rs>; :outcome 'found'; :source [ :text ?text_a; :span [ :unit ?unit_a ] ]; :view [ :id ?view_a; :expires_at ?expiry_a ] } SERVICE :source { ?b :unit <rift://source/project/src/b.rs>; :outcome 'found'; :source [ :text ?text_b; :span [ :unit ?unit_b ] ]; :view [ :id ?view_b; :expires_at ?expiry_b ] } }";
    let selected =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let [row] = query::rows(&selected)? else {
        return Err(format!("two original source associations: {selected}").into());
    };
    assert_eq!(query::literal(row, "text_a")?, a);
    assert_eq!(query::literal(row, "text_b")?, b);
    assert_eq!(
        query::literal(row, "view_a")?,
        query::literal(row, "view_b")?
    );
    assert_eq!(
        query::literal(row, "expiry_a")?,
        query::literal(row, "expiry_b")?
    );
    let view = query::literal(row, "view_a")?;
    let expiry = query::literal(row, "expiry_a")?;
    let unit_a = query::iri(row, "unit_a")?;
    let unit_b = query::iri(row, "unit_b")?;
    std::fs::remove_file(directory.path().join("src/a.rs"))?;
    std::fs::remove_file(directory.path().join("src/b.rs"))?;
    let request = rift_protocol::source_read::GetSourceParams {
        unit: rift_protocol::read::SourceUnitId::parse(unit_a)?,
        range: None,
        view: Some(rift_protocol::symbol_read::CapturedViewId::parse(view)?),
        rev: None,
    };
    let subset = call_retrying_acceptance(
        &client,
        tool_request("search", &serde_json::to_value(query::source(&request)?)?),
    )
    .await?;
    let [subset_row] = query::rows(&subset)? else {
        return Err(format!("one captured subset source: {subset}").into());
    };
    assert_eq!(query::literal(subset_row, "text")?, a);
    assert_eq!(query::literal(subset_row, "source_view")?, view);
    assert_eq!(query::literal(subset_row, "source_expiry")?, expiry);
    let captured_view = oxigraph::model::Literal::from(view);
    let statement = format!(
        "SELECT ?text_a ?text_b {{ SERVICE :source {{ ?b :unit <{unit_b}>; :view {captured_view}; :outcome 'found'; :source [ :text ?text_b ] }} SERVICE :source {{ ?a :unit <{unit_a}>; :view {captured_view}; :outcome 'found'; :source [ :text ?text_a ] }} }}"
    );
    let reordered =
        call_retrying_acceptance(&client, tool_request("search", &json!({"query":statement})))
            .await?;
    let [reordered_row] = query::rows(&reordered)? else {
        return Err(format!("reordered captured source services: {reordered}").into());
    };
    assert_eq!(query::literal(reordered_row, "text_a")?, a);
    assert_eq!(query::literal(reordered_row, "text_b")?, b);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
