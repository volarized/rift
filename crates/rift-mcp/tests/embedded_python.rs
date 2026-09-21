//! Incoming Python references through the embedded ty engine and public MCP tools.

mod hermetic_search;
#[expect(
    dead_code,
    reason = "This suite uses the relative-root workspace fixture."
)]
mod workspace_client;

use serde_json::json;
use workspace_client::{
    TestResult, call_retrying_acceptance, served_relative_workspace, tool_request,
};

const SERVICE: &str = "def serve(port: int) -> int:\n    return port\n";
const CALLER: &str = "from service import serve\n\ndef caller() -> int:\n    return serve(8080)\n";
const CONFIGURATION: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

#[tokio::test]
async fn incoming_references_resolve_cross_file_callers_without_writes() -> TestResult {
    let (directory, client, server_task) = served_relative_workspace(
        &[("service.py", SERVICE), ("main.py", CALLER)],
        Some(CONFIGURATION.to_owned()),
    )
    .await?;
    let lookup = call_retrying_acceptance(
        &client,
        tool_request("get_symbol", &json!({"name":"serve", "language":"python"})),
    )
    .await?;
    let seed = lookup["hits"][0]["symbol"]["id"]
        .as_str()
        .ok_or("serve must have a symbol identity")?;
    let answer = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"target":"symbol", "traversal":{
                "seed":seed, "direction":"incoming", "facets":["references"]
            }}),
        ),
    )
    .await?;
    let results = answer["results"]
        .as_array()
        .ok_or("search must return results")?;
    assert_eq!(results.len(), 1, "{answer:#}");
    let hit = &results[0];
    assert_eq!(hit["path"], json!("main.py"), "{answer:#}");
    assert_eq!(hit["hit"]["symbol"]["name"], json!("caller"), "{answer:#}");
    let path = hit["traversal_path"]
        .as_array()
        .ok_or("caller must carry its traversal path")?;
    assert_eq!(path.len(), 1, "{answer:#}");
    assert_eq!(path[0]["direction"], json!("incoming"), "{answer:#}");
    let relationship = &path[0]["relationship"];
    assert_eq!(
        relationship["from"], hit["hit"]["symbol"]["id"],
        "{answer:#}"
    );
    assert_eq!(relationship["to"], json!(seed), "{answer:#}");
    assert_eq!(relationship["facets"], json!(["references"]), "{answer:#}");
    assert_eq!(
        relationship["derivation"],
        json!("resolution"),
        "{answer:#}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("service.py"))?,
        SERVICE
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("main.py"))?,
        CALLER
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
