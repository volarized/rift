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

use rmcp::model::ReadResourceRequestParams;
use serde_json::Value;
use workspace_client::{
    TestResult, resource_json, resource_text, served_prepared_workspace, served_root,
    served_workspace,
};

/// Reads one resource URI through the client and returns its JSON body.
async fn resource_body(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    uri: &str,
) -> TestResult<Value> {
    let answer = client
        .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
        .await?;
    resource_json(&answer, uri)
}

/// A read answers two contents for the requested URI: the compact text first, then the
/// JSON body.
#[tokio::test]
async fn the_workspace_resource_answers_compact_text_then_json() -> TestResult {
    let (_directory, client, server_task) =
        served_workspace(&[("lib.rs", "pub fn beacon() {}\n")], None).await?;

    let answer = client
        .read_resource(ReadResourceRequestParams::new(
            "rift://workspace".to_owned(),
        ))
        .await?;
    assert_eq!(answer.contents.len(), 2, "{:?}", answer.contents);
    let text = resource_text(&answer, "rift://workspace")?;
    assert!(text.starts_with("workspace "), "{text}");
    let body = resource_json(&answer, "rift://workspace")?;
    assert!(body.is_object(), "{body}");

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn the_workspace_resource_answers_effective_languages_and_source() -> TestResult {
    let configuration = "[languages.rust.lsp]\ncommand = \"rust-analyzer\"\n".to_owned();
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
        .find(|language| language["language"] == "rust")
        .ok_or("the shipped Rust entry is reported")?;
    assert_eq!(rust["syntax"], Value::from(true), "{rust}");
    assert_eq!(rust["lsp"]["process"], Value::from("rust"), "{rust}");
    assert_eq!(rust["lsp"]["state"], Value::from("stopped"), "{rust}");

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

#[tokio::test]
async fn workspace_pages_partition_the_complete_visible_catalog() -> TestResult {
    use rift_protocol::workspace::WORKSPACE_SOURCE_UNITS_MAX;

    let file_count = WORKSPACE_SOURCE_UNITS_MAX + 1;
    let configuration =
        "[languages.rust]\nenabled = false\n\n[search.text]\ninclude = [\"**/*.txt\"]\n";
    let (_directory, client, server_task) = served_prepared_workspace(
        &[("disabled.rs", "pub fn disabled() {}\n")],
        Some(configuration.to_owned()),
        |root| {
            for index in 0..file_count {
                std::fs::write(root.join(format!("opaque-{index:04}.bin")), [0xff, 0, 0xfe])
                    .expect("raw fixture file");
            }
        },
    )
    .await?;
    let first = resource_body(&client, "rift://workspace").await?;
    let second = resource_body(&client, "rift://workspace?page_index=1").await?;
    let first_rows = first["source"].as_array().ok_or("source array")?;
    let second_rows = second["source"].as_array().ok_or("source array")?;
    assert_eq!(first_rows.len(), WORKSPACE_SOURCE_UNITS_MAX);
    assert_eq!(
        second_rows.len(),
        3,
        "remaining raw files and configuration"
    );
    assert_eq!(first["pagination"]["total_pages"], Value::from(2));
    assert_eq!(second["pagination"]["total_pages"], Value::from(2));
    assert_eq!(
        first["configuration_revision"],
        second["configuration_revision"]
    );
    let paths: Vec<&str> = first_rows
        .iter()
        .chain(second_rows)
        .filter_map(|row| row["path"].as_str())
        .collect();
    assert_eq!(paths.len(), file_count + 2);
    assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
    let disabled = &first_rows[0];
    assert_eq!(disabled["path"], Value::from("disabled.rs"));
    assert_eq!(disabled["language"], Value::from("rust"));
    let raw_digest = "af9ceddc";
    for row in first_rows.iter().chain(second_rows).filter(|row| {
        row["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("opaque-"))
    }) {
        assert_eq!(row["digest"], Value::from(raw_digest));
        assert!(row.get("language").is_none());
    }
    let repeated = resource_body(&client, "rift://workspace").await?;
    assert_eq!(
        repeated, first,
        "unchanged publication repeats identical page"
    );
    let overflow =
        resource_body(&client, "rift://workspace?page_index=18446744073709551615").await?;
    assert_eq!(overflow["source"], serde_json::json!([]));
    assert_eq!(overflow["pagination"]["total_pages"], Value::from(2));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// One byte past the 4 MiB `[providers.syntax] max_file` every shipped provider
/// declares by default.
const OVERSIZED_FILE_BYTES: usize = 4 * 1024 * 1024 + 1;

/// The source listing's paths.
async fn listed_paths(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> TestResult<Vec<String>> {
    let body = resource_body(client, "rift://workspace").await?;
    Ok(body["source"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|unit| unit["path"].as_str().map(str::to_owned))
        .collect())
}

/// Under the default `[search.text] large_files = "split"`, a file past `max_file` is held
/// as text, so the source listing names it; under `skip` the index leaves it out, and the
/// listing omits it the way the index does instead of refusing the whole resource.
#[tokio::test]
async fn a_file_past_max_file_is_listed_under_split_and_absent_under_skip() -> TestResult {
    let oversized = "x".repeat(OVERSIZED_FILE_BYTES);
    let files = [
        ("lib.rs", "pub fn beacon() {}\n"),
        ("blob.txt", oversized.as_str()),
    ];
    let (_directory, client, server_task) = served_workspace(&files, None).await?;
    let paths = listed_paths(&client).await?;
    assert!(paths.iter().any(|path| path == "lib.rs"), "{paths:?}");
    assert!(
        paths.iter().any(|path| path == "blob.txt"),
        "split holds the file as text: {paths:?}"
    );
    client.cancel().await?;
    server_task.await?;

    let skip = Some("[search.text]\nlarge_files = \"skip\"\n".to_owned());
    let (_directory, client, server_task) = served_workspace(&files, skip).await?;
    let paths = listed_paths(&client).await?;
    assert!(paths.iter().any(|path| path == "lib.rs"), "{paths:?}");
    assert!(
        !paths.iter().any(|path| path == "blob.txt"),
        "a file the index leaves out is no source unit: {paths:?}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cached_workspace_resources_refuse_after_root_is_removed() -> TestResult {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    std::fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    let (client, server_task) = served_root(directory.path()).await?;
    std::fs::remove_dir_all(directory.path())?;

    for uri in ["rift://map", "rift://workspace"] {
        let error = client
            .read_resource(ReadResourceRequestParams::new(uri.to_owned()))
            .await
            .expect_err("a removed workspace root must not serve cached resource data");
        let rmcp::ServiceError::McpError(error) = error else {
            return Err(format!("the refusal must be an MCP error: {error}").into());
        };
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&serde_json::json!("resource_not_found")),
            "uri={uri}, error={error:?}"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
