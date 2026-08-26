//! Proves `[source].exclude` reaches an already-indexed file through the live reconcile
//! path, with no server restart: `reconcile_workspace` (`crates/rift-mcp/src/server.rs`)
//! captures the configuration fingerprint beside the tree fingerprint on every request, and
//! a mismatch triggers a full rebuild whose lexical commit lands before the rebuild
//! publishes - so a single request issued after `rift.toml` names an exclusion already
//! answers from the narrowed set.

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

fn arguments(value: &Value) -> TestResult<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    tool: &'static str,
    tool_arguments: Value,
) -> TestResult<Value> {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments(&tool_arguments)?))
        .await?;
    result
        .structured_content
        .ok_or_else(|| format!("{tool} must return structured content").into())
}

/// A Rust source file's declaration reaches `get_symbol` by name; a plain-text file's prose
/// reaches only the lexical search store, since identifier search never parses a non-source
/// file and `get_symbol` resolves declarations alone. Excluding both in one `rift.toml`
/// write proves the reconcile path drops a file from each store it was ever indexed into.
#[tokio::test]
async fn source_exclude_drops_an_already_indexed_file_from_get_symbol_and_the_lexical_store()
-> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("phantom_symbol.rs"),
        "pub fn phantom_generated() {}\n",
    )?;
    fs::write(
        directory.path().join("phantom_notes.txt"),
        "Wandering falcon migrations chart northern coastal thermals precisely.\n",
    )?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    let client = client_for(directory.path()).await?;

    // Baseline: both files are indexed before the exclusion lands.
    let symbol = call(
        &client,
        "get_symbol",
        json!({ "name": "phantom_generated" }),
    )
    .await?;
    assert_eq!(
        symbol["hits"].as_array().map(Vec::len),
        Some(1),
        "the symbol must be indexed before exclusion: {symbol:#}"
    );

    let prose = call(
        &client,
        "search",
        json!({ "query": "wandering falcon migrations" }),
    )
    .await?;
    assert!(
        prose["results"].as_array().is_some_and(|results| results
            .iter()
            .any(|hit| hit["path"] == json!("phantom_notes.txt"))),
        "the text file must answer a lexical-store-only prose query before exclusion: \
         {prose:#}"
    );

    // `[source].exclude` now names both already-indexed files.
    let excluded_configuration = format!(
        "{}\n[source]\nexclude = [\"phantom_symbol.rs\", \"phantom_notes.txt\"]\n",
        hermetic_search::SEMANTIC_DISABLED
    );
    fs::write(directory.path().join("rift.toml"), excluded_configuration)?;

    // One request each proves the file left both stores, with no restart.
    let symbol_after = call(
        &client,
        "get_symbol",
        json!({ "name": "phantom_generated" }),
    )
    .await?;
    assert_eq!(
        symbol_after["hits"].as_array().map(Vec::len),
        Some(0),
        "an excluded symbol must leave get_symbol on the next request: {symbol_after:#}"
    );

    let prose_after = call(
        &client,
        "search",
        json!({ "query": "wandering falcon migrations" }),
    )
    .await?;
    assert!(
        prose_after["results"].as_array().is_some_and(Vec::is_empty),
        "an excluded text file must leave the lexical store on the next request: \
         {prose_after:#}"
    );

    client.cancel().await?;
    Ok(())
}

/// `search`'s `paths.force_include` reaches past `[source].exclude` for an ordinary
/// current-tree request, exactly as it reaches past `.gitignore`: the file is parsed on
/// demand outside the index. Excluding the symbol file and then naming it in
/// `force_include` proves the glob still resolves it.
#[tokio::test]
async fn force_include_still_reaches_a_file_source_exclude_dropped() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("phantom_symbol.rs"),
        "pub fn phantom_generated() {}\n",
    )?;
    let excluded_configuration = format!(
        "{}\n[source]\nexclude = [\"phantom_symbol.rs\"]\n",
        hermetic_search::SEMANTIC_DISABLED
    );
    fs::write(directory.path().join("rift.toml"), excluded_configuration)?;
    let client = client_for(directory.path()).await?;

    let plain = call(
        &client,
        "search",
        json!({ "query": "phantom_generated", "target": "symbol" }),
    )
    .await?;
    assert!(
        plain["results"].as_array().is_some_and(Vec::is_empty),
        "an excluded file must not answer a plain search: {plain:#}"
    );

    let forced = call(
        &client,
        "search",
        json!({
            "query": "phantom_generated",
            "target": "symbol",
            "paths": { "force_include": ["phantom_symbol.rs"] }
        }),
    )
    .await?;
    assert!(
        forced["results"].as_array().is_some_and(|results| results
            .iter()
            .any(|hit| hit["path"] == json!("phantom_symbol.rs"))),
        "force_include must still reach a source-excluded file: {forced:#}"
    );

    client.cancel().await?;
    Ok(())
}
