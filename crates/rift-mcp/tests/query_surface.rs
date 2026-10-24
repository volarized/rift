//! Query forms and retained results through the advertised MCP contract.

#[cfg(unix)]
#[expect(
    dead_code,
    reason = "shared engine fixtures serve several integration suites"
)]
mod fake_engine;
mod hermetic_search;
mod query;
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
