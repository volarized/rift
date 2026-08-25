//! Live integration: the semantic search tier against the real model hub.
//!
//! `RIFT_SEARCH_LIVE=1 cargo test -p rift-mcp --test live_semantic_search`
//! runs the suite; without the variable every test skips visibly. This is the
//! one suite that lets a fixture keep the shipped `[search.semantic]` table, so
//! it is the one that proves what every other fixture turns off: the weights are
//! acquired from the hub, the published declarations are embedded, and a query
//! sharing no word with the code it describes still reaches it.
//!
//! The suite drives a live rmcp client and reads the tier's state the way a
//! caller does, from a `search` result's own warnings: `semantic_index_preparing`
//! while the pass runs, and nothing once every declaration carries a vector.
//! Nothing here reads server internals, because nothing a caller cannot see is
//! what this suite is for.
//!
//! Every wait is bounded and every bound is named below. A hub that is slow or
//! unreachable fails a test with the readiness it reached; no wait here can hang.
//!
//! The serving helper is local rather than `workspace_client`'s, because every
//! fixture that helper lays out carries the table this suite must not have.

mod search_live_gate;

use std::error::Error;
use std::fs;
use std::time::{Duration, Instant};

use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use search_live_gate::search_live;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The declaration the paraphrase must reach.
///
/// Its identifiers carry no word of [`PARAPHRASE`], which
/// `assert_shares_no_token` proves rather than asserts by comment, so the
/// lexical tier cannot match it and only an embedding can.
const LIBRARY: &str = "pub fn scale_value(input: f64) -> f64 {\n    input * 2.0\n}\n";

/// A second declaration, so the ranking has more than one candidate to place.
const NEIGHBOUR: &str = "pub fn open_socket(port: u16) -> u16 {\n    port\n}\n";

/// What a caller would ask, in words neither file contains.
const PARAPHRASE: &str = "multiply numeric quantity twice";

/// The cold acquisition's budget.
///
/// The shipped model is roughly 130MB over three files, and a slow link is still
/// expected inside five minutes. A hub slower than this fails the test naming the
/// last warnings it saw.
const COLD_READY_MAX: Duration = Duration::from_mins(5);

/// The warm acquisition's budget.
///
/// A cached revision short-circuits before any request is made, so a second
/// server only loads the weights from disk and embeds the fixture again. One
/// minute is far above that cost and far below any cold download, which is what
/// makes the second wait evidence that nothing was fetched twice.
const WARM_READY_MAX: Duration = Duration::from_mins(1);

/// Wait between two readings of the tier's state.
const READINESS_POLL: Duration = Duration::from_millis(250);

/// One served workspace and the task serving it.
type Served = (
    tempfile::TempDir,
    RunningService<RoleClient, ()>,
    tokio::task::JoinHandle<()>,
);

/// Serves one workspace carrying no `rift.toml`, so every shipped default
/// applies and the semantic tier acquires its model.
async fn served_default_workspace(files: &[(&str, &str)]) -> TestResult<Served> {
    served_configured_workspace(files, None).await
}

/// Serves one workspace, optionally under `configuration`.
///
/// The `None` spelling leaves the shipped defaults in place, which is the whole
/// point of this suite; the `Some` spelling is how the control below turns the
/// semantic tier off over the very same files.
async fn served_configured_workspace(
    files: &[(&str, &str)],
    configuration: Option<&str>,
) -> TestResult<Served> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        fs::write(directory.path().join(name), source)?;
    }
    if let Some(configuration) = configuration {
        fs::write(directory.path().join("rift.toml"), configuration)?;
    }
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
    Ok((directory, client, server_task))
}

/// The fixture both tests serve.
fn workspace() -> [(&'static str, &'static str); 2] {
    [("lib.rs", LIBRARY), ("net.rs", NEIGHBOUR)]
}

/// Runs one `search` and returns its structured result.
async fn search(client: &RunningService<RoleClient, ()>, query: &str) -> TestResult<Value> {
    let arguments = json!({ "query": query })
        .as_object()
        .cloned()
        .ok_or("search arguments are an object")?;
    let request = CallToolRequestParams::new("search").with_arguments(arguments);
    let result = client.peer().call_tool(request).await?;
    result
        .structured_content
        .ok_or_else(|| "search must return structured content".into())
}

/// Whether one search answer still reports the semantic tier as not answering.
fn tier_is_waiting(answer: &Value) -> bool {
    answer["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(is_semantic_warning))
}

/// Whether one warning is the semantic tier reporting on itself.
fn is_semantic_warning(warning: &Value) -> bool {
    matches!(
        warning["code"].as_str(),
        Some("semantic_index_preparing" | "semantic_ranking_unavailable")
    )
}

/// Polls `query` until the semantic tier stops warning about itself, and returns
/// the answer it settled on together with how long that took.
///
/// The loop runs at most `budget / READINESS_POLL` times, so a hub that never
/// answers spends the budget and fails naming the last warnings it saw. It never
/// waits unbounded.
async fn ready_answer(
    client: &RunningService<RoleClient, ()>,
    query: &str,
    budget: Duration,
) -> TestResult<(Value, Duration)> {
    let started = Instant::now();
    let attempts_max = budget.as_millis() / READINESS_POLL.as_millis();
    let mut last = Value::Null;
    for _attempt in 0..attempts_max {
        let answer = search(client, query).await?;
        if !tier_is_waiting(&answer) {
            return Ok((answer, started.elapsed()));
        }
        last = answer;
        tokio::time::sleep(READINESS_POLL).await;
    }
    Err(format!(
        "the semantic tier did not answer within {budget:?}; last warnings: {}",
        last["warnings"]
    )
    .into())
}

/// Proves `query` shares no word with `sources`, so a hit for it cannot have come
/// from the lexical tier.
fn assert_shares_no_token(query: &str, sources: &[(&str, &str)]) {
    for term in query.split_whitespace() {
        let lowered = term.to_lowercase();
        for (name, source) in sources {
            assert!(
                !source.to_lowercase().contains(&lowered),
                "the query must share no word with the code it reaches: \
                 term={lowered} file={name}"
            );
        }
    }
}

/// Whether one answer carries a hit at `path`.
fn reaches(answer: &Value, path: &str) -> bool {
    answer["results"]
        .as_array()
        .is_some_and(|results| results.iter().any(|hit| hit["path"] == json!(path)))
}

/// The control's table: the same files, served with the tier the suite is about
/// turned off.
const SEMANTIC_DISABLED: &str = "[search.semantic]\ndisabled = true\n";

#[tokio::test]
async fn a_paraphrase_reaches_code_it_shares_no_word_with() -> TestResult {
    if !search_live() {
        return Ok(());
    }
    assert_shares_no_token(PARAPHRASE, &workspace());

    // The control: the same files with the semantic tier off. A disabled tier never
    // prepares, so this answer is the lexical one whatever the cache already holds -
    // which is what makes the comparison below a fact rather than a race.
    let (_off_directory, off, off_task) =
        served_configured_workspace(&workspace(), Some(SEMANTIC_DISABLED)).await?;
    let lexical_only = search(&off, PARAPHRASE).await?;
    assert!(
        !reaches(&lexical_only, "lib.rs"),
        "a paraphrase must not reach the declaration lexically: {lexical_only:#}"
    );
    off.cancel().await?;
    off_task.await?;

    let (_directory, client, server_task) = served_default_workspace(&workspace()).await?;
    let (answer, elapsed) = ready_answer(&client, PARAPHRASE, COLD_READY_MAX).await?;
    assert!(
        reaches(&answer, "lib.rs"),
        "the prepared semantic tier must reach the declaration the paraphrase describes \
         after {elapsed:?}: {answer:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn a_second_server_loads_the_cached_weights_instead_of_fetching_them() -> TestResult {
    if !search_live() {
        return Ok(());
    }
    // The first server leaves the revision in the shared cache, whatever the
    // machine already held.
    let (_first_directory, first, first_task) = served_default_workspace(&workspace()).await?;
    ready_answer(&first, PARAPHRASE, COLD_READY_MAX).await?;
    first.cancel().await?;
    first_task.await?;

    // A cached revision short-circuits before any request, so the second server
    // reaches the same state inside a budget no download fits in.
    let (_second_directory, second, second_task) = served_default_workspace(&workspace()).await?;
    let (answer, elapsed) = ready_answer(&second, PARAPHRASE, WARM_READY_MAX).await?;
    assert!(
        reaches(&answer, "lib.rs"),
        "the second server must rank through the cached weights: {answer:#}"
    );
    assert!(
        elapsed < WARM_READY_MAX,
        "a cached acquisition must not spend a download's time: {elapsed:?}"
    );

    second.cancel().await?;
    second_task.await?;
    Ok(())
}
