//! Capture the documentation fixtures through the live, in-process MCP surface.

#[allow(dead_code)]
mod global_api;
mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::fs;
use std::path::Path;

use global_api::{FixtureOptions, GlobalFixture, NAMED_POSITIONS, SymbolFixture};
use rift_history::fixture::{commit_all_at, git, init};
use rmcp::{RoleClient, model::ReadResourceRequestParams, service::RunningService};
use serde_json::{Map, Value, json};
use workspace_client::{
    TestResult, captured_seed, query_after_population, resource_json, resource_text,
    served_prepared_workspace, served_workspace, tool_request,
};

const MAP_URI: &str = "rift://map";
const FIXTURE_CONFIGURATION: &str = include_str!("fixtures/docs-example/rift.toml");
const WORKSPACE_URI: &str = "rift://workspace";
const PACKAGE_QUERIES: &[(&str, &str)] = &[
    (
        "package-scope",
        r#"SELECT ?symbol ?name {
  SERVICE :search {
    ?hit :query "beacon"; :target "symbol"; :scope "global";
         :packages '[{"manager":"cargo","registry":"crates.io","name":"demo"}]'^^rdf:JSON;
         :symbol ?symbol; :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 5"#,
    ),
    (
        "pinned-package",
        r#"SELECT ?symbol ?name {
  SERVICE :search {
    ?hit :query "beacon"; :target "symbol"; :scope "all";
         :packages '[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]'^^rdf:JSON;
         :symbol ?symbol; :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 5"#,
    ),
];
const FIXTURE: &[(&str, &str, &str)] = &[
    (
        "Cargo.toml",
        "toml",
        include_str!("fixtures/docs-example/Cargo.toml"),
    ),
    (
        "README.md",
        "markdown",
        include_str!("fixtures/docs-example/README.md"),
    ),
    ("rift.toml", "toml", FIXTURE_CONFIGURATION),
    (
        "src/config.rs",
        "rust",
        include_str!("fixtures/docs-example/src/config.rs"),
    ),
    (
        "src/error.rs",
        "rust",
        include_str!("fixtures/docs-example/src/error.rs"),
    ),
    (
        "src/lib.rs",
        "rust",
        include_str!("fixtures/docs-example/src/lib.rs"),
    ),
    (
        "src/main.rs",
        "rust",
        include_str!("fixtures/docs-example/src/main.rs"),
    ),
];

const QUERIES: &[(&str, &str)] = &[
    (
        "select-symbols",
        "SELECT ?symbol ?name { ?symbol a :Symbol; :name ?name } ORDER BY ?name ?symbol LIMIT 5",
    ),
    (
        "empty-search",
        "SELECT ?symbol { ?symbol a :Symbol; :name \"load_configs\" }",
    ),
    (
        "symbol-name",
        "SELECT ?symbol ?kind {\n  ?symbol a :Symbol; :name \"load_config\"; :kind ?kind\n} ORDER BY ?symbol",
    ),
    (
        "full-text",
        r#"SELECT ?symbol ?name ?source {
  SERVICE :search {
    ?hit :query "load_config"; :target "symbol";
         :symbol ?symbol; :source ?source; :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 3"#,
    ),
    (
        "regex",
        r#"SELECT ?path ?start ?end ?source {
  SERVICE :search {
    ?hit :pattern "tokio::[A-Za-z_]+"; :target "file";
         :path ?path; :start ?start; :end ?end; :source ?source .
  }
}
ORDER BY ?path ?start"#,
    ),
    (
        "edge-provider",
        r#"ASK {
  ?symbol a :Symbol; :name "load_config"; :calls ?target .
  ?edge a :Edge; :from ?symbol; :to ?target; :kinds :calls; :provider "static" .
}"#,
    ),
    (
        "physical-usages",
        r#"SELECT ?unit ?start ?end {
  ?symbol a :Symbol; :name "load_config"; :usages ?usage .
  ?usage :range [ :unit ?unit; :start ?start; :end ?end ] .
}
ORDER BY ?unit ?start"#,
    ),
    (
        "references",
        r#"SELECT ?caller {
  ?symbol a :Symbol; :name "load_config" .
  ?caller :references ?symbol .
}
ORDER BY ?caller"#,
    ),
    (
        "calls",
        r#"SELECT ?callee {
  ?symbol a :Symbol; :name "load_config" .
  ?symbol :calls/:calls ?callee .
}
ORDER BY ?callee"#,
    ),
    (
        "transitive-references",
        r#"SELECT DISTINCT ?caller {
  ?symbol a :Symbol; :name "load_config" .
  ?symbol ^:references+ ?caller .
}
ORDER BY ?caller"#,
    ),
    (
        "flows",
        r#"SELECT ?flow ?score ?index ?symbol {
  SERVICE :flows {
    ?flow :query "load_config"; :max_results 2; :score ?score; :steps ?step .
    ?step :index ?index; :value ?symbol .
  }
}
ORDER BY DESC(?score) ?flow ?index"#,
    ),
    (
        "changes",
        r#"SELECT ?symbol ?kind ?base_path ?head_path {
  SERVICE :search {
    ?hit :change '{"base":"baseline","head":"HEAD"}'^^rdf:JSON;
         :symbol ?symbol; :change ?change .
    ?change :kind ?kind .
    OPTIONAL { ?change :base_path ?base_path }
    OPTIONAL { ?change :head_path ?head_path }
  }
}
ORDER BY ?symbol ?kind"#,
    ),
    (
        "commits",
        r#"SELECT ?message ?revision {
  SERVICE :search {
    ?hit :target "commit"; :query "configuration"; :hit ?record .
    ?record :commit ?commit .
    ?commit :message ?message; :revision ?revision .
  }
}
ORDER BY ?message ?revision"#,
    ),
];

fn carries_warnings(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.contains_key("warnings") || fields.values().any(carries_warnings)
        }
        Value::Array(items) => items.iter().any(carries_warnings),
        _ => false,
    }
}

fn check_capture(text: &str, answer: &Value) {
    assert!(
        !carries_warnings(answer),
        "MCP capture carries warnings: {answer}"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with("warning | ")),
        "MCP capture carries a warning line: {text}"
    );
}

fn check_search_examples(examples: &Map<String, Value>) -> TestResult<()> {
    let selected = &examples["select-symbols"];
    let table = selected["text"].as_str().ok_or("search text")?;
    let mut lines = table.lines();
    assert_eq!(
        lines.next(),
        Some("Query finished: 5 results found on 1/1 pages with 10 results per page."),
        "{table}"
    );
    assert_eq!(lines.next(), Some(""), "{table}");
    let header = lines.next().ok_or("search table header")?;
    assert_eq!(
        header.split('|').map(str::trim).collect::<Vec<_>>(),
        ["", "?symbol", "?name", ""],
        "{table}"
    );
    let divider = lines.next().ok_or("search table divider")?;
    let divider_cells: Vec<_> = divider
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect();
    assert_eq!(divider_cells.len(), 2, "{table}");
    assert!(
        divider_cells
            .iter()
            .all(|cell| cell.len() >= 3 && cell.bytes().all(|byte| byte == b'-')),
        "{table}"
    );
    let rows: Vec<_> = lines.collect();
    assert_eq!(rows.len(), 5, "{table}");
    assert!(
        rows.iter()
            .all(|row| row.starts_with("| ") && row.ends_with(" |")),
        "{table}"
    );

    let empty = &examples["empty-search"];
    assert!(
        workspace_client::query::rows(&empty["json"])?.is_empty(),
        "{empty}"
    );
    assert_eq!(
        empty["text"],
        "No results were found. Please check the query examples to build it correctly.\n"
    );
    Ok(())
}

/// Captures both MCP response forms and drops the process-local view.
async fn capture_tool(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    arguments: Value,
    expected_error: bool,
) -> TestResult<Value> {
    let response = client.call_tool(tool_request(tool, &arguments)).await?;
    let text = &response
        .content
        .first()
        .and_then(rmcp::model::ContentBlock::as_text)
        .ok_or("tool response is missing its compact text")?
        .text;
    assert_eq!(
        response.is_error.unwrap_or(false),
        expected_error,
        "{tool} {arguments}: {text}"
    );
    let mut answer = match &response.structured_content {
        Some(answer) => answer.clone(),
        None => serde_json::to_value(&response)?,
    };
    check_capture(text, &answer);
    if let Some(fields) = answer.as_object_mut() {
        fields.remove("view");
    }
    Ok(json!({ "tool": tool, "arguments": arguments, "text": text, "json": answer }))
}

async fn capture_query(client: &RunningService<RoleClient, ()>, query: &str) -> TestResult<Value> {
    let arguments = json!({ "query": query });
    query_after_population(client, &serde_json::from_value(arguments.clone())?).await?;
    let example = capture_tool(client, "search", arguments, false).await?;
    assert!(
        example["json"].get("answer").is_some(),
        "{query}: {example}"
    );
    Ok(example)
}

async fn capture_resource(client: &RunningService<RoleClient, ()>, uri: &str) -> TestResult<Value> {
    let response = client
        .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
        .await?;
    let text = resource_text(&response, uri)?;
    let answer = resource_json(&response, uri)?;
    check_capture(text, &answer);
    Ok(json!({
        "arguments": { "uri": uri },
        "text": text,
        "json": answer,
    }))
}

/// A separate workspace keeps the loopback endpoint out of the illustrated project.
async fn capture_routed_examples(examples: &mut Map<String, Value>) -> TestResult<()> {
    let global = GlobalFixture::start_with(FixtureOptions {
        python_collection: true,
        query_narrowed: false,
        ..FixtureOptions::default()
    })
    .await?;
    // The loopback port changes on each capture. Keep configuration bytes out of the tree digest.
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\n\
         [languages.rust]\nstdlib = false\n\n[source]\nexclude = [\"rift.toml\"]\n",
        global.endpoint
    );
    let (directory, client, server) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = 'docs-example'\nversion = '0.1.0'\n[lib]\npath = 'lib.rs'\n",
            ),
            (
                "lib.rs",
                "pub fn beacon() {}\n\n\
                 #[cfg(unix)]\npub fn data_dir() -> &'static str { \"/var/lib/demo\" }\n\n\
                 #[cfg(windows)]\npub fn data_dir() -> &'static str { \"C:/ProgramData/demo\" }\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    let collected = global_api::collected_identity(SymbolFixture::Valid, "beacon");
    let local = captured_seed(&client, "beacon", "lib.rs").await?;
    for &(name, query) in PACKAGE_QUERIES {
        let example = capture_query(&client, query).await?;
        let rows = workspace_client::query::rows(&example["json"])?;
        assert!(
            rows.iter()
                .any(|row| workspace_client::query::iri(row, "symbol").ok()
                    == Some(collected.as_str())),
            "{name} must return the collected package symbol: {example}"
        );
        if name == "pinned-package" {
            assert!(
                rows.iter()
                    .any(|row| workspace_client::query::iri(row, "symbol").ok()
                        == Some(local.as_str())),
                "the all-scope example must include the local symbol: {example}"
            );
        }
        examples.insert(name.to_owned(), example);
    }
    for (name, arguments) in [
        (
            "global-symbol-documentation",
            json!({
                "id": collected,
                "revision": "rift://revision/cargo/crates.io/demo@1.0.0",
                "include": {"documentation": true},
            }),
        ),
        (
            "global-symbol-history",
            json!({"id": collected, "include": {"history": true}}),
        ),
        (
            "runtime-symbol",
            json!({
                "id": global_api::named_identity(&NAMED_POSITIONS[0]),
                "revision": global_api::named_release(&NAMED_POSITIONS[0]),
            }),
        ),
    ] {
        let example = capture_tool(&client, "get_symbol", arguments, false).await?;
        assert_eq!(example["json"]["outcome"], "found", "{name}: {example}");
        examples.insert(name.to_owned(), example);
    }
    assert!(examples["global-symbol-documentation"]["json"]["documentation"].is_object());
    assert!(examples["global-symbol-history"]["json"]["history"]["edges"].is_array());
    capture_missing_examples(&client, &local, directory.path(), examples).await?;
    capture_paged_examples(&client, examples).await?;
    client.cancel().await?;
    server.await?;
    Ok(())
}

async fn capture_paged_examples(
    client: &RunningService<RoleClient, ()>,
    examples: &mut Map<String, Value>,
) -> TestResult<()> {
    let id = captured_seed(client, "data_dir", "lib.rs").await?;
    let all = capture_tool(client, "get_symbol", json!({"id": id}), false).await?;
    assert_eq!(all["json"]["pagination"]["total"], 2, "{all}");
    let first = capture_tool(
        client,
        "get_symbol",
        json!({"id": id, "page": 1, "max_results": 1}),
        false,
    )
    .await?;
    let revision = first["json"]["revision"].as_str().ok_or("page revision")?;
    let next = capture_tool(
        client,
        "get_symbol",
        json!({"id": id, "page": 2, "max_results": 1, "revision": revision}),
        false,
    )
    .await?;
    assert_eq!(next["json"]["pagination"]["page"], 2, "{next}");
    for (name, example) in [
        ("multi-declaration-full", all),
        ("multi-declaration-first", first),
        ("multi-declaration-next", next),
    ] {
        examples.insert(name.to_owned(), example);
    }
    Ok(())
}

async fn capture_missing_examples(
    client: &RunningService<RoleClient, ()>,
    anchor: &str,
    workspace: &Path,
    examples: &mut Map<String, Value>,
) -> TestResult<()> {
    let prefix = anchor.strip_suffix("/beacon").ok_or("beacon identity")?;
    let missing = capture_tool(
        client,
        "get_symbol",
        json!({"id": format!("{prefix}/beacn")}),
        true,
    )
    .await?;
    assert_eq!(missing["json"]["outcome"], "missing", "{missing}");
    assert_eq!(
        missing["json"]["symbol_not_found"]["alternatives"][0]["id"], anchor,
        "{missing}"
    );
    examples.insert("missing-symbol".to_owned(), missing);
    let mut unavailable = capture_tool(
        client,
        "get_symbol",
        json!({"id": anchor, "revision": "rift://revision/git/main"}),
        true,
    )
    .await?;
    // The registered failure includes the canonical workspace path in its message.
    let workspace = fs::canonicalize(workspace)?;
    let workspace = workspace.to_string_lossy();
    let text = unavailable["text"].as_str().ok_or("unavailable text")?;
    assert!(text.contains(workspace.as_ref()), "{unavailable}");
    unavailable["text"] = json!(text.replace(workspace.as_ref(), "example-project"));
    unavailable["json"]["content"][0]["text"] = unavailable["text"].clone();
    examples.insert("revision-unavailable".to_owned(), unavailable);
    Ok(())
}

#[tokio::test]
#[ignore = "run with cargo xtask docs-examples"]
async fn capture_docs_examples() -> TestResult {
    let mode = std::env::var("RIFT_DOCS_EXAMPLES")?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let artifact = root.join("docs/src/components/workspace-examples.generated.json");
    let files: Vec<_> = FIXTURE
        .iter()
        .map(|&(path, _, source)| (path, source))
        .collect();
    let (directory, client, server) = served_prepared_workspace(&files, None, |workspace| {
        fs::write(workspace.join("rift.toml"), FIXTURE_CONFIGURATION)
            .expect("write the canonical fixture configuration");
        init(workspace);
        let config = workspace.join("src/config.rs");
        let source = fs::read_to_string(&config).expect("read fixture source");
        fs::write(
            &config,
            source.replace("Loads the workspace", "Reads the workspace"),
        )
        .expect("write baseline");
        commit_all_at(
            workspace,
            "Add workspace configuration reader",
            "2024-01-01T00:00:00Z",
        );
        git(workspace, &["branch", "baseline"]);
        fs::write(&config, source).expect("restore the current fixture source");
        commit_all_at(
            workspace,
            "Document configuration loading",
            "2024-01-02T00:00:00Z",
        );
    })
    .await?;

    let mut queries = Map::new();
    for &(name, query) in QUERIES {
        queries.insert(name.to_owned(), capture_query(&client, query).await?);
    }
    check_search_examples(&queries)?;
    let symbol = captured_seed(&client, "load_config", "src/config.rs").await?;
    for (name, tool, arguments, expected_error) in [
        ("local-symbol", "get_symbol", json!({"id": symbol}), false),
        ("symbol-facets", "facets", json!({"symbol": symbol}), false),
        (
            "malformed-symbol",
            "get_symbol",
            json!({"id": "load_config"}),
            true,
        ),
    ] {
        let example = capture_tool(&client, tool, arguments, expected_error).await?;
        queries.insert(name.to_owned(), example);
    }

    let map = capture_resource(&client, MAP_URI).await?;
    queries.insert(
        "workspace".to_owned(),
        capture_resource(&client, WORKSPACE_URI).await?,
    );

    client.cancel().await?;
    server.await?;
    Box::pin(capture_routed_examples(&mut queries)).await?;
    let files: Vec<_> = FIXTURE
        .iter()
        .map(|&(path, language, code)| {
            json!({
                "path": path, "language": language, "excerpt": false, "code": code,
            })
        })
        .collect();
    let generated =
        serde_json::to_string_pretty(&json!({ "map": map, "queries": queries, "files": files }))?
            + "\n";
    assert!(
        !generated
            .chars()
            .any(|character| matches!(u32::from(character), 0x00B7 | 0x2013 | 0x2014)),
        "a docs capture uses a non-ASCII separator"
    );
    match mode.as_str() {
        "generate" => fs::write(&artifact, generated)?,
        "check" => {
            if fs::read_to_string(&artifact)? != generated {
                return Err(
                    "docs example capture differs. Run cargo xtask docs-examples generate".into(),
                );
            }
        }
        _ => return Err("expected RIFT_DOCS_EXAMPLES=generate or check".into()),
    }
    drop(directory);
    Ok(())
}
