//! Drives `search`'s `traversal` block through a live rmcp client over a fixture workspace:
//! a callers walk, an indirect-caller walk, a path query, a hit reached both lexically and
//! by the walk, outgoing walks through call hierarchy, the refusals a walk with no edge
//! source draws, the warnings a walk carries when an engine could not answer in full, and
//! the changed files a publication hands the engines.
//!
//! A configured language engine resolves the references and calls the walk follows. Most
//! fixtures select the embedded `ty` engine over a Python workspace: no external process,
//! and the same lane a real read uses. Fake `sh` engines script the answers a real engine
//! gives while it loads or after an edit, and two suites drive rust-analyzer and
//! typescript-language-server themselves under `RIFT_ENGINE_LIVE`.

#[cfg(unix)]
mod fake_engine;
#[allow(
    dead_code,
    reason = "shared fixture global API exposes helpers this suite does not use"
)]
mod global_api;
mod hermetic_search;
#[path = "../../rift-lsp/tests/live_engine_gate.rs"]
mod live_engine_gate;
#[cfg(unix)]
#[path = "../../rift-lsp/tests/typescript_install.rs"]
mod typescript_install;
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

/// A `sh` engine that answers `initialize`, announces work it never ends, and
/// answers every later request with no location, so the session reads analyzing on
/// every attempt. Each start appends one line to the file its first argument names.
#[cfg(unix)]
const ANALYZING_ENGINE: &str = r#"echo start >> "$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// An incoming walk whose engine still reads analyzing when its wait is
/// spent answers with `engine_analysis_unavailable` and keeps the session, as an outgoing
/// walk does: the second walk meets the same engine process instead of starting a
/// replacement.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_past_the_readiness_timeout_warns_and_keeps_the_engine() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let starts = engine.path().join("starts.log");
    std::fs::write(&script, ANALYZING_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        starts.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[(
            "lib.rs",
            "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
        )],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "beacon"}))).await?;
    let started_lines = || std::fs::read_to_string(&starts).map_or(0, |text| text.lines().count());
    let mut starts_after = Vec::new();
    for _walk in 0..2 {
        let started = std::time::Instant::now();
        let structured = call_retrying_acceptance(
            &client,
            tool_request(
                "search",
                &json!({"traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}}),
            ),
        )
        .await?;
        let elapsed = started.elapsed();
        eprintln!("incoming walk: elapsed={elapsed:?} answer={structured}");
        assert!(results(&structured).is_empty(), "{structured}");
        let warning = &structured["warnings"][0];
        assert_eq!(
            warning["code"],
            json!("engine_analysis_unavailable"),
            "{structured}"
        );
        assert!(
            warning["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("readiness_timeout")),
            "{structured}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
        starts_after.push(started_lines());
    }
    eprintln!("incoming: engine starts after each walk={starts_after:?}");
    assert_eq!(
        starts_after,
        [1, 1],
        "both walks meet the one engine process"
    );

    client.cancel().await?;
    Ok(())
}

/// A fake engine that registers one `**/*.rs` file watcher once initialized and appends
/// every `workspace/didChangeWatchedFiles` body it receives to the log named by `$1`. It
/// answers every request with an empty list.
const WATCHING_ENGINE: &str = r#"log="$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}" ;;
    *'"method":"initialized"'*)
      frame '{"jsonrpc":"2.0","id":"watch","method":"client/registerCapability","params":{"registrations":[{"id":"watch-rust","method":"workspace/didChangeWatchedFiles","registerOptions":{"watchers":[{"globPattern":"**/*.rs"}]}}]}}' ;;
    *'"method":"workspace/didChangeWatchedFiles"'*)
      printf '%s\n' "$body" >> "$log" ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// A publication's changed files reach the live engine session as one classified
/// `workspace/didChangeWatchedFiles` batch before the next walk asks it anything. The
/// edit touches files no walk opens: one modified, one created, one deleted.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_tells_the_engine_of_files_changed_outside_the_walk() -> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let notified = engine.path().join("notified.log");
    std::fs::write(&script, WATCHING_ENGINE)?;
    let configuration = format!(
        "[languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n\
         retry = {{ attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }}\n",
        script.display(),
        notified.display()
    );
    let (directory, client, _server_task) = served_workspace(
        &[
            (
                "lib.rs",
                "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
            ),
            ("other.rs", "pub fn other_marker() {}\n"),
            ("gone.rs", "pub fn gone_marker() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let walk = || {
        call_retrying_acceptance(
            &client,
            tool_request(
                "search",
                &json!({"traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}}),
            ),
        )
    };
    walk().await?;
    assert!(!notified.exists(), "a new session is told nothing");

    std::fs::write(
        directory.path().join("other.rs"),
        "pub fn other_marker() {}\n\npub fn other_marker_two() {}\n",
    )?;
    std::fs::write(
        directory.path().join("added.rs"),
        "pub fn added_marker() {}\n",
    )?;
    std::fs::remove_file(directory.path().join("gone.rs"))?;
    let edited = std::time::Instant::now();
    for (name, present) in [
        ("other_marker_two", true),
        ("added_marker", true),
        ("gone_marker", false),
    ] {
        published_holds(&client, name, present).await?;
    }
    let published = edited.elapsed();
    walk().await?;
    let fed = edited.elapsed();

    let text = std::fs::read_to_string(&notified)?;
    let mut changes: Vec<(String, u64)> = Vec::new();
    for line in text.lines() {
        let body: Value = serde_json::from_str(line)?;
        for change in body["params"]["changes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let uri = change["uri"].as_str().unwrap_or_default();
            let name = uri.rsplit('/').next().unwrap_or_default().to_owned();
            changes.push((name, change["type"].as_u64().unwrap_or_default()));
        }
    }
    eprintln!(
        "feed: published after {published:?}, walk answered after {fed:?}, \
         batches={} changes={changes:?}",
        text.lines().count()
    );
    changes.sort();
    assert_eq!(
        changes,
        vec![
            ("added.rs".to_owned(), 1),
            ("gone.rs".to_owned(), 3),
            ("other.rs".to_owned(), 2),
        ],
        "created, deleted, and changed reach the engine once each: {text}"
    );

    client.cancel().await?;
    Ok(())
}

/// The rift-lsp live Rust fixture, served with rust-analyzer 1.98 through rustup.
const LIVE_RUST_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        include_str!("../../rift-lsp/tests/fixtures/rust/Cargo.toml"),
    ),
    (
        "lib.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/lib.rs"),
    ),
    (
        "hub.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/hub.rs"),
    ),
    (
        "walk.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/walk.rs"),
    ),
    (
        "caller.rs",
        include_str!("../../rift-lsp/tests/fixtures/rust/caller.rs"),
    ),
];

const LIVE_RUST_ENGINE: &str =
    "[languages.rust.lsp]\ncommand = [\"rustup\", \"run\", \"1.98\", \"rust-analyzer\"]\n";

/// Walks from `seed` in `direction` until the reached names equal `expected`, at most
/// [`LIVE_WALK_ATTEMPTS_MAX`] times, printing each answer with its wall time. Returns
/// the attempt that matched and its answer.
async fn live_walk(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    label: &str,
    seed: &str,
    direction: &str,
    expected: &[&str],
) -> TestResult<(usize, Value)> {
    for attempt in 1..=LIVE_WALK_ATTEMPTS_MAX {
        let started = std::time::Instant::now();
        let structured = call_retrying_acceptance(
            client,
            tool_request(
                "search",
                &json!({"traversal": {"seed": seed, "direction": direction}}),
            ),
        )
        .await?;
        let reached = symbol_names(&structured);
        eprintln!(
            "live {label} attempt {attempt}: elapsed={:?} reached={reached:?} warnings={}",
            started.elapsed(),
            structured["warnings"]
        );
        if reached == expected {
            return Ok((attempt, structured));
        }
    }
    Err(format!("{label} never answered {expected:?}").into())
}

/// Walks one live answer may take while rust-analyzer loads the fixture.
const LIVE_WALK_ATTEMPTS_MAX: usize = 10;

/// The first walk on a cold rust-analyzer answers the callee on its first request, since
/// the walk waits out the load instead of taking the empty prepare rust-analyzer answers
/// while loading. rust-analyzer runs in client watcher mode, so it learns of a file no read
/// opens only through the server's feed. `beacon` becomes `beacon2` in hub.rs, below a
/// new `helper`, and caller.rs gains `again`. The outgoing walk from `larger` opens only
/// walk.rs, and the incoming read of `beacon2` opens only hub.rs.
#[tokio::test]
async fn live_rust_analyzer_walks_answer_a_rename_in_an_unopened_file() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let (directory, client, _server_task) =
        served_workspace(LIVE_RUST_FILES, Some(LIVE_RUST_ENGINE.to_owned())).await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "beacon"}))).await?;
    let larger = "rift://symbol/rust/walk.rs/larger";
    let (cold, answer) =
        live_walk(&client, "outgoing before", larger, "outgoing", &["beacon"]).await?;
    assert_eq!(
        cold, 1,
        "the first walk waits out rust-analyzer's load instead of taking the empty prepare \
         it answers while loading as final"
    );
    let dropped = answer["warnings"]
        .as_array()
        .and_then(|warnings| {
            warnings
                .iter()
                .find(|warning| warning["code"] == json!("callees_dropped"))
        })
        .ok_or("the walk counts the callees it dropped")?;
    assert_eq!(
        dropped["callees"],
        json!(2),
        "`max` and `len` sit in the standard library: {answer}"
    );
    live_walk(
        &client,
        "incoming before",
        "rift://symbol/rust/hub.rs/beacon",
        "incoming",
        &["larger", "total"],
    )
    .await?;
    eprintln!("live: warm after {:?}", started.elapsed());

    std::fs::write(
        directory.path().join("hub.rs"),
        "pub fn helper() -> i32 {\n    0\n}\n\npub fn beacon2(value: i32) -> i32 {\n    value\n}\n",
    )?;
    std::fs::write(
        directory.path().join("walk.rs"),
        include_str!("../../rift-lsp/tests/fixtures/rust/walk.rs").replace("beacon", "beacon2"),
    )?;
    std::fs::write(
        directory.path().join("caller.rs"),
        "use crate::hub::beacon2;\n\npub fn total() -> i32 {\n    beacon2(2)\n}\n\n\
         pub fn again() -> i32 {\n    beacon2(3)\n}\n",
    )?;
    let edited = std::time::Instant::now();
    for (name, present) in [("beacon2", true), ("again", true), ("beacon", false)] {
        published_holds(&client, name, present).await?;
    }
    eprintln!("live: published after {:?}", edited.elapsed());
    let (outgoing, _) =
        live_walk(&client, "outgoing after", larger, "outgoing", &["beacon2"]).await?;
    eprintln!("live: outgoing fresh after {:?}", edited.elapsed());
    let (incoming, _) = live_walk(
        &client,
        "incoming after",
        "rift://symbol/rust/hub.rs/beacon2",
        "incoming",
        &["again", "larger", "total"],
    )
    .await?;
    eprintln!("live: incoming fresh after {:?}", edited.elapsed());
    assert_eq!(
        (outgoing, incoming),
        (1, 1),
        "both answer the edit on the first walk"
    );

    client.cancel().await?;
    Ok(())
}

/// The rift-lsp live TypeScript fixture: `larger` in walk.ts calls `beacon` from hub.ts and
/// `Math.max` from TypeScript's own lib, and `total` and the TSX `Banner` call `beacon` too.
#[cfg(unix)]
const LIVE_TYPESCRIPT_FILES: &[(&str, &str)] = &[
    (
        "tsconfig.json",
        include_str!("../../rift-lsp/tests/fixtures/typescript/tsconfig.json"),
    ),
    (
        "hub.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/hub.ts"),
    ),
    (
        "caller.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/caller.ts"),
    ),
    (
        "view.tsx",
        include_str!("../../rift-lsp/tests/fixtures/typescript/view.tsx"),
    ),
    (
        "walk.ts",
        include_str!("../../rift-lsp/tests/fixtures/typescript/walk.ts"),
    ),
];

/// The typescript-language-server the fixture's lockfile pins, run from the workspace's
/// own `node_modules` for both dialects, with that folder left out of the index.
#[cfg(unix)]
fn live_typescript_engine() -> String {
    format!(
        "[source]\nexclude = [\"node_modules/**\"]\n\n\
         [languages.typescript]\nlsp = \"typescript\"\n\n\
         [languages.\"typescript:tsx\"]\nlsp = \"typescript\"\n\n\
         [lsp.typescript]\ncommand = [\"{}\", \"--stdio\"]\n\n\
         [lsp.typescript.initialization_options.tsserver]\nuseSyntaxServer = \"never\"\n",
        typescript_install::LANGUAGE_SERVER_PROGRAM
    )
}

/// typescript-language-server answers both walk directions through `search`, each on its
/// first walk, since the walk waits out the project load. The outgoing walk from `larger`
/// reaches `beacon`, and addresses `Math.max` in the `typescript` package the workspace
/// installs, whose lib declares it: with the global API off it drops, and the answer names
/// why. The incoming walk to `beacon` reaches `larger`, `total`, and the TSX `Banner`.
#[cfg(unix)]
#[tokio::test]
async fn live_typescript_language_server_walks_both_directions() -> TestResult {
    if !live_engine_gate::engine_live() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let mut files = typescript_install::typescript_package_files().to_vec();
    files.extend_from_slice(LIVE_TYPESCRIPT_FILES);
    let (_directory, client, _server_task) = workspace_client::served_prepared_workspace(
        &files,
        Some(live_typescript_engine()),
        typescript_install::install_typescript_engine,
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "beacon"}))).await?;
    let (outgoing, answer) = live_walk(
        &client,
        "typescript outgoing",
        "rift://symbol/typescript/walk.ts/larger",
        "outgoing",
        &["beacon"],
    )
    .await?;
    eprintln!("live typescript: outgoing after {:?}", started.elapsed());
    let hop = &results(&answer)[0]["traversal_path"][0];
    assert_eq!(hop["direction"], json!("outgoing"), "{answer}");
    assert_eq!(hop["relationship"]["facets"], json!(["calls"]), "{answer}");
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("the walk counts the callee it dropped")?;
    assert_eq!(warnings.len(), 2, "{answer}");
    assert_eq!(warnings[0]["code"], json!("callees_dropped"), "{answer}");
    assert_eq!(
        warnings[0]["callees"],
        json!(1),
        "`Math.max` sits in TypeScript's own lib: {answer}"
    );
    assert_eq!(
        warnings[1],
        json!({"code": "global_access_disabled"}),
        "the lib is a package file the global API names: {answer}"
    );
    let (incoming, answer) = live_walk(
        &client,
        "typescript incoming",
        "rift://symbol/typescript/hub.ts/beacon",
        "incoming",
        &["Banner", "larger", "total"],
    )
    .await?;
    eprintln!("live typescript: incoming after {:?}", started.elapsed());
    for hit in results(&answer) {
        let hop = &hit["traversal_path"][0];
        assert_eq!(hop["direction"], json!("incoming"), "{hit}");
        assert_eq!(
            hop["relationship"]["facets"],
            json!(["references"]),
            "{hit}"
        );
    }
    assert!(answer["warnings"].is_null(), "{answer}");
    assert_eq!(
        (outgoing, incoming),
        (1, 1),
        "both walks answer on the first request"
    );

    client.cancel().await?;
    Ok(())
}

/// Waits until the current publication holds `name` as a symbol, or no longer does.
async fn published_holds(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    present: bool,
) -> TestResult {
    for _attempt in 0..PUBLICATION_ATTEMPTS_MAX {
        let structured =
            call_retrying_acceptance(client, tool_request("search", &json!({"query": name})))
                .await?;
        if symbol_names(&structured).iter().any(|held| held == name) == present {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(format!("the publication never settled on {name} present={present}").into())
}

/// Searches one edit may take before its publication is current: 10 s at 50 ms apart.
const PUBLICATION_ATTEMPTS_MAX: usize = 200;

/// The embedded `ty` engine under the default retry table and `settle_delay`: it declares
/// itself ready at its start, so a walk takes its first answer.
const OUTGOING_ENGINE: &str = "[languages.python.lsp]\nembedded = \"ty\"\n";

const ROOT: &str = "rift://symbol/python/graph.py/root";

/// [`CALL_GRAPH_FILES`] plus a module that calls into the standard library and holds a
/// class whose body makes a call.
const OUTGOING_FILES: &[(&str, &str)] = &[
    CALL_GRAPH_FILES[0],
    (
        "extra.py",
        "from graph import leaf\n\nLIMIT = 3\n\n\
         def counted() -> int:\n    return len([leaf()]) + LIMIT\n\n\
         class Holder:\n    size = counted()\n\n    def grow(self) -> int:\n        return root_call()\n\n\
         def root_call() -> int:\n    return 1\n",
    ),
];

#[tokio::test]
async fn search_traversal_outgoing_depth_one_reaches_the_direct_callees() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;

    let started = std::time::Instant::now();
    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({ "traversal": { "seed": ROOT, "direction": "outgoing" } }),
        ),
    )
    .await?;
    eprintln!("outgoing depth 1: {:?}", started.elapsed());

    assert_eq!(
        symbol_names(&structured),
        ["branch_a", "branch_b"],
        "{structured}"
    );
    for hit in results(&structured) {
        assert_eq!(hit["distance"], json!(1), "{hit}");
        let hop = &hit["traversal_path"][0];
        assert_eq!(hop["direction"], json!("outgoing"), "{hit}");
        assert_eq!(hop["relationship"]["from"], json!(ROOT), "{hit}");
        assert_eq!(hop["relationship"]["facets"], json!(["calls"]), "{hit}");
    }
    assert!(structured["warnings"].is_null(), "{structured}");

    client.cancel().await?;
    Ok(())
}

#[tokio::test]
async fn search_traversal_outgoing_depth_two_reaches_the_callees_of_the_callees() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;

    let started = std::time::Instant::now();
    let structured = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({ "traversal": { "seed": ROOT, "direction": "outgoing", "depth": 2 } }),
        ),
    )
    .await?;
    eprintln!("outgoing depth 2: {:?}", started.elapsed());

    assert_eq!(
        symbol_names(&structured),
        ["branch_a", "branch_b", "leaf"],
        "{structured}"
    );
    let leaf = results(&structured)
        .into_iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == json!("leaf"))
        .ok_or("leaf must be a hit")?;
    assert_eq!(leaf["distance"], json!(2), "{leaf}");
    assert_eq!(
        leaf["traversal_path"][1]["relationship"]["to"],
        json!(LEAF),
        "{leaf}"
    );

    let only_leaf = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": { "seed": ROOT, "direction": "outgoing", "depth": 2, "to": LEAF }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&only_leaf), ["leaf"], "{only_leaf}");

    client.cancel().await?;
    Ok(())
}

/// `query` and an outgoing walk merge as an incoming walk does, and facets filter on
/// `calls`: `references` has no outgoing lane, so asking for it alone refuses and beside
/// `calls` it warns.
#[tokio::test]
async fn search_traversal_outgoing_merges_with_query_and_filters_on_calls() -> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(CALL_GRAPH_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;

    let merged = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "query": "branch_a",
                "target": "symbol",
                "traversal": { "seed": ROOT, "direction": "outgoing", "facets": ["calls"] }
            }),
        ),
    )
    .await?;
    let hit = results(&merged)
        .into_iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == json!("branch_a"))
        .ok_or("branch_a must be a hit")?;
    let matched_by = hit["matched_by"].as_array().ok_or("matched_by")?;
    assert!(matched_by.contains(&json!("name")), "{hit}");
    assert!(matched_by.contains(&json!("relationship")), "{hit}");
    assert!(merged["warnings"].is_null(), "{merged}");

    let warned = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": ROOT, "direction": "outgoing", "facets": ["references", "calls"]
                }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&warned), ["branch_a", "branch_b"], "{warned}");
    assert_eq!(
        warned["warnings"][0]["code"],
        json!("relationship_coverage_missing")
    );
    assert_eq!(warned["warnings"][0]["facets"], json!(["references"]));

    let code = refusal_code(
        &client,
        &json!({"traversal": {"seed": ROOT, "direction": "outgoing", "facets": ["references"]}}),
    )
    .await?;
    assert_eq!(code, json!("capability_unavailable"));

    client.cancel().await?;
    Ok(())
}

/// With `[global] enabled = false`, a standard library callee (`len`, answered in ty's
/// vendored `builtins.pyi`) is named by no declaration, so its edge drops, the answer counts
/// it in `callees_dropped` beside the warning naming the global API off, and the project
/// callee stays; a class seed answers the calls in its own body alone (`counted`, not
/// `root_call` inside `grow`).
#[tokio::test]
async fn search_traversal_outgoing_drops_a_standard_library_callee_and_walks_a_class_body()
-> TestResult {
    let (_directory, client, _server_task) =
        served_workspace(OUTGOING_FILES, Some(OUTGOING_ENGINE.to_owned())).await?;

    let counted = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/python/extra.py/counted", "direction": "outgoing"
                }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&counted), ["leaf"], "{counted}");
    let warnings = counted["warnings"]
        .as_array()
        .ok_or("a dropped callee warns")?;
    assert_eq!(warnings.len(), 2, "{counted}");
    assert_eq!(warnings[0]["code"], json!("callees_dropped"), "{counted}");
    assert_eq!(warnings[0]["callees"], json!(1), "{counted}");
    assert_eq!(
        warnings[1],
        json!({"code": "global_access_disabled"}),
        "{counted}"
    );

    let holder = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/python/extra.py/Holder", "direction": "outgoing"
                }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&holder), ["counted"], "{holder}");
    assert!(
        holder["warnings"].is_null(),
        "every callee the class body names is in the project: {holder}"
    );

    client.cancel().await?;
    Ok(())
}

/// Once the engine reads ready, an empty prepare at the seed refuses, naming the seed's
/// kind; the incoming walk's other refusals stay: a walk beside `change` refuses.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_from_a_seed_without_a_call_hierarchy_item_refuses() -> TestResult
{
    let (_engine, configuration) = fake_engine::rust_engine(fake_engine::READY_UNPREPARED_ENGINE)?;
    let (_directory, client, _server_task) = served_workspace(
        &[("lib.rs", "pub struct Beacon;\n\npub fn caller() {}\n")],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "Beacon"}))).await?;

    let error = client
        .call_tool(tool_request(
            "search",
            &json!({
                "traversal": { "seed": "rift://symbol/rust/lib.rs/Beacon", "direction": "outgoing" }
            }),
        ))
        .await
        .expect_err("the ready engine prepares no call hierarchy item at a struct");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };
    eprintln!("refusal: message={:?} data={:?}", error.message, error.data);
    let data = error.data.clone().unwrap_or_default();
    assert_eq!(data["code"], json!("capability_unavailable"), "{error:?}");
    assert!(error.message.contains("of kind `struct`"), "{error:?}");

    let code = refusal_code(
        &client,
        &json!({
            "change": {"base": "baseline"},
            "traversal": {"seed": "rift://symbol/rust/lib.rs/caller", "direction": "outgoing"}
        }),
    )
    .await?;
    assert_eq!(code, json!("capability_unavailable"));

    client.cancel().await?;
    Ok(())
}

/// The `PROGRESS` step of an engine that begins and ends its work at start, so the session
/// reads it ready once quiet.
#[cfg(unix)]
const ANNOUNCED_WORK: &str = r#"frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}'
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"end"}}}'"#;

/// A line past the end of `lib.rs`, which no served byte holds.
#[cfg(unix)]
const UNSERVED_LINES: fake_engine::ScriptedLines = fake_engine::ScriptedLines {
    callee: 50,
    caller: 50,
};

/// An engine that announces no work reads unconfirmed, so a walk in either direction takes
/// its answer once the session read it quiet past `settle_delay`, and the answer names the
/// engine in `engine_readiness_unconfirmed`.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_over_an_engine_that_announces_nothing_warns_readiness_unconfirmed()
-> TestResult {
    let (_engine, (_directory, client, _server_task)) = fake_engine::scripted_calls_workspace(
        fake_engine::UNANNOUNCED_WORK,
        fake_engine::SERVED_LINES,
    )
    .await?;
    for (seed, direction, reached) in [
        ("rift://symbol/rust/lib.rs/caller", "outgoing", "beacon"),
        ("rift://symbol/rust/lib.rs/beacon", "incoming", "caller"),
    ] {
        let structured = call_retrying_acceptance(
            &client,
            tool_request(
                "search",
                &json!({"traversal": {"seed": seed, "direction": direction}}),
            ),
        )
        .await?;
        assert_eq!(symbol_names(&structured), [reached], "{structured}");
        let warnings = structured["warnings"]
            .as_array()
            .ok_or("an unconfirmed engine warns")?;
        assert_eq!(warnings.len(), 1, "{structured}");
        assert_eq!(
            warnings[0]["code"],
            json!("engine_readiness_unconfirmed"),
            "{structured}"
        );
        assert_eq!(warnings[0]["processes"], json!(["rust"]), "{structured}");
    }

    client.cancel().await?;
    Ok(())
}

/// An engine answer naming a line the served `lib.rs` does not hold drops the engine's
/// contribution with `engine_analysis_unavailable`, in either direction. The indexed store
/// holds no edge, and the walk still answers with the warning instead of refusing.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_over_an_unmappable_engine_answer_warns_in_both_directions() -> TestResult
{
    let (_engine, (_directory, client, _server_task)) =
        fake_engine::scripted_calls_workspace(ANNOUNCED_WORK, UNSERVED_LINES).await?;
    for (seed, direction) in [
        ("rift://symbol/rust/lib.rs/caller", "outgoing"),
        ("rift://symbol/rust/lib.rs/beacon", "incoming"),
    ] {
        let structured = call_retrying_acceptance(
            &client,
            tool_request(
                "search",
                &json!({"traversal": {"seed": seed, "direction": direction}}),
            ),
        )
        .await?;
        assert!(results(&structured).is_empty(), "{structured}");
        let warnings = structured["warnings"]
            .as_array()
            .ok_or("the dropped contribution warns")?;
        assert_eq!(warnings.len(), 1, "{structured}");
        assert_eq!(
            warnings[0]["code"],
            json!("engine_analysis_unavailable"),
            "{structured}"
        );
        assert_eq!(warnings[0]["language"], json!("rust"), "{structured}");
        assert!(
            warnings[0]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("line_out_of_range")),
            "the engine answered, about a line the served bytes do not hold: {structured}"
        );
    }

    client.cancel().await?;
    Ok(())
}

/// A `sh` engine serving references and call hierarchy that announces work it never
/// ends; each start appends one line to the file its first argument names.
#[cfg(unix)]
const ANALYZING_CALL_HIERARCHY_ENGINE: &str = r#"echo start >> "$1"
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=$(printf '%s' "$header" | tr -dc 0-9)
  body=$(dd bs=1 count="$length" 2>/dev/null)
  id=$(printf '%s' "$body" | grep -o '"id":[0-9]*' | head -1 | tr -dc 0-9)
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true,\"callHierarchyProvider\":true}}}"
      frame '{"jsonrpc":"2.0","method":"$/progress","params":{"token":"fake/analysis","value":{"kind":"begin","title":"analysis"}}}' ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[]}" ;;
  esac
done
"#;

/// An outgoing walk whose engine still reads analyzing when its wait is spent
/// answers with `engine_analysis_unavailable` and keeps the session: the second walk
/// meets the same engine process instead of starting a replacement.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_past_the_readiness_timeout_warns_and_keeps_the_engine()
-> TestResult {
    let engine = tempfile::tempdir()?;
    let script = engine.path().join("engine.sh");
    let starts = engine.path().join("starts.log");
    std::fs::write(&script, ANALYZING_CALL_HIERARCHY_ENGINE)?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [languages.rust.lsp]\ncommand = [\"sh\", \"{}\", \"{}\"]\n",
        script.display(),
        starts.display()
    );
    let (_directory, client, _server_task) = served_workspace(
        &[(
            "lib.rs",
            "pub fn beacon() {}\n\npub fn caller() {\n    beacon();\n}\n",
        )],
        Some(configuration),
    )
    .await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "caller"}))).await?;
    let started_lines = || std::fs::read_to_string(&starts).map_or(0, |text| text.lines().count());
    let mut starts_after = Vec::new();
    for _walk in 0..2 {
        let started = std::time::Instant::now();
        let structured = call_retrying_acceptance(
            &client,
            tool_request(
                "search",
                &json!({"traversal": {
                    "seed": "rift://symbol/rust/lib.rs/caller", "direction": "outgoing"
                }}),
            ),
        )
        .await?;
        let elapsed = started.elapsed();
        eprintln!("outgoing walk: elapsed={elapsed:?} answer={structured}");
        assert!(results(&structured).is_empty(), "{structured}");
        let warning = &structured["warnings"][0];
        assert_eq!(
            warning["code"],
            json!("engine_analysis_unavailable"),
            "{structured}"
        );
        assert!(
            warning["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("readiness_timeout")),
            "{structured}"
        );
        assert!(elapsed < std::time::Duration::from_secs(2), "{elapsed:?}");
        starts_after.push(started_lines());
    }
    assert_eq!(
        starts_after,
        [1, 1],
        "both walks meet the one engine process"
    );

    client.cancel().await?;
    Ok(())
}

/// The relationship graph serves the project alone, and package facts come from the
/// global index, so a `global` search leaves a walk nothing to run over and refuses naming
/// `traversal`.
#[tokio::test]
async fn a_global_search_with_traversal_refuses_invalid_request() -> TestResult {
    let (_directory, client, _server_task) = served_workspace(ENGINELESS_FILES, None).await?;
    call_retrying_acceptance(&client, tool_request("search", &json!({"query": "beacon"}))).await?;

    let error = client
        .call_tool(tool_request(
            "search",
            &json!({
                "query": "beacon",
                "scope": "global",
                "traversal": {"seed": "rift://symbol/rust/lib.rs/beacon"}
            }),
        ))
        .await
        .expect_err("the server must refuse a global walk");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };

    let wire = error.data.ok_or("a refusal carries its wire data")?;
    assert_eq!(wire["code"], json!("invalid_request"), "{wire:#}");
    assert!(
        error.message.contains("traversal"),
        "the refusal names the field: {}",
        error.message
    );

    client.cancel().await?;
    Ok(())
}

/// The `[global]` table pointing at `endpoint`, with bounds a loopback fixture meets.
fn global_table(endpoint: &str) -> String {
    format!(
        "[global]\nenabled = true\nendpoint = \"{endpoint}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"5s\"\n"
    )
}

/// The fixture global API holding the Python collection the callee walks name.
async fn python_global_api() -> TestResult<global_api::GlobalFixture> {
    Ok(
        global_api::GlobalFixture::start_with(global_api::FixtureOptions {
            python_collection: true,
            ..global_api::FixtureOptions::default()
        })
        .await?,
    )
}

/// The declaration requests the fixture received.
async fn declaration_requests(fixture: &global_api::GlobalFixture) -> Vec<Value> {
    fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.ends_with("/declarations"))
        .filter_map(|request| request.body)
        .collect()
}

/// The hit whose symbol `name` names, with its identity, unit, and the edge reaching it.
fn callee_hit(structured: &Value, name: &str) -> TestResult<Value> {
    results(structured)
        .into_iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == json!(name))
        .ok_or_else(|| format!("{name} must be a hit: {structured}").into())
}

/// A standard library callee's position goes to the global API as `stdlib/python` at the
/// release the resolution served, at typeshed's own path, and the answered declaration ends
/// the edge: the callee is a hit addressed by its unit, and nothing drops.
#[tokio::test]
async fn search_traversal_outgoing_names_a_standard_library_callee_by_its_global_id() -> TestResult
{
    let fixture = python_global_api().await?;
    let configuration = format!("{}\n{OUTGOING_ENGINE}", global_table(&fixture.endpoint));
    let (_directory, client, _server_task) =
        served_workspace(OUTGOING_FILES, Some(configuration)).await?;

    let counted = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/python/extra.py/counted", "direction": "outgoing"
                }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&counted), ["leaf", "len"], "{counted}");
    let len = callee_hit(&counted, "len")?;
    let id = "rift://symbol/python/stdlib/python@3.12.9/builtins.pyi/len";
    assert_eq!(len["hit"]["symbol"]["id"], json!(id), "{len}");
    assert_eq!(len["hit"]["symbol"]["kind"], json!("function"), "{len}");
    assert_eq!(
        len["hit"]["symbol"]["origin"],
        json!({"location": "stdlib", "source_kind": "authored"}),
        "{len}"
    );
    assert_eq!(
        len["unit"],
        json!("rift://source/stdlib/python@3.12.9/builtins.pyi"),
        "{len}"
    );
    assert!(len["path"].is_null() && len["range"].is_null(), "{len}");
    assert_eq!(len["distance"], json!(1), "{len}");
    let hop = &len["traversal_path"][0]["relationship"];
    assert_eq!(hop["from"], json!("rift://symbol/python/extra.py/counted"));
    assert_eq!(hop["to"], json!(id), "{len}");
    assert_eq!(hop["facets"], json!(["calls"]), "{len}");
    assert!(counted["warnings"].is_null(), "{counted}");

    let requests = declaration_requests(&fixture).await;
    assert_eq!(requests.len(), 1, "one request per walk: {requests:?}");
    let positions = requests[0]["positions"]
        .as_array()
        .ok_or("the request carries positions")?;
    assert_eq!(positions.len(), 1, "{requests:?}");
    assert_eq!(
        positions[0]["package"],
        json!({"manager": "stdlib", "name": "python", "version": "3.12.9"})
    );
    assert_eq!(positions[0]["path"], json!("builtins.pyi"));

    client.cancel().await?;
    Ok(())
}

/// `greeting` 1.0.0 as `uv sync` installs it into the workspace's own `.venv`, with the
/// lockfile naming it and a module calling both its functions.
#[cfg(unix)]
const INSTALLED_FILES: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        "[project]\nname = \"app\"\nversion = \"0.1.0\"\ndependencies = [\"greeting\"]\n",
    ),
    (
        "uv.lock",
        "version = 1\n\n\
         [[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = { editable = \".\" }\n\n\
         [[package]]\nname = \"greeting\"\nversion = \"1.0.0\"\n\
         source = { registry = \"https://pypi.org/simple\" }\n",
    ),
    (
        ".venv/pyvenv.cfg",
        "home = /usr/bin\nversion_info = 3.12.4\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting/__init__.py",
        "from greeting.core import greet, quiet\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting/core.py",
        "def greet(name: str) -> str:\n    return name\n\n\ndef quiet() -> None:\n    return None\n",
    ),
    (
        ".venv/lib/python3.12/site-packages/greeting-1.0.0.dist-info/RECORD",
        "greeting/__init__.py,,\ngreeting/core.py,,\ngreeting-1.0.0.dist-info/RECORD,,\n",
    ),
    (
        "app.py",
        "from greeting.core import greet, quiet\n\n\n\
         def hello() -> str:\n    quiet()\n    return greet(\"rift\")\n",
    ),
];

/// A dependency callee's file below the package's import folder in `.venv` addresses the
/// package at its locked version, at its installed path, so the global API names `greet`;
/// `quiet`, a position it answers no declaration at, drops and is counted.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_names_a_dependency_callee_and_counts_one_answered_none()
-> TestResult {
    let fixture = python_global_api().await?;
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n{OUTGOING_ENGINE}",
        global_table(&fixture.endpoint)
    );
    let (_directory, client, _server_task) =
        served_workspace(INSTALLED_FILES, Some(configuration)).await?;

    let hello = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": { "seed": "rift://symbol/python/app.py/hello", "direction": "outgoing" }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&hello), ["greet"], "{hello}");
    let greet = callee_hit(&hello, "greet")?;
    assert_eq!(
        greet["hit"]["symbol"]["id"],
        json!("rift://symbol/python/pypi/greeting@1.0.0/greeting/core.py/greet"),
        "{greet}"
    );
    assert_eq!(greet["hit"]["symbol"]["kind"], json!("function"), "{greet}");
    assert_eq!(
        greet["hit"]["symbol"]["origin"],
        json!({
            "location": "dependency",
            "package": {"manager": "pypi", "name": "greeting", "version": "1.0.0"},
            "source_kind": "authored"
        }),
        "{greet}"
    );
    assert_eq!(
        greet["unit"],
        json!("rift://source/pypi/greeting@1.0.0/greeting/core.py"),
        "{greet}"
    );
    let warnings = hello["warnings"]
        .as_array()
        .ok_or("a dropped callee warns")?;
    assert_eq!(
        warnings.as_slice(),
        [json!({
            "code": "callees_dropped",
            "callees": 1,
            "detail": "the language engine named callees outside the project and every \
                       installed package, or at a position the global index answered no \
                       declaration at, so the walk carries no edge to them"
        })],
        "{hello}"
    );

    let requests = declaration_requests(&fixture).await;
    let mut asked: Vec<(String, String, u64)> = requests
        .iter()
        .flat_map(|request| request["positions"].as_array().cloned().unwrap_or_default())
        .map(|position| {
            (
                position["package"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                position["path"].as_str().unwrap_or_default().to_owned(),
                position["line"].as_u64().unwrap_or(u64::MAX),
            )
        })
        .collect();
    asked.sort();
    assert_eq!(
        asked,
        [
            ("greeting".to_owned(), "greeting/core.py".to_owned(), 0),
            ("greeting".to_owned(), "greeting/core.py".to_owned(), 4),
        ],
        "each callee's position at its installed path, in one request: {requests:?}"
    );
    assert_eq!(requests.len(), 1, "one request per walk: {requests:?}");

    client.cancel().await?;
    Ok(())
}

/// `greeting` 1.0.0 installed into `.venv` after the first walk, as `uv sync` installs
/// it from the unchanged `uv.lock`: the files `INSTALLED_FILES` holds below
/// `site-packages`.
#[cfg(unix)]
fn installed_after(path: &str) -> bool {
    path.starts_with(".venv/lib/python3.12/site-packages/greeting")
}

/// A distribution `uv sync` installs into the existing `.venv` without touching `uv.lock`
/// reaches the next walk: the engine resolves the import, and the dependency context
/// locates the package it lands in, so the callee is named rather than dropped.
#[cfg(unix)]
#[tokio::test]
async fn search_traversal_outgoing_names_a_callee_installed_after_the_first_walk() -> TestResult {
    let fixture = python_global_api().await?;
    let configuration = format!(
        "[source]\nexclude = [\".venv/**\"]\n\n{}\n{OUTGOING_ENGINE}",
        global_table(&fixture.endpoint)
    );
    let mut before: Vec<(&str, &str)> = INSTALLED_FILES
        .iter()
        .copied()
        .filter(|(path, _)| !installed_after(path))
        .collect();
    before.push((".venv/lib/python3.12/site-packages/_virtualenv.py", ""));
    let (directory, client, _server_task) = served_workspace(&before, Some(configuration)).await?;
    let walk = json!({
        "traversal": { "seed": "rift://symbol/python/app.py/hello", "direction": "outgoing" }
    });

    let first = call_retrying_acceptance(&client, tool_request("search", &walk)).await?;
    assert!(
        symbol_names(&first).is_empty(),
        "nothing installed resolves: {first}"
    );

    for (path, content) in INSTALLED_FILES
        .iter()
        .filter(|(path, _)| installed_after(path))
    {
        let file = directory.path().join(path);
        std::fs::create_dir_all(file.parent().ok_or("an installed file has a folder")?)?;
        std::fs::write(file, content)?;
    }
    let second = call_retrying_acceptance(&client, tool_request("search", &walk)).await?;
    assert_eq!(symbol_names(&second), ["greet"], "{second}");
    let greet = callee_hit(&second, "greet")?;
    assert_eq!(
        greet["hit"]["symbol"]["id"],
        json!("rift://symbol/python/pypi/greeting@1.0.0/greeting/core.py/greet"),
        "{greet}"
    );
    assert_eq!(
        greet["hit"]["symbol"]["origin"]["location"],
        json!("dependency"),
        "{greet}"
    );

    client.cancel().await?;
    Ok(())
}

/// A global API that refuses the connection names no callee: every package callee drops,
/// and the answer carries the typed warning naming the failure.
#[tokio::test]
async fn search_traversal_outgoing_over_an_unavailable_global_api_drops_package_callees()
-> TestResult {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let configuration = format!(
        "{}\n{OUTGOING_ENGINE}",
        global_table(&format!("http://127.0.0.1:{port}/rift/rest"))
    );
    let (_directory, client, _server_task) =
        served_workspace(OUTGOING_FILES, Some(configuration)).await?;

    let counted = call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "traversal": {
                    "seed": "rift://symbol/python/extra.py/counted", "direction": "outgoing"
                }
            }),
        ),
    )
    .await?;
    assert_eq!(symbol_names(&counted), ["leaf"], "{counted}");
    let codes: Vec<&Value> = counted["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|warning| &warning["code"])
        .collect();
    assert_eq!(
        codes,
        [&json!("callees_dropped"), &json!("global_api_unavailable")],
        "{counted}"
    );

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
