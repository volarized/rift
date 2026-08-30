//! `rift://workspace` end to end: the resource an agent reads accepted
//! configuration through.
//!
//! The server routes a resource read by its URI family, so a suite that
//! called the workspace handler directly would leave that routing unproven.
//! This one drives a live rmcp client, the way an agent does.

mod hermetic_search;
// `workspace_client` carries the shared served-workspace scaffolding; this
// binary reads resources rather than calling tools, so it uses one entry point.
#[allow(dead_code)]
mod workspace_client;

use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use serde_json::Value;
use workspace_client::{TestResult, served_workspace};

/// Reads one resource URI through the client and returns its text body.
async fn resource_body(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    uri: &str,
) -> TestResult<Value> {
    let answer = client
        .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
        .await?;
    let ResourceContents::TextResourceContents { text, .. } = answer
        .contents
        .first()
        .ok_or("a resource read answers with one content")?
    else {
        return Err("a workspace read answers with text".into());
    };
    Ok(serde_json::from_str(text)?)
}

#[tokio::test]
async fn the_workspace_resource_answers_effective_languages_hooks_and_source() -> TestResult {
    let configuration = "[languages.rust.lsp]\ncommand = \"rust-analyzer\"\n\
         [[hooks]]\nid = \"check\"\nkind = \"build\"\ncommand = [\"true\"]\n\
         determinism = \"deterministic\"\ninclude = [\"**/*.rs\"]\n"
        .to_owned();
    let (_directory, client, server_task) = served_workspace(
        &[
            ("lib.rs", "pub fn beacon() {}\n"),
            ("notes.txt", "beacon\n"),
        ],
        Some(configuration),
    )
    .await?;

    let body = resource_body(&client, "rift://workspace").await?;

    assert_eq!(
        body["configuration_revision"].as_str().map(str::len),
        Some(8),
        "{body:#}"
    );
    let languages = body["languages"]
        .as_array()
        .ok_or("workspace languages are an array")?;
    let rust = languages
        .iter()
        .find(|language| language["language"]["name"] == "rust")
        .ok_or("the shipped Rust entry is reported")?;
    assert_eq!(rust["syntax"], Value::from(true), "{rust}");
    assert_eq!(rust["lsp"]["process"], Value::from("rust"), "{rust}");
    assert_eq!(rust["lsp"]["state"], Value::from("stopped"), "{rust}");

    let hooks = body["hooks"].as_array().ok_or("hooks are an array")?;
    assert_eq!(hooks.len(), 1, "{body:#}");
    assert_eq!(hooks[0]["id"], Value::from("check"));
    assert_eq!(hooks[0]["include"], serde_json::json!(["**/*.rs"]));

    let source = body["source"].as_array().ok_or("source is an array")?;
    let paths: Vec<&str> = source
        .iter()
        .filter_map(|unit| unit["path"].as_str())
        .collect();
    assert!(paths.contains(&"lib.rs"), "{body:#}");
    assert!(
        paths.contains(&"notes.txt"),
        "a visible file no language claims joins the catalog: {body:#}"
    );
    assert_eq!(body["pagination"]["page_index"], Value::from(0));
    assert_eq!(body["pagination"]["total_pages"], Value::from(1));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn a_workspace_page_past_the_end_answers_an_empty_catalog() -> TestResult {
    let (_directory, client, server_task) =
        served_workspace(&[("lib.rs", "pub fn beacon() {}\n")], None).await?;

    let body = resource_body(&client, "rift://workspace?page_index=4").await?;

    assert_eq!(body["source"], serde_json::json!([]), "{body:#}");
    assert_eq!(body["pagination"]["page_index"], Value::from(4));
    assert_eq!(
        body["pagination"]["total_pages"],
        Value::from(1),
        "the page count stays the catalog's own: {body:#}"
    );
    assert!(
        body["languages"]
            .as_array()
            .is_some_and(|languages| !languages.is_empty()),
        "every page repeats the language summaries: {body:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
