//! The bounds a large workspace meets first, proven through the served surface: the
//! lexical index's `[search.lexical] units_max` key, the `[source] workspace_size` key,
//! one file a syntax provider refuses under its own bounds, and the file bound on
//! `search`'s `paths.force_include`.
//!
//! A workspace past `workspace_size` refuses to build naming the key and its maximum, and
//! the same workspace under the default serves; lowering `workspace_size` on a served
//! workspace fails the rebuild, and the next request answers from the last snapshot with
//! `stale_index` naming the key. A workspace past `units_max` serves, and `search` answers
//! from identifier matching with `lexical_ranking_unavailable` naming the key and its
//! maximum. A workspace holding one file past the syntax depth bound still answers
//! `search` from its other file, with the deep file absent. A `force_include` matching
//! more files than its bound refuses naming the field, the bound, and the count.

mod hermetic_search;
#[allow(dead_code)]
mod workspace_client;

use std::fs;

use rift_core::constants::FORCE_INCLUDE_FILES_MAX;
use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use serde_json::{Value, json};
use workspace_client::{TestResult, served_root, served_workspace, tool_request};

/// Polls of one served answer a test waits on before it gives up: two seconds, at
/// [`ANSWER_POLL`] each.
const ANSWER_ATTEMPTS_MAX: usize = 100;
/// Wait between two polls of a served answer.
const ANSWER_POLL: std::time::Duration = std::time::Duration::from_millis(20);

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

/// The `detail` of the first warning on `answer` whose code is `code`, when one is.
fn warning_detail<'a>(answer: &'a Value, code: &str) -> Option<&'a str> {
    answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == code)
        .and_then(|warning| warning["detail"].as_str())
}

/// Polls `search` for `query` under a bound until `accept` takes the answer.
async fn search_until(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    query: &str,
    what: &str,
    accept: impl Fn(&Value) -> bool,
) -> TestResult<Value> {
    let mut answer = search_page(client, query).await?;
    for _attempt in 0..ANSWER_ATTEMPTS_MAX {
        if accept(&answer) {
            return Ok(answer);
        }
        tokio::time::sleep(ANSWER_POLL).await;
        answer = search_page(client, query).await?;
    }
    Err(format!("{what}; the last answer was {answer:#}").into())
}

#[tokio::test]
async fn a_workspace_past_units_max_serves_and_search_names_the_key() -> TestResult {
    let directory = tempfile::tempdir()?;
    for (name, source) in unit_files() {
        fs::write(directory.path().join(name), source)?;
    }
    let mut configuration = hermetic_search::SEMANTIC_DISABLED.to_owned();
    configuration.push_str(UNITS_MAX_CONFIGURATION);
    fs::write(directory.path().join("rift.toml"), configuration)?;
    let (client, server_task) = served_root(directory.path()).await?;

    // The lexical commit runs behind the publication, so the store's refusal reaches the
    // answer once the lane has run it; until then the answer names the commit it waits on.
    let answer = search_until(
        &client,
        "beacon",
        "the refused commit never reached search",
        |answer| {
            warning_detail(answer, "lexical_ranking_unavailable").is_some_and(|detail| {
                detail.contains("units_max") && detail.contains("maximum 1000")
            })
        },
    )
    .await?;
    assert!(
        hit_paths(&answer).contains(&"lib.rs"),
        "identifier matching still answers: {answer:#}"
    );

    client.cancel().await?;
    server_task.await?;
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
    // The rebuild under the lowered bound fails, and the read answers from the last
    // snapshot with the failure rather than waiting for a rebuild that cannot land.
    let stale = search_page(&client, "beacon").await?;
    assert!(hit_paths(&stale).contains(&"lib.rs"), "{stale:#}");
    let detail = warning_detail(&stale, "stale_index")
        .ok_or_else(|| format!("the answer carries stale_index: {stale:#}"))?;
    assert!(
        detail.contains("source.workspace_size") && detail.contains("maximum 16777216"),
        "the warning must name the key and its maximum: {detail}"
    );

    // The restored file reaches the server through the filesystem watcher, so the
    // recovery is polled under a bound rather than asserted on the next request.
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    search_until(
        &client,
        "beacon",
        "the restored bound must serve the workspace again without a warning",
        |answer| {
            hit_paths(answer).contains(&"lib.rs") && warning_detail(answer, "stale_index").is_none()
        },
    )
    .await?;

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Text files below `extra/`, two past the file bound one `paths.force_include` may reach.
const FORCE_INCLUDE_FILE_COUNT: usize = FORCE_INCLUDE_FILES_MAX + 2;

/// One normal source file, a `.gitignore` leaving `extra/` out of the index, and
/// `FORCE_INCLUDE_FILE_COUNT` one-line text files below it.
fn force_include_files() -> Vec<(String, String)> {
    let mut files = vec![
        ("lib.rs".to_owned(), "pub fn beacon() {}\n".to_owned()),
        (".gitignore".to_owned(), "extra/\n".to_owned()),
    ];
    files.extend((0..FORCE_INCLUDE_FILE_COUNT).map(|index| {
        (
            format!("extra/note-{index:04}.txt"),
            format!("note {index}\n"),
        )
    }));
    files
}

/// The wire `data` of one refused tool call.
fn refusal_data(error: rmcp::ServiceError) -> Value {
    match error {
        rmcp::ServiceError::McpError(data) => data.data.expect("wire error data must be present"),
        other => panic!("expected a protocol-level McpError, got {other:?}"),
    }
}

#[tokio::test]
async fn a_force_include_past_its_file_bound_refuses_with_the_match_count_as_evidence() -> TestResult
{
    let files = force_include_files();
    let (_directory, client, server_task) = served_workspace(&borrowed(&files), None).await?;

    let refused = client
        .call_tool(tool_request(
            "search",
            &json!({ "query": "note", "paths": { "force_include": ["extra/**"] } }),
        ))
        .await
        .expect_err("a force_include past its file bound refuses the request");
    let wire = refusal_data(refused);
    assert_eq!(wire["code"], json!("limit_exceeded"), "{wire:#}");
    assert_eq!(
        wire["limit"],
        json!({
            "field": "paths.force_include",
            "limit": FORCE_INCLUDE_FILES_MAX,
            "required": FORCE_INCLUDE_FILE_COUNT
        }),
        "the refusal must carry typed wire evidence: {wire:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
