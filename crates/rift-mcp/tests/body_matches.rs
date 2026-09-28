//! Proves one copy of the text through the `search` tool: a word only a declaration's body
//! holds reaches that declaration, matched through its content, although no symbol row
//! stores the declaration's source; and a lockfile the index leaves out answers no search
//! while a request selecting it says so.

mod hermetic_search;

use std::error::Error;
use std::fs;
use std::path::Path;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

async fn client_for(root: &Path) -> TestResult<RunningService<RoleClient, ()>> {
    let server = RiftMcp::build(root, WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    Ok(().serve(client_transport).await?)
}

async fn search(client: &RunningService<RoleClient, ()>, arguments: Value) -> TestResult<Value> {
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or("search arguments are an object")?;
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(arguments))
        .await?;
    result
        .structured_content
        .ok_or_else(|| "search must return structured content".into())
}

/// Each hit's declared name, or its path for a file hit, in answer order.
fn named(answer: &Value) -> Vec<String> {
    answer["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .map(|hit| {
                    hit["hit"]["symbol"]["name"]
                        .as_str()
                        .or_else(|| hit["path"].as_str())
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn workspace() -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    fs::write(root.join("rift.toml"), hermetic_search::HERMETIC_TABLES)?;
    fs::create_dir_all(root.join("src"))?;
    fs::write(
        root.join("src/lib.rs"),
        "pub fn alpha() -> u32 {\n    let quokka = 1;\n    quokka\n}\n\npub fn beta() {}\n",
    )?;
    fs::write(
        root.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"quokka\"\n",
    )?;
    Ok(directory)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_term_reaches_its_declaration_and_a_lockfile_answers_nothing() -> TestResult {
    let directory = workspace()?;
    let client = client_for(directory.path()).await?;

    let symbols = search(&client, json!({ "query": "quokka", "target": "symbol" })).await?;
    assert_eq!(named(&symbols), ["alpha"], "{symbols:#}");
    assert_eq!(
        symbols["results"][0]["matched_by"],
        json!(["content"]),
        "a declaration its body placed answers through its content: {symbols:#}"
    );

    let files = search(&client, json!({ "query": "quokka", "target": "file" })).await?;
    assert_eq!(
        named(&files),
        ["src/lib.rs"],
        "no lockfile answers: {files:#}"
    );

    let selected = search(
        &client,
        json!({ "query": "quokka", "paths": { "include": ["Cargo.lock"] } }),
    )
    .await?;
    assert!(named(&selected).is_empty(), "{selected:#}");
    let warning = selected["warnings"]
        .as_array()
        .and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == json!("lockfile_excluded"))
        })
        .ok_or("selecting an excluded lockfile warns")?;
    assert_eq!(
        warning["files"],
        json!(["rift://file/Cargo.lock"]),
        "{warning:#}"
    );

    client.cancel().await?;
    Ok(())
}
