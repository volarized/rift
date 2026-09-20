//! Drives `search`'s `traversal` block through a live rmcp client over a fixture workspace:
//! a callers walk, an indirect-caller walk, a path query, a hit reached both lexically and
//! by the walk, and the refusals a walk with no edge source draws.
//!
//! A configured language engine resolves the references the walk follows, so every fixture
//! here selects the embedded `ty` engine over a Python workspace: no external process, and
//! the same lane a real read uses.

mod hermetic_search;
// `served_relative_workspace` and its `relative_spelling` helper are part of
// `workspace_client`'s shared surface; this binary serves its root the plain way.
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
    "graph.py",
    "def leaf() -> int:\n    return 1\n\n\
     def branch_a() -> int:\n    return leaf()\n\n\
     def branch_b() -> int:\n    return leaf()\n\n\
     def root() -> int:\n    return branch_a() + branch_b()\n",
)];

/// The engine that resolves this workspace's references, embedded in the binary.
const ENGINE: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

/// A workspace with no configured engine, so no lane resolves a reference in it.
const ENGINELESS_FILES: &[(&str, &str)] = &[("lib.rs", "pub fn beacon() {}\n")];

const LEAF: &str = "rift://symbol/python/graph.py/leaf";

#[tokio::test]
async fn search_traversal_neighbors_depth_one_reaches_the_direct_callers() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({ "traversal": { "seed": LEAF } })),
    )
    .await?;

    let reached = symbol_names(&structured);
    assert_eq!(
        reached,
        ["branch_a", "branch_b"],
        "a walk with no explicit depth reaches exactly leaf's direct callers: {structured}"
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
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": LEAF,
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
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": LEAF,
                    "depth": 2,
                    "to": "rift://symbol/python/graph.py/root"
                }
            }),
        ),
    )
    .await?;

    let reached = results(&structured);
    assert_eq!(reached.len(), 1, "{structured}");
    assert_eq!(reached[0]["hit"]["symbol"]["name"], json!("root"));
    assert_eq!(
        reached[0]["distance"],
        json!(2),
        "root's shortest path from leaf is two hops: {structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// A hit both `query` and `traversal` reach carries every field either lane placed: the
/// lexical `matched_by` entry stays beside the traversal's, and the hit keeps its walked path.
#[tokio::test]
async fn search_query_and_traversal_merge_carry_both_matched_by_entries_and_the_walked_path()
-> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "query": "branch_a",
                "target": "symbol",
                "traversal": { "seed": LEAF }
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

/// The engine lane resolves references alone, so an `implements` asked for beside them has
/// no lane and the served answer says so instead of leaving the caller to read the walk's
/// hits as the whole coverage.
#[tokio::test]
async fn search_traversal_over_an_unproduced_facet_serves_the_coverage_warning() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": LEAF,
                    "facets": ["implements", "references"]
                }
            }),
        ),
    )
    .await?;

    assert_eq!(symbol_names(&structured), ["branch_a", "branch_b"]);
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

    client.cancel().await?;
    Ok(())
}

/// A facets list naming no facet the engine lane populates leaves the walk no edge source
/// at all, so the read refuses instead of answering an empty page.
#[tokio::test]
async fn search_traversal_over_only_unproduced_facets_refuses() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let code = refusal_code(
        &client,
        &json!({"traversal": {"seed": LEAF, "facets": ["implements"]}}),
    )
    .await?;

    assert_eq!(code, json!("capability_unavailable"));

    client.cancel().await?;
    Ok(())
}

/// The same walk narrowed to the produced facet carries no warning, so the code keeps
/// naming an absent lane rather than an empty answer.
#[tokio::test]
async fn search_traversal_over_a_produced_facet_serves_no_coverage_warning() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": LEAF,
                    "facets": ["references"]
                }
            }),
        ),
    )
    .await?;

    assert_eq!(symbol_names(&structured), ["branch_a", "branch_b"]);
    assert!(
        structured["warnings"].is_null(),
        "a walk the engine lane covers carries no warning: {structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// A walk names its starting declaration through `seed`, so a request naming none refuses.
#[tokio::test]
async fn search_traversal_without_a_seed_refuses() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(ENGINE.to_owned())).await?;

    let code = refusal_code(&client, &json!({"traversal": {"direction": "incoming"}})).await?;

    assert_eq!(code, json!("invalid_request"));

    client.cancel().await?;
    Ok(())
}

/// No configured engine serves Rust in this workspace, so nothing resolves a reference for
/// the seed and the walk has no edge source at all.
#[tokio::test]
async fn search_traversal_without_an_engine_refuses_capability_unavailable() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;

    let code = refusal_code(
        &client,
        &json!({"traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}}),
    )
    .await?;

    assert_eq!(code, json!("capability_unavailable"));

    client.cancel().await?;
    Ok(())
}

/// A comparison names two committed revisions, which no engine session serves, so a walk
/// beside one refuses.
#[tokio::test]
async fn search_traversal_beside_a_change_refuses_capability_unavailable() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;

    let code = refusal_code(
        &client,
        &json!({
            "change": {"base": "baseline"},
            "traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}
        }),
    )
    .await?;

    assert_eq!(code, json!("capability_unavailable"));

    client.cancel().await?;
    Ok(())
}

/// The `code` the server refuses `arguments` with.
async fn refusal_code(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    arguments: &Value,
) -> TestResult<Value> {
    let error = client
        .call_tool(tool_request("search", arguments))
        .await
        .expect_err("the server must refuse this walk");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .cloned()
        .ok_or_else(|| "a refusal carries its code".into())
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
