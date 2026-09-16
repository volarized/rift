//! Drives `search`'s `change` block through a live rmcp client over a committed fixture
//! workspace: an introduced declaration, a removed one, a signature change, the walk a
//! `traversal` riding beside the comparison runs, and the refusals a comparison of two
//! committed revisions answers.

mod hermetic_search;
// `served_relative_workspace` and its `relative_spelling` helper are part of
// `workspace_client`'s shared surface; this binary drives no engine, so the root spelling
// never matters here.
#[allow(dead_code)]
mod workspace_client;

use std::fs;

use rift_history::fixture::{commit_all, git, init};
use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use workspace_client::{TestResult, call_retrying_acceptance, served_root, tool_request};

/// The baseline revision, tagged `baseline`: one declaration that stays, one that a later
/// revision removes, and one whose signature a later revision widens.
const BASELINE: &[(&str, &str)] = &[
    ("src/kept.rs", "pub fn kept() {}\n"),
    ("src/gone.rs", "pub fn gone() {}\n"),
    ("src/shifted.rs", "pub fn shifted() {}\n"),
];

/// The head revision: `src/gone.rs` is deleted, `shifted` takes a parameter, and
/// `src/added.rs` arrives.
const HEAD: &[(&str, &str)] = &[
    ("src/shifted.rs", "pub fn shifted(flag: bool) {}\n"),
    ("src/added.rs", "pub fn added() {}\n"),
];

/// One served workspace holding both revisions, with `baseline` tagged on the first.
async fn served_change_workspace() -> TestResult<(
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    write_all(directory.path(), BASELINE)?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    fs::remove_file(directory.path().join("src/gone.rs"))?;
    write_all(directory.path(), HEAD)?;
    commit_all(directory.path(), "head");
    let (client, server_task) = served_root(directory.path()).await?;
    Ok((directory, client, server_task))
}

fn write_all(root: &std::path::Path, files: &[(&str, &str)]) -> TestResult {
    for (name, source) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, source)?;
    }
    Ok(())
}

fn results(structured: &Value) -> &[Value] {
    structured["results"].as_array().map_or(&[], Vec::as_slice)
}

/// Each hit's declaration name, its change kind, and the paths it names on both sides.
fn changes(structured: &Value) -> Vec<(String, String, Value, Value)> {
    results(structured)
        .iter()
        .map(|hit| {
            (
                hit["hit"]["symbol"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                hit["change"]["kind"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                hit["change"]["base_path"].clone(),
                hit["change"]["head_path"].clone(),
            )
        })
        .collect()
}

#[tokio::test]
async fn search_change_answers_every_declaration_the_two_revisions_differ_in() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "change": {"base": "baseline", "head": "HEAD"},
                "include": ["source"]
            }),
        ),
    )
    .await?;

    assert_eq!(
        changes(&structured),
        [
            (
                "added".to_owned(),
                "introduced".to_owned(),
                Value::Null,
                json!("src/added.rs")
            ),
            (
                "gone".to_owned(),
                "removed".to_owned(),
                json!("src/gone.rs"),
                Value::Null
            ),
            (
                "shifted".to_owned(),
                "signature_changed".to_owned(),
                json!("src/shifted.rs"),
                json!("src/shifted.rs")
            ),
        ],
        "{structured}"
    );
    for hit in results(&structured) {
        assert_eq!(hit["matched_by"], json!(["change"]), "{hit}");
        assert!(hit["score"].is_null(), "a comparison ranks nothing: {hit}");
    }
    let removed = &results(&structured)[1];
    assert_eq!(
        removed["source"],
        json!("pub fn gone() {}"),
        "a removed declaration keeps its base-side source: {removed}"
    );
    assert_eq!(removed["path"], json!("src/gone.rs"), "{removed}");

    client.cancel().await?;
    Ok(())
}

/// `head` defaults to `HEAD`, so a comparison naming `base` alone answers the same set.
#[tokio::test]
async fn search_change_with_base_alone_compares_against_head() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"change": {"base": "baseline"}})),
    )
    .await?;

    let names: Vec<String> = changes(&structured).into_iter().map(|hit| hit.0).collect();
    assert_eq!(names, ["added", "gone", "shifted"], "{structured}");

    client.cancel().await?;
    Ok(())
}

/// A comparison reaches declarations alone, so a `file` target carries no change hit.
#[tokio::test]
async fn search_change_with_a_file_target_answers_an_empty_page() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({"change": {"base": "baseline"}, "target": "file"}),
        ),
    )
    .await?;

    assert!(results(&structured).is_empty(), "{structured}");

    client.cancel().await?;
    Ok(())
}

/// The refusals a comparison answers, each with the wire code the served surface carries.
#[tokio::test]
async fn search_change_refuses_the_requests_it_cannot_answer() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let refusals = [
        (
            json!({"change": {"base": "baseline"}, "rev": "main"}),
            "invalid_request",
        ),
        (
            json!({"change": {"base": "baseline"}, "query": "kept"}),
            "invalid_request",
        ),
        (
            json!({
                "change": {"base": "baseline"},
                "traversal": {"seed": "rift://symbol/rust/src/kept.rs/kept"}
            }),
            "invalid_request",
        ),
        (
            json!({"change": {"base": "baseline"}, "scope": "all"}),
            "invalid_request",
        ),
        (
            json!({"change": {"base": "no-such-branch"}}),
            "resource_not_found",
        ),
    ];

    for (arguments, code) in refusals {
        let error = client
            .call_tool(tool_request("search", &arguments))
            .await
            .expect_err("the server must refuse this comparison");
        let rmcp::ServiceError::McpError(error) = error else {
            panic!("the refusal must arrive as an MCP error: {error}");
        };
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(code)),
            "arguments={arguments}, error={error:?}"
        );
    }

    client.cancel().await?;
    Ok(())
}

/// A workspace whose `[providers.history]` table is off serves no committed source, so a
/// comparison refuses the way every other revision read does.
#[tokio::test]
async fn search_change_refuses_a_workspace_with_history_disabled() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        format!(
            "{}\n[providers.history]\nenabled = false\n",
            hermetic_search::SEMANTIC_DISABLED
        ),
    )?;
    write_all(directory.path(), BASELINE)?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    let (client, _server_task) = served_root(directory.path()).await?;

    let error = client
        .call_tool(tool_request(
            "search",
            &json!({"change": {"base": "baseline"}}),
        ))
        .await
        .expect_err("a workspace with history disabled must refuse");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!("capability_unavailable")),
        "{error:?}"
    );

    client.cancel().await?;
    Ok(())
}

/// A workspace no repository versions has no committed revision to compare, so a
/// comparison refuses the way a revision read does.
#[tokio::test]
async fn search_change_refuses_a_workspace_with_no_repository() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    write_all(directory.path(), BASELINE)?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;

    let error = client
        .call_tool(tool_request("search", &json!({"change": {"base": "main"}})))
        .await
        .expect_err("a workspace with no repository must refuse");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!("capability_unavailable")),
        "{error:?}"
    );

    client.cancel().await?;
    server_task.abort();
    Ok(())
}

/// A declaration that moved between two paths comes back as one `moved` hit carrying both
/// sides' paths, and its identity reads the head side.
#[tokio::test]
async fn search_change_pairs_a_declaration_moved_between_paths() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    write_all(
        directory.path(),
        &[("src/from.rs", "pub fn travelled() {}\n")],
    )?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    fs::remove_file(directory.path().join("src/from.rs"))?;
    write_all(
        directory.path(),
        &[("src/to.rs", "pub fn travelled() {}\n")],
    )?;
    commit_all(directory.path(), "head");
    let (client, _server_task) = served_root(directory.path()).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request("search", &json!({"change": {"base": "baseline"}})),
    )
    .await?;

    assert_eq!(
        changes(&structured),
        [(
            "travelled".to_owned(),
            "moved".to_owned(),
            json!("src/from.rs"),
            json!("src/to.rs")
        )],
        "{structured}"
    );
    assert_eq!(
        results(&structured)[0]["hit"]["symbol"]["id"],
        json!("rift://symbol/rust/src/to.rs/travelled"),
        "{structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// The base revision of the impact fixture: `watched`, and `calls_watched` calling it.
const IMPACT_BASE: &str = "pub fn watched() {}\npub fn calls_watched() {\n    watched();\n}\n";

/// The head revision: `watched` takes a parameter, and its caller's bytes are unchanged.
const IMPACT_HEAD: &str =
    "pub fn watched(flag: bool) {}\npub fn calls_watched() {\n    watched();\n}\n";

/// A `traversal` riding beside a comparison starts at every changed declaration, reaches
/// their callers through the current tree's relationship graph, and discloses that the
/// edges are that tree's rather than either compared revision's.
#[tokio::test]
async fn search_change_with_a_traversal_reaches_the_callers_of_every_changed_declaration()
-> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED,
    )?;
    write_all(directory.path(), &[("lib.rs", IMPACT_BASE)])?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    write_all(directory.path(), &[("lib.rs", IMPACT_HEAD)])?;
    commit_all(directory.path(), "head");
    let (client, _server_task) = served_root(directory.path()).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "change": {"base": "baseline"},
                "traversal": {"direction": "incoming", "facets": ["calls"]}
            }),
        ),
    )
    .await?;

    let names: Vec<String> = results(&structured)
        .iter()
        .filter_map(|hit| hit["hit"]["symbol"]["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(names, ["calls_watched", "watched"], "{structured}");
    let caller = &results(&structured)[0];
    assert_eq!(caller["matched_by"], json!(["relationship"]), "{caller}");
    assert_eq!(caller["distance"], json!(1), "{caller}");
    assert_eq!(
        caller["traversal_path"][0]["relationship"]["to"],
        json!("rift://symbol/rust/lib.rs/watched"),
        "the first hop names the changed declaration the walk started at: {caller}"
    );
    assert!(caller["change"].is_null(), "{caller}");
    let changed = &results(&structured)[1];
    assert_eq!(changed["matched_by"], json!(["change"]), "{changed}");
    assert_eq!(
        changed["change"]["kind"],
        json!("signature_changed"),
        "{changed}"
    );
    let disclosure = structured["warnings"]
        .as_array()
        .and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == json!("change_traversal_current_tree"))
        })
        .ok_or("every walked comparison discloses which tree supplied the edges")?;
    assert_eq!(disclosure["unplaced"], json!(0), "{disclosure}");

    client.cancel().await?;
    Ok(())
}
