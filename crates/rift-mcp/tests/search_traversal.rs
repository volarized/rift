//! Drives `search`'s `traversal` block through a live rmcp client over a fixture workspace: a
//! neighbors walk, an impact walk, a path query, and a hit reached both lexically and by the
//! walk.

mod hermetic_search;
// `served_relative_workspace` and its `relative_spelling` helper are part of
// `workspace_client`'s shared surface; this binary drives no engine, so the root spelling
// never matters here.
#[allow(dead_code)]
mod workspace_client;

use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_workspace, tool_request};

/// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`:
///
/// ```text
/// root --calls--> branch_a --calls--> leaf
/// root --calls--> branch_b --calls--> leaf
/// ```
const CALL_GRAPH_FILES: &[(&str, &str)] = &[(
    "src/lib.rs",
    "pub fn root() {\n    branch_a();\n    branch_b();\n}\n\
     pub fn branch_a() {\n    leaf();\n}\n\
     pub fn branch_b() {\n    leaf();\n}\n\
     pub fn leaf() {}\n",
)];

#[tokio::test]
async fn search_traversal_neighbors_depth_one_reaches_the_direct_calls() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/root"
                }
            }),
        ),
    )
    .await?;

    let reached = symbol_names(&structured);
    assert_eq!(
        reached,
        ["branch_a", "branch_b"],
        "an outgoing walk with no explicit depth reaches exactly root's direct calls: \
         {structured}"
    );
    for hit in results(&structured) {
        assert_eq!(hit["distance"], json!(1), "{hit}");
        assert_eq!(
            hit["traversal_path"].as_array().map(Vec::len),
            Some(1),
            "{hit}"
        );
        assert!(
            hit["matched_by"]
                .as_array()
                .is_some_and(|matched| matched.contains(&json!("relationship"))),
            "{hit}"
        );
    }

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_impact_incoming_depth_two_reaches_every_caller() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/leaf",
                    "direction": "incoming",
                    "depth": 2
                }
            }),
        ),
    )
    .await?;

    let reached = symbol_names(&structured);
    assert_eq!(
        reached,
        ["branch_a", "branch_b", "root"],
        "an incoming walk from leaf reaches both direct callers and root, its indirect one: \
         {structured}"
    );
    let root_hit = results(&structured)
        .into_iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == json!("root"))
        .ok_or("root must be a hit")?;
    assert_eq!(
        root_hit["distance"],
        json!(2),
        "root reaches leaf through one intermediate caller: {root_hit}"
    );

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_to_keeps_only_the_path_query_target() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/root",
                    "depth": 2,
                    "to": "rift://symbol/rust/src/lib.rs/leaf"
                }
            }),
        ),
    )
    .await?;

    let reached = results(&structured);
    assert_eq!(reached.len(), 1, "{structured}");
    assert_eq!(reached[0]["hit"]["symbol"]["name"], json!("leaf"));
    assert_eq!(
        reached[0]["distance"],
        json!(2),
        "leaf's shortest path from root is two hops: {structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// A hit both `query` and `traversal` reach carries every field either lane placed: the
/// lexical `matched_by` entry stays beside the traversal's, and the hit keeps its walked path.
#[tokio::test]
async fn search_query_and_traversal_merge_carry_both_matched_by_entries_and_the_walked_path()
-> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "query": "branch_a",
                "target": "symbol",
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/root"
                }
            }),
        ),
    )
    .await?;

    let hit = results(&structured)
        .into_iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == json!("branch_a"))
        .ok_or("branch_a must be a hit")?;
    let matched_by = hit["matched_by"]
        .as_array()
        .ok_or("matched_by must be an array")?;
    assert!(matched_by.contains(&json!("name")), "{hit}");
    assert!(matched_by.contains(&json!("relationship")), "{hit}");
    assert_eq!(
        hit["traversal_path"].as_array().map(Vec::len),
        Some(1),
        "{hit}"
    );

    client.cancel().await?;
    Ok(())
}

fn results(structured: &Value) -> Vec<Value> {
    structured["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Every symbol hit's `name`, sorted, so a walk's discovery order never makes an assertion
/// flaky.
fn symbol_names(structured: &Value) -> Vec<String> {
    let mut names: Vec<String> = results(structured)
        .iter()
        .filter_map(|hit| hit["hit"]["symbol"]["name"].as_str().map(str::to_owned))
        .collect();
    names.sort();
    names
}
