//! Capture the documentation fixtures through the live, in-process MCP surface.

mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::fs;
use std::path::Path;

use rift_history::fixture::{commit_all_at, git, init};
use rmcp::model::ReadResourceRequestParams;
use serde_json::{Map, json};
use workspace_client::{
    TestResult, query_after_population, resource_json, resource_text, served_prepared_workspace,
    tool_request,
};

const MAP_URI: &str = "rift://map";
const FIXTURE_CONFIGURATION: &str = include_str!("fixtures/docs-example/rift.toml");
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
        "package-scope",
        r#"SELECT ?symbol ?name {
  SERVICE :search {
    ?hit :query "Serialize"; :target "symbol"; :scope "global";
         :packages '[{"manager":"cargo","registry":"crates.io","name":"serde_core"}]'^^rdf:JSON;
         :symbol ?symbol; :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 5"#,
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
    (
        "pinned-package",
        r#"SELECT ?symbol ?name {
  SERVICE :search {
    ?hit :query "spawn_blocking"; :target "symbol"; :scope "all";
         :packages '[{"manager":"cargo","registry":"crates.io","name":"tokio","version":"1.54.1"}]'^^rdf:JSON;
         :symbol ?symbol; :index ?index .
    ?symbol :name ?name
  }
}
ORDER BY ?index
LIMIT 5"#,
    ),
];

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
        let arguments = json!({ "query": query });
        let remote = matches!(name, "package-scope" | "pinned-package");
        if !remote {
            query_after_population(&client, &serde_json::from_value(arguments.clone())?).await?;
        }
        let response = client.call_tool(tool_request("search", &arguments)).await?;
        let text = response
            .content
            .first()
            .and_then(rmcp::model::ContentBlock::as_text)
            .ok_or("search response is missing its compact text")?
            .text
            .clone();
        let mut json = match response.structured_content {
            Some(json) => json,
            None if remote => json!({ "content": response.content, "isError": response.is_error }),
            None => return Err(format!("{name}: search failed with {text}").into()),
        };
        // A retained-view identity and expiry belong to the capture's process.
        if let Some(fields) = json.as_object_mut() {
            fields.remove("view");
        }
        queries.insert(
            name.to_owned(),
            json!({ "query": query, "arguments": arguments, "text": text, "json": json }),
        );
    }

    let resource = client
        .read_resource(ReadResourceRequestParams::new(MAP_URI.to_owned()))
        .await?;
    let map = json!({
        "arguments": { "uri": MAP_URI },
        "text": resource_text(&resource, MAP_URI)?,
        "json": resource_json(&resource, MAP_URI)?,
    });

    client.cancel().await?;
    server.await?;
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
