//! `rift://map` end to end: the workspace orientation snapshot an agent reads to get its
//! bearings.
//!
//! The server routes a resource read by its URI family, so a suite that called the map
//! handler directly would leave that routing unproven. This one drives a live rmcp client,
//! the way an agent does.

mod hermetic_search;
// `workspace_client` carries the shared served-workspace scaffolding; this binary reads
// resources rather than calling tools, so it uses one entry point.
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
        return Err("a map read answers with text".into());
    };
    Ok(serde_json::from_str(text)?)
}

#[tokio::test]
async fn the_map_resource_answers_revision_languages_modules_and_entry_points() -> TestResult {
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "src/lib.rs",
                "pub fn beacon() {}\nfn main() { beacon(); }\n",
            ),
            ("docs/guide.md", "# Guide\n"),
        ],
        None,
    )
    .await?;

    let body = resource_body(&client, "rift://map").await?;

    assert_eq!(body["revision"].as_str().map(str::len), Some(8), "{body:#}");

    let languages = body["languages"]
        .as_array()
        .ok_or("map languages are an array")?;
    let rust = languages
        .iter()
        .find(|language| language["language"] == "rust")
        .ok_or("the rust entry is reported")?;
    assert_eq!(rust["files"], Value::from(1), "{rust}");
    assert_eq!(rust["symbols"], Value::from(2), "{rust}");

    let modules = body["modules"]
        .as_array()
        .ok_or("map modules are an array")?;
    let src = modules
        .iter()
        .find(|module| module["path"] == "src")
        .ok_or("the src module is reported")?;
    assert_eq!(src["files"], Value::from(1), "{src}");
    assert_eq!(src["symbols"], Value::from(2), "{src}");
    assert!(
        modules.iter().all(|module| module["path"] != "src/lib.rs"),
        "modules name directories, not files: {body:#}"
    );

    assert_eq!(
        body["entry_points"],
        serde_json::json!(["rift://symbol/rust/src/lib.rs/main"]),
        "{body:#}"
    );
    assert_eq!(
        body["docs"],
        serde_json::json!(["docs/guide.md"]),
        "{body:#}"
    );
    assert_eq!(body["pagination"]["page_index"], Value::from(0));
    assert_eq!(body["pagination"]["total_pages"], Value::from(1));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn an_unknown_query_string_on_the_map_uri_is_refused() -> TestResult {
    let (_directory, client, server_task) =
        served_workspace(&[("lib.rs", "pub fn beacon() {}\n")], None).await?;

    let error = client
        .read_resource(ReadResourceRequestParams::new(
            "rift://map?page_index=0".to_owned(),
        ))
        .await
        .expect_err("a query string on rift://map names no resource");
    assert!(
        error.to_string().contains("rift://map?page_index=0"),
        "{error}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn declared_resources_lists_map() -> TestResult {
    let (_directory, client, server_task) =
        served_workspace(&[("lib.rs", "pub fn beacon() {}\n")], None).await?;

    let resources = client.list_all_resources().await?;
    assert!(
        resources
            .iter()
            .any(|resource| resource.uri == "rift://map"),
        "{resources:#?}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
