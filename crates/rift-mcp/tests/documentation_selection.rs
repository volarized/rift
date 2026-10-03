//! Proves the documentation selection through `search`: by default a workspace's change
//! log and archived pages stay out of `target: "documentation"`, and the `[documentation]`
//! table overrides that default on the next request after `rift.toml` changes, through the
//! same reconcile path every index-owned table takes.

mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::error::Error;
use std::fs;
use std::path::Path;

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use serde_json::{Value, json};
use workspace_client::await_workspace_ready;

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
    let client = ().serve(client_transport).await?;
    await_workspace_ready(&client).await?;
    Ok(client)
}

/// The paths of the documentation hits a `lighthouse` query answers, sorted.
async fn documentation_paths(client: &RunningService<RoleClient, ()>) -> TestResult<Vec<String>> {
    let arguments = json!({ "query": "lighthouse", "target": "documentation", "limit": 50 });
    let arguments = arguments
        .as_object()
        .cloned()
        .ok_or("tool arguments must be an object")?;
    let result = client
        .call_tool(CallToolRequestParams::new("search").with_arguments(arguments))
        .await?;
    let answer: Value = result
        .structured_content
        .ok_or("search must return structured content")?;
    let mut paths: Vec<String> = answer["results"]
        .as_array()
        .ok_or_else(|| format!("search must answer results: {answer:#}"))?
        .iter()
        .filter_map(|hit| hit["path"].as_str().map(str::to_owned))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn write_workspace(root: &Path) -> TestResult {
    fs::create_dir_all(root.join("docs/archive"))?;
    fs::write(
        root.join("README.md"),
        "# Beacon\n\nThe lighthouse beacon guides ships home.\n",
    )?;
    fs::write(
        root.join("CHANGELOG.md"),
        "# Changes\n\nThe lighthouse beacon gained a second lamp.\n",
    )?;
    fs::write(
        root.join("docs/guide.md"),
        "# Guide\n\nPoint the lighthouse beacon at the harbor.\n",
    )?;
    fs::write(
        root.join("docs/archive/v1.md"),
        "# Version one\n\nThe first lighthouse beacon burned oil.\n",
    )?;
    Ok(())
}

#[tokio::test]
async fn the_documentation_table_overrides_the_default_selection_on_the_next_request() -> TestResult
{
    let directory = tempfile::tempdir()?;
    write_workspace(directory.path())?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let client = client_for(directory.path()).await?;

    assert_eq!(
        documentation_paths(&client).await?,
        ["README.md", "docs/guide.md"],
        "the change log and the archived page stay out by default"
    );

    let force_included = format!(
        "{}\n[documentation]\nforce_include = [\"CHANGELOG.md\"]\nexclude = [\"docs/guide.md\"]\n",
        hermetic_search::HERMETIC_TABLES
    );
    fs::write(directory.path().join("rift.toml"), force_included)?;
    assert_eq!(
        documentation_paths(&client).await?,
        ["CHANGELOG.md", "README.md"],
        "force_include adds the change log back, and exclude drops the guide"
    );

    let disabled = format!(
        "{}\n[documentation]\nenabled = false\n",
        hermetic_search::HERMETIC_TABLES
    );
    fs::write(directory.path().join("rift.toml"), disabled)?;
    assert!(
        documentation_paths(&client).await?.is_empty(),
        "a disabled table collects no documentation file"
    );

    client.cancel().await?;
    Ok(())
}
