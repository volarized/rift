//! Drives change comparisons through SPARQL through a live rmcp client over a committed fixture
//! workspace: an introduced declaration, a removed one, a signature change, a comparison
//! against the working tree's uncommitted edits, and the refusals a comparison answers.

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
use rift_protocol::error::ErrorCode;
use rmcp::ServiceExt as _;
use serde_json::{Value, json};
use workspace_client::{
    TestResult, call_retrying_acceptance, captured_seed, failed_call, served_root, tool_request,
};

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
        hermetic_search::HERMETIC_TABLES,
    )?;
    write_all(
        directory.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='fixture'\nversion='0.0.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            (
                "src/lib.rs",
                "pub mod kept;\npub mod gone;\npub mod shifted;\npub mod added;\n",
            ),
        ],
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

fn results(structured: &Value) -> TestResult<&[Value]> {
    workspace_client::query::rows(structured)
}

/// Each hit's declaration name, its change kind, and the paths it names on both sides.
fn changes(structured: &Value) -> TestResult<Vec<(String, String, Value, Value)>> {
    results(structured)?
        .iter()
        .map(|row| {
            let optional = |field| {
                if row.get(field).is_some() {
                    workspace_client::query::literal(row, field).map(|value| json!(value))
                } else {
                    Ok(Value::Null)
                }
            };
            Ok((
                workspace_client::query::literal(row, "name")?.to_owned(),
                workspace_client::query::literal(row, "change_kind")?.to_owned(),
                optional("base_path")?,
                optional("head_path")?,
            ))
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
            &change_request(&json!({
                "change": {"base": "baseline", "head": "HEAD"},
                "include": ["source"]
            }))?,
        ),
    )
    .await?;

    assert_eq!(
        changes(&structured)?,
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
    for hit in results(&structured)? {
        assert_eq!(
            workspace_client::query::literal(hit, "matched_by")?,
            "change",
            "{hit}"
        );
        assert!(hit["score"].is_null(), "a comparison ranks nothing: {hit}");
    }
    let removed = &results(&structured)?[1];
    assert_eq!(
        workspace_client::query::literal(removed, "source")?,
        "pub fn gone() {}",
        "a removed declaration keeps its base-side source: {removed}"
    );
    assert_eq!(
        workspace_client::query::literal(removed, "path")?,
        "src/gone.rs",
        "{removed}"
    );

    client.cancel().await?;
    Ok(())
}

/// The working tree as `head` answers the uncommitted edits against `baseline`: the same
/// three declarations the committed head revision answers, with nothing committed.
#[tokio::test]
async fn search_change_against_the_working_tree_answers_uncommitted_edits() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::HERMETIC_TABLES,
    )?;
    write_all(directory.path(), BASELINE)?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    fs::remove_file(directory.path().join("src/gone.rs"))?;
    write_all(directory.path(), HEAD)?;
    let (client, _server_task) = served_root(directory.path()).await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &change_request(
                &json!({"change": {"base": "baseline", "head": {"kind": "working_tree"}}}),
            )?,
        ),
    )
    .await?;

    assert_eq!(
        changes(&structured)?,
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
    assert!(
        structured["warnings"].as_array().is_none_or(Vec::is_empty),
        "{structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// `head` defaults to `HEAD`, so a comparison naming `base` alone answers the same set.
#[tokio::test]
async fn search_change_with_base_alone_compares_against_head() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &change_request(&json!({"change": {"base": "baseline"}}))?,
        ),
    )
    .await?;

    let names: Vec<String> = changes(&structured)?.into_iter().map(|hit| hit.0).collect();
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
            &change_request(&json!({"change": {"base": "baseline"}, "target": "file"}))?,
        ),
    )
    .await?;

    assert!(results(&structured)?.is_empty(), "{structured}");

    client.cancel().await?;
    Ok(())
}

/// The refusals a comparison answers, each with the wire code the served surface carries.
#[tokio::test]
async fn search_change_refuses_the_requests_it_cannot_answer() -> TestResult {
    let (_directory, client, _server_task) = served_change_workspace().await?;

    let seed = captured_seed(&client, "kept", "src/kept.rs").await?;

    let refusals = [
        (
            json!({"change": {"base": "baseline"}, "rev": "main"}),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"change": {"base": "baseline"}, "query": "kept"}),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"language":"sparql", "query":format!("SELECT ?symbol {{ SERVICE :search {{ ?hit :change '{{\"base\":\"baseline\"}}'^^rdf:JSON; :symbol ?symbol }} <{seed}> :calls ?symbol }}")}),
            ErrorCode::CapabilityUnavailable,
        ),
        (
            json!({"change": {"base": "baseline"}, "scope": "all"}),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"change": {"base": "no-such-branch"}}),
            ErrorCode::ResourceNotFound,
        ),
    ];

    for (producer, code) in refusals {
        let arguments = if producer.get("language").is_some() {
            producer
        } else {
            change_request(&producer)?
        };
        let failure = failed_call(client.call_tool(tool_request("search", &arguments)).await)?;
        assert_eq!(
            failure.code, code,
            "arguments={arguments}, failure={failure:?}"
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
            hermetic_search::HERMETIC_TABLES
        ),
    )?;
    write_all(directory.path(), BASELINE)?;
    init(directory.path());
    commit_all(directory.path(), "baseline");
    git(directory.path(), &["tag", "baseline"]);
    let (client, _server_task) = served_root(directory.path()).await?;

    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &change_request(&json!({"change": {"base": "baseline"}}))?,
            ))
            .await,
    )?;
    assert_eq!(
        failure.code,
        ErrorCode::CapabilityUnavailable,
        "{failure:?}"
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
        hermetic_search::HERMETIC_TABLES,
    )?;
    write_all(directory.path(), BASELINE)?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())
        .await?
        .with_query_program(workspace_client::query_program()?);
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;

    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &change_request(&json!({"change": {"base": "main"}}))?,
            ))
            .await,
    )?;
    assert_eq!(
        failure.code,
        ErrorCode::CapabilityUnavailable,
        "{failure:?}"
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
        hermetic_search::HERMETIC_TABLES,
    )?;
    write_all(
        directory.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname='move_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
            ),
            ("src/lib.rs", "pub mod from;\npub mod to;\n"),
            ("src/from.rs", "pub fn travelled() {}\n"),
        ],
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
        tool_request(
            "search",
            &change_request(&json!({"change": {"base": "baseline"}}))?,
        ),
    )
    .await?;

    assert_eq!(
        changes(&structured)?,
        [(
            "travelled".to_owned(),
            "moved".to_owned(),
            json!("src/from.rs"),
            json!("src/to.rs")
        )],
        "{structured}"
    );
    assert_eq!(
        workspace_client::query::iri(&results(&structured)?[0], "symbol")?,
        captured_seed(&client, "travelled", "src/to.rs").await?,
        "{structured}"
    );

    client.cancel().await?;
    Ok(())
}

/// Change records remain typed projected fields on the public query answer.
fn change_request(parameters: &Value) -> TestResult<Value> {
    let parameters = serde_json::from_value(parameters.clone())?;
    Ok(serde_json::to_value(
        workspace_client::query::projected_retrieval(
            &parameters,
            "?change_kind ?base_path ?head_path ?matched_by",
            "OPTIONAL { ?hit :change ?change . ?change :kind ?change_kind . OPTIONAL { ?change :base_path ?base_path } OPTIONAL { ?change :head_path ?head_path } } OPTIONAL { ?hit :matched_by ?match . ?match :value ?matched_by } ",
        )?,
    )?)
}
