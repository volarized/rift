//! The bounds a large workspace meets first, proven through the served surface: the
//! lexical index's `[search.lexical] units_max` key, the `[source] workspace_size` key,
//! one file a syntax provider refuses under its own bounds, and one file the Contribution
//! contract refuses.
//!
//! A workspace past `units_max` or `workspace_size` refuses to build naming the key and
//! its maximum, and the same workspace under the default serves; lowering
//! `workspace_size` on a served workspace rebuilds the index and refuses the next
//! request the same way. A workspace holding one file past the syntax depth bound still
//! answers `search` from its other file, with the deep file absent; a force-included file
//! the Contribution contract refuses is left out the same way, and the answer names it.

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

/// A declaration named exactly `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes: the document keeps
/// it, and the identity minted from it passes the bound, so the Contribution contract
/// refuses it when the file is published.
fn wide_source() -> String {
    format!(
        "pub struct {};\n",
        "S".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX)
    )
}

/// Whether one search answer names `path` in a `source_unavailable` warning.
fn names_unavailable(answer: &Value, path: &str) -> bool {
    let unit = format!("rift://file/{path}");
    answer["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|warning| warning["code"] == "source_unavailable" && warning["unit"] == unit)
    })
}

/// `search` with `paths.force_include` naming a file the walk skipped and the Contribution
/// contract refuses answers from the other file, with the refused file named in a
/// `source_unavailable` warning, instead of failing the request. A plain search carries
/// no such warning: the file is left out by the on-demand index alone.
#[tokio::test]
async fn a_force_included_file_the_contract_refuses_is_left_out_and_the_answer_warns() -> TestResult
{
    let wide = wide_source();
    let (_directory, client, server_task) = served_workspace(
        &[
            (".gitignore", "src/wide.rs\n"),
            ("src/lib.rs", "pub fn beacon() {}\n"),
            ("src/wide.rs", wide.as_str()),
        ],
        None,
    )
    .await?;

    let plain = search_page(&client, "beacon").await?;
    assert!(hit_paths(&plain).contains(&"src/lib.rs"), "{plain:#}");
    assert!(
        !names_unavailable(&plain, "src/wide.rs"),
        "the walk never reaches the ignored file: {plain:#}"
    );

    let forced = client
        .call_tool(tool_request(
            "search",
            &json!({"query": "beacon", "paths": {"force_include": ["src/wide.rs"]}}),
        ))
        .await?;
    let forced = forced
        .structured_content
        .ok_or("search answers with structured content")?;
    assert!(hit_paths(&forced).contains(&"src/lib.rs"), "{forced:#}");
    assert!(
        names_unavailable(&forced, "src/wide.rs"),
        "the answer names the refused file: {forced:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Text files whose bytes together pass the smallest `workspace_size` acceptance admits.
const WORKSPACE_FILE_COUNT: usize = 17;

/// Bytes each of those files holds: one MiB, under every language's per-file bound.
const WORKSPACE_FILE_BYTES: usize = 1 << 20;

/// The smallest `workspace_size` acceptance admits, on its own table.
const WORKSPACE_SIZE_CONFIGURATION: &str = "[source]\nworkspace_size = \"16mb\"\n";

/// One normal source file and `WORKSPACE_FILE_COUNT` text files of `WORKSPACE_FILE_BYTES`
/// each.
fn workspace_files() -> Vec<(String, String)> {
    let mut files = vec![("lib.rs".to_owned(), "pub fn beacon() {}\n".to_owned())];
    let line = "bulk text line\n";
    let body = line.repeat(WORKSPACE_FILE_BYTES / line.len());
    files.extend(
        (0..WORKSPACE_FILE_COUNT).map(|index| (format!("bulk-{index:02}.txt"), body.clone())),
    );
    files
}

/// The message of one refused tool call.
fn refusal_message(error: rmcp::ServiceError) -> String {
    match error {
        rmcp::ServiceError::McpError(data) => data.message.into_owned(),
        other => panic!("expected a protocol-level McpError, got {other:?}"),
    }
}

#[tokio::test]
async fn a_workspace_past_workspace_size_refuses_to_build_naming_the_key() -> TestResult {
    let directory = tempfile::tempdir()?;
    for (name, source) in workspace_files() {
        fs::write(directory.path().join(name), source)?;
    }
    let mut configuration = hermetic_search::SEMANTIC_DISABLED.to_owned();
    configuration.push_str(WORKSPACE_SIZE_CONFIGURATION);
    fs::write(directory.path().join("rift.toml"), configuration)?;

    let refusal = match RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await {
        Ok(_server) => {
            return Err("a workspace past workspace_size must refuse to build".into());
        }
        Err(error) => error.to_string(),
    };
    assert!(
        refusal.contains("source.workspace_size") && refusal.contains("maximum 16777216"),
        "the refusal must name the key and its maximum: {refusal}"
    );
    Ok(())
}

#[tokio::test]
async fn the_same_workspace_serves_under_the_default_workspace_size_and_a_change_rebuilds()
-> TestResult {
    let files = workspace_files();
    let (directory, client, server_task) = served_workspace(&borrowed(&files), None).await?;

    let answer = search_page(&client, "beacon").await?;
    assert!(hit_paths(&answer).contains(&"lib.rs"), "{answer:#}");

    let mut lowered = hermetic_search::SEMANTIC_DISABLED.to_owned();
    lowered.push_str(WORKSPACE_SIZE_CONFIGURATION);
    fs::write(directory.path().join("rift.toml"), lowered)?;
    let refused = client
        .call_tool(tool_request("search", &json!({"query": "beacon"})))
        .await
        .expect_err("the rebuild under the lowered bound refuses the request");
    let message = refusal_message(refused);
    assert!(
        message.contains("source.workspace_size"),
        "the refusal must name the key: {message}"
    );

    // The restored file reaches the server through the filesystem watcher, so the
    // recovery is polled under a bound rather than asserted on the next request.
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    let mut recovered = false;
    for _attempt in 0..100 {
        if search_page(&client, "beacon")
            .await
            .is_ok_and(|answer| hit_paths(&answer).contains(&"lib.rs"))
        {
            recovered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        recovered,
        "the restored bound must serve the workspace again"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
