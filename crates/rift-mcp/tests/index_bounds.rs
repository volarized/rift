//! The two bounds a large workspace meets first, proven through the served surface:
//! the lexical index's `[search.lexical] units_max` key, and one file a syntax provider
//! refuses under its own bounds.
//!
//! A workspace past `units_max` refuses to build naming the key and its maximum, and the
//! same workspace under the default serves. A workspace holding one file past the syntax
//! depth bound still answers `search` from its other file, with the deep file absent.

mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::fs;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use serde_json::{Value, json};
use workspace_client::{TestResult, served_workspace, tool_request};

/// Text files past `units_max = 1000`, one lexical unit each.
const UNIT_FILE_COUNT: usize = 1_100;

/// The smallest `units_max` acceptance admits, on its own table.
const UNITS_MAX_CONFIGURATION: &str = "[search.lexical]\nunits_max = 1000\n";

/// Parenthesis nesting past the shipped syntax depth bound of 512.
const DEEP_NESTING: usize = 600;

/// One normal source file and `UNIT_FILE_COUNT` one-line text files.
fn unit_files() -> Vec<(String, String)> {
    let mut files = vec![("lib.rs".to_owned(), "pub fn beacon() {}\n".to_owned())];
    files.extend(
        (0..UNIT_FILE_COUNT)
            .map(|index| (format!("note-{index:04}.txt"), format!("note {index}\n"))),
    );
    files
}

/// The same files in the borrowed form the served-workspace scaffolding takes.
fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect()
}

/// A Rust source whose syntax tree runs deeper than the provider accepts.
fn deep_source() -> String {
    format!(
        "pub fn deep() -> i32 {{ {open}1{close} }}\n",
        open = "(".repeat(DEEP_NESTING),
        close = ")".repeat(DEEP_NESTING),
    )
}

/// The project paths the hits on one search page name.
fn hit_paths(answer: &Value) -> Vec<&str> {
    answer["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hit| hit["path"].as_str())
        .collect()
}

/// One search page for `query`, as the structured content the tool answers with.
async fn search_page(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    query: &str,
) -> TestResult<Value> {
    let answer = client
        .call_tool(tool_request("search", &json!({"query": query})))
        .await?;
    answer
        .structured_content
        .ok_or_else(|| "search answers with structured content".into())
}

#[tokio::test]
async fn a_workspace_past_units_max_refuses_to_build_naming_the_key() -> TestResult {
    let directory = tempfile::tempdir()?;
    for (name, source) in unit_files() {
        fs::write(directory.path().join(name), source)?;
    }
    let mut configuration = hermetic_search::SEMANTIC_DISABLED.to_owned();
    configuration.push_str(UNITS_MAX_CONFIGURATION);
    fs::write(directory.path().join("rift.toml"), configuration)?;

    let refusal = match RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await {
        Ok(_server) => return Err("a workspace past units_max must refuse to build".into()),
        Err(error) => error.to_string(),
    };
    assert!(
        refusal.contains("units_max") && refusal.contains("maximum 1000"),
        "the refusal must name the key and its maximum: {refusal}"
    );
    Ok(())
}

#[tokio::test]
async fn the_same_workspace_serves_under_the_default_units_max() -> TestResult {
    let files = unit_files();
    let (_directory, client, server_task) = served_workspace(&borrowed(&files), None).await?;

    let answer = search_page(&client, "beacon").await?;
    assert!(hit_paths(&answer).contains(&"lib.rs"), "{answer:#}");

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn a_file_past_a_syntax_bound_is_left_out_and_the_rest_serves() -> TestResult {
    let deep = deep_source();
    let (_directory, client, server_task) = served_workspace(
        &[
            ("src/lib.rs", "pub fn beacon() {}\n"),
            ("src/deep.rs", deep.as_str()),
        ],
        None,
    )
    .await?;

    let kept = search_page(&client, "beacon").await?;
    assert!(hit_paths(&kept).contains(&"src/lib.rs"), "{kept:#}");
    let absent = search_page(&client, "deep").await?;
    assert!(
        !hit_paths(&absent).contains(&"src/deep.rs"),
        "the deep file answers no search: {absent:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
