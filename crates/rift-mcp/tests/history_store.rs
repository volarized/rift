//! Drives `get_symbol`'s history through a live rmcp client over a committed fixture
//! workspace: a timeline the history store answers once the background fill holds the
//! served commit, with each version's author and the file's move followed, and a second
//! worktree's server reading the store the first one filled.

mod hermetic_search;
// `served_relative_workspace` and its `relative_spelling` helper are part of
// `workspace_client`'s shared surface; this binary drives no engine, so the root spelling
// never matters here.
#[allow(dead_code)]
mod workspace_client;

use std::fs;
use std::path::Path;
use std::time::Duration;

use rift_history::fixture::{commit_all, git, init};
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, search_after_population, served_root, tool_request,
};

/// How long a test waits for the background fill of a few fixture commits.
const FILL_WAIT_MAX: Duration = Duration::from_secs(30);

/// The poll interval a test asks again at while the store lags.
const FILL_POLL: Duration = Duration::from_millis(50);

/// Three commits: `travelled` introduced in `before.rs`, its body grown, then the
/// file moved to `after.rs` without a byte changed.
fn moved_workspace(root: &Path) -> TestResult {
    fs::write(root.join("rift.toml"), hermetic_search::HERMETIC_TABLES)?;
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='history_fixture'\nversion='0.1.0'\nedition='2024'\n\
         [lib]\npath='before.rs'\n",
    )?;
    fs::write(root.join("before.rs"), "pub fn travelled() {}\n")?;
    init(root);
    commit_all(root, "introduce travelled");
    fs::write(
        root.join("before.rs"),
        "pub fn travelled() { let _grown = 1; }\n",
    )?;
    commit_all(root, "grow travelled");
    git(root, &["mv", "before.rs", "after.rs"]);
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='history_fixture'\nversion='0.1.0'\nedition='2024'\n\
         [lib]\npath='after.rs'\n",
    )?;
    commit_all(root, "move travelled");
    Ok(())
}

/// The history of the observed `travelled` identity.
async fn travelled_history(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> TestResult<Value> {
    let search = search_after_population(
        client,
        &json!({"query": "travelled", "target": "symbol", "include": ["source"]}),
    )
    .await?;
    let mut rows = search["results"]
        .as_array()
        .ok_or("search results are an array")?
        .iter()
        .filter(|row| {
            row["hit"]["symbol"]["name"] == "travelled"
                && row["hit"]["symbol"]["language"] == "rust"
                && row["path"] == "after.rs"
        });
    let row = rows
        .next()
        .ok_or("the original travelled declaration is present")?;
    assert!(rows.next().is_none(), "travelled is unique: {search}");
    assert_eq!(row["hit"]["symbol"]["kind"], "function");
    let id = row["hit"]["symbol"]["id"]
        .as_str()
        .ok_or("travelled has an established identity")?;
    let identity = rift_protocol::identity::SymbolIdentity::parse(id)?;
    assert!(matches!(
        identity.owner(),
        rift_protocol::identity::SymbolOwner::Local
    ));
    assert_eq!(identity.wire_identity(), id);
    let view = search
        .get("view")
        .ok_or("search carries its captured view")?;
    let unavailable = call_retrying_acceptance(
        client,
        tool_request(
            "get_symbol",
            &json!({"id": id, "include": ["history"], "view": view["id"]}),
        ),
    )
    .await?;
    assert_eq!(unavailable["outcome"], "unavailable", "{unavailable}");
    assert_eq!(unavailable["reason"], "insufficient_coverage");
    assert_eq!(unavailable["id"], id);
    assert_eq!(unavailable["view"], *view);
    let structured = call_retrying_acceptance(
        client,
        tool_request("get_symbol", &json!({"id": id, "include": ["history"]})),
    )
    .await?;
    assert_eq!(structured["outcome"], "found", "{structured}");
    assert_eq!(structured["symbol"]["id"], id);
    let history_view = structured
        .get("view")
        .ok_or("history carries its captured view")?;
    let repeated = call_retrying_acceptance(
        client,
        tool_request(
            "get_symbol",
            &json!({"id": id, "include": ["history"], "view": history_view["id"]}),
        ),
    )
    .await?;
    assert_eq!(repeated["outcome"], "found", "{repeated}");
    assert_eq!(repeated["symbol"]["id"], id);
    assert_eq!(repeated["view"], *history_view);
    assert_eq!(repeated["history"], structured["history"]);
    structured
        .get("history")
        .cloned()
        .ok_or_else(|| "the requested history is present".into())
}

/// Asks until the history answers complete, as a caller does while the store lags.
async fn complete_history(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> TestResult<Value> {
    let deadline = tokio::time::Instant::now() + FILL_WAIT_MAX;
    loop {
        let history = travelled_history(client).await?;
        if history["complete"] == json!(true) {
            return Ok(history);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the history never answered complete: {history}").into());
        }
        tokio::time::sleep(FILL_POLL).await;
    }
}

/// The store files one folder holds.
fn store_files(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| {
                    let database = Path::new(name)
                        .extension()
                        .is_some_and(|extension| extension == "db");
                    name.starts_with("store-") && database
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn a_symbol_history_answers_from_the_store_with_authors_across_a_move() -> TestResult {
    let directory = tempfile::tempdir()?;
    moved_workspace(directory.path())?;
    let (client, _server_task) = served_root(directory.path()).await?;

    let history = complete_history(&client).await?;

    let edges: Vec<(String, String)> = history["edges"]
        .as_array()
        .ok_or("edges is an array")?
        .iter()
        .map(|edge| {
            (
                edge["edge"].as_str().unwrap_or_default().to_owned(),
                edge["commit"]["path"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect();
    let expected = [
        ("renamed_from", "after.rs"),
        ("changed_in", "before.rs"),
        ("introduced_in", "before.rs"),
    ]
    .map(|(edge, path)| (edge.to_owned(), path.to_owned()));
    assert_eq!(edges, expected, "{history}");
    for edge in history["edges"].as_array().into_iter().flatten() {
        assert_eq!(
            edge["commit"]["author"],
            json!({"name": "Rift Fixture", "email": "fixture@rift.invalid"}),
            "{edge}"
        );
    }
    assert_eq!(store_files(&directory.path().join(".git/.rift")).len(), 1);
    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn a_second_worktree_reads_the_store_the_first_filled() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    moved_workspace(root)?;
    let (first, _first_task) = served_root(root).await?;
    complete_history(&first).await?;
    let others = tempfile::tempdir()?;
    let linked = others.path().join("linked");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            &linked.display().to_string(),
            "HEAD",
        ],
    );

    let (second, _second_task) = served_root(&linked).await?;
    let history = travelled_history(&second).await?;

    assert_eq!(
        history["complete"],
        json!(true),
        "the store already holds every shared commit: {history}"
    );
    assert_eq!(history["edges"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        store_files(&root.join(".git/.rift")).len(),
        1,
        "one store per repository"
    );
    assert!(store_files(&linked.join(".rift")).is_empty());
    first.cancel().await?;
    second.cancel().await?;
    Ok(())
}
