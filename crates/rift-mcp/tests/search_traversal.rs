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

/// `implements` reaches no reference role, so no provider populates it and the served answer
/// says so instead of leaving the caller to read an empty result set as an absent neighbor.
#[tokio::test]
async fn search_traversal_over_an_unproduced_facet_serves_the_coverage_warning() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/root",
                    "facets": ["implements"]
                }
            }),
        ),
    )
    .await?;

    assert!(results(&structured).is_empty(), "{structured}");
    let warnings = structured["warnings"]
        .as_array()
        .ok_or("the answer must carry warnings")?;
    assert_eq!(warnings.len(), 1, "{structured}");
    assert_eq!(
        warnings[0]["code"],
        json!("relationship_coverage_missing"),
        "{structured}"
    );
    assert_eq!(warnings[0]["facets"], json!(["implements"]), "{structured}");
    assert!(
        warnings[0]["language"].is_null(),
        "a facet gap omits the language member: {structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// The same walk narrowed to a produced facet carries no warning, so the code keeps naming
/// an absent provider rather than an empty answer.
#[tokio::test]
async fn search_traversal_over_a_produced_facet_serves_no_coverage_warning() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(CALL_GRAPH_FILES, None).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/rust/src/lib.rs/root",
                    "facets": ["calls"]
                }
            }),
        ),
    )
    .await?;

    assert_eq!(symbol_names(&structured), ["branch_a", "branch_b"]);
    assert!(
        structured["warnings"].is_null(),
        "a walk every provider covers carries no warning: {structured}"
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
