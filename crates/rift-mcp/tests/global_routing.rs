//! Global package routing through served `get_symbol` and `search` reads: the project hits
//! come from the served snapshot, the package hits from a fixture global API, and every
//! context entry no public registry serves answers with its typed warning.

mod global_api;
mod hermetic_search;
#[allow(
    dead_code,
    reason = "shared integration fixture exposes helpers this suite does not use"
)]
mod workspace_client;

use std::{fs, time::Duration};

use axum::http::StatusCode;
use global_api::{
    BODY_BOUND_CURSOR, BODY_MATCH_QUERY, COLLECTED_UNIT, FixtureOptions, GlobalFixture, Hold,
    SymbolFixture, UNSATISFIED_REQUIREMENT,
};
use rift_protocol::error::ErrorCode;
use rmcp::model::ReadResourceRequestParams;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use workspace_client::{
    ServedWorkspace, TestResult, call_retrying_acceptance, failed_call, served_workspace,
    tool_request,
};

const LOCK_WITH_HELPER: &str = "version = 4\n\n[[package]]\nname = \"helper\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\ndependencies = [\n \"helper\",\n]\n";

/// A served workspace whose manifest depends on `helper` by a path outside it.
struct DependentWorkspace {
    /// Held for the life of the server: the directory the path dependency names.
    _helper: tempfile::TempDir,
    served: ServedWorkspace,
}

async fn served_dependent_workspace(configuration: Option<&str>) -> TestResult<DependentWorkspace> {
    let helper = tempfile::tempdir()?;
    let manifest = format!(
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nhelper = {{ path = '{}' }}\n",
        helper.path().display()
    );
    let served = served_workspace(
        &[
            ("Cargo.toml", manifest.as_str()),
            ("Cargo.lock", LOCK_WITH_HELPER),
            (
                "src/lib.rs",
                "pub fn local_beacon() {}\npub fn beacon() {}\n",
            ),
        ],
        configuration.map(str::to_owned),
    )
    .await?;
    Ok(DependentWorkspace {
        _helper: helper,
        served,
    })
}

async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &'static str,
    args: Value,
) -> TestResult<Value> {
    let result = client.call_tool(tool_request(name, &args)).await?;
    result
        .structured_content
        .ok_or_else(|| format!("{name} must return structured content").into())
}

async fn get_symbol(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: Value,
) -> TestResult<Value> {
    call_tool(client, "get_symbol", args).await
}

/// The configured package list naming the collected `demo` release. The fixture tables
/// already open `[dependencies]`, so the list lands as an array of tables below it.
const DEMO_PACKAGE: &str =
    "[[dependencies.packages]]\nmanager = \"cargo\"\nname = \"demo\"\nversion = \"1.0.0\"\n";

/// The configured package list naming `demo`, an exact `absent` the fixture collection
/// lacks, and a `wanted` requirement it cannot resolve.
const THREE_PACKAGES: &str = "[[dependencies.packages]]\nmanager = \"cargo\"\nname = \"demo\"\nversion = \"1.0.0\"\n\n\
     [[dependencies.packages]]\nmanager = \"cargo\"\nname = \"absent\"\nversion = \"1.0.0\"\n\n\
     [[dependencies.packages]]\nmanager = \"cargo\"\nname = \"wanted\"\nrequirement = \"^1\"\n";

/// The `[global]` connect bound under which a refused loopback port still answers as
/// refused.
///
/// Windows answers a refused connect only after it resends the SYN, which takes about
/// two seconds on loopback; a shorter bound reports the refusal as a timeout there.
const REFUSAL_CONNECT_TIMEOUT: &str = "10s";
/// The `[global]` request bound above [`REFUSAL_CONNECT_TIMEOUT`].
const REFUSAL_REQUEST_TIMEOUT: &str = "20s";

#[tokio::test]
async fn refused_global_api_returns_typed_warning_and_no_package_facts() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"http://127.0.0.1:{port}/rift/rest\"\n\
         token_env = \"RIFT_TEST_GLOBAL_TOKEN\"\nattempts = 1\n\
         request_timeout = \"{REFUSAL_REQUEST_TIMEOUT}\"\n\
         connect_timeout = \"{REFUSAL_CONNECT_TIMEOUT}\"\n\n{DEMO_PACKAGE}"
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let answer = get_symbol(&client, json!({"name":"local_beacon","scope":"global"})).await?;
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let warning = warnings
        .iter()
        .find(|warning| warning["code"] == "global_api_unavailable")
        .ok_or_else(|| format!("missing global API warning: {answer:#}"))?;
    assert_eq!(
        warning,
        &json!({"code": "global_api_unavailable", "failure_class": "connection"}),
        "the warning carries no package counts"
    );
    let rendered = serde_json::to_string(warning)?;
    assert!(!rendered.contains("RIFT_TEST_GLOBAL_TOKEN"));
    assert!(!rendered.contains("local_beacon"));
    assert_eq!(answer["hits"], json!([]));
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// How long the fixture holds a resolution. A server that ignored the one-second
/// request deadline could answer only after it, and one that honors the deadline answers
/// well before it, however slow the machine running the test.
const RESOLUTION_DELAY: Duration = Duration::from_secs(10);

#[tokio::test]
async fn request_deadline_bounds_global_resolution() -> TestResult {
    let hold = Some(Hold::Resolution(RESOLUTION_DELAY));
    let fixture = GlobalFixture::start_holding(SymbolFixture::Valid, hold).await?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"30s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let started = tokio::time::Instant::now();
    let answer = get_symbol(&client, json!({"name":"local_beacon","scope":"global"})).await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed < RESOLUTION_DELAY,
        "the request deadline must end the resolution wait: elapsed={elapsed:?}"
    );
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_api_unavailable")
        .ok_or_else(|| format!("missing deadline warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "timeout");
    assert_eq!(fixture.requests().await.len(), 2);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// How long the fixture holds a symbol or search page: past the one-second request
/// deadline, however slow the machine running the test.
const READ_DELAY: Duration = Duration::from_secs(10);

/// A package read the request deadline cuts short discards the remote lane: `get_symbol`
/// and `search` each answer the project hits alone, with the timeout warning, well before
/// the held page would have arrived.
#[tokio::test]
async fn request_deadline_bounds_the_remote_package_read() -> TestResult {
    let hold = Some(Hold::Read(READ_DELAY));
    let fixture = GlobalFixture::start_holding(SymbolFixture::Valid, hold).await?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"1s\"\n\n\
         [global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"30s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let reads = [
        (
            "get_symbol",
            "hits",
            json!({"name": "beacon", "scope": "all"}),
        ),
        (
            "search",
            "results",
            json!({"query": "beacon", "scope": "all"}),
        ),
    ];
    for (tool, member, args) in reads {
        let started = tokio::time::Instant::now();
        let answer = call_tool(&client, tool, args).await?;
        let elapsed = started.elapsed();
        assert!(
            elapsed < READ_DELAY,
            "{tool}: the request deadline must end the read wait: elapsed={elapsed:?}"
        );
        let warning = answer["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|warning| warning["code"] == "global_api_unavailable")
            .ok_or_else(|| format!("{tool}: missing deadline warning: {answer:#}"))?;
        assert_eq!(warning["failure_class"], "timeout", "{tool}");
        let hits = answer[member]
            .as_array()
            .ok_or("the answer lists its hits")?;
        assert!(!hits.is_empty(), "{tool}: {answer:#}");
        assert!(
            hits.iter().all(|hit| hit["path"] == "src/lib.rs"),
            "{tool}: every hit is a project hit: {answer:#}"
        );
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// How long the fixture holds the global answer an outgoing walk waits for: past the
/// request deadline [`CALLEE_WALK_BUDGET`] sets, however slow the machine running the test.
const CALLEE_HOLD: Duration = Duration::from_secs(60);

/// The `[server] readiness_timeout` an outgoing walk against a held global answer runs
/// under: the embedded engine's walk fits in the nine tenths of it the walk may spend,
/// and the held answer outlasts all of it.
const CALLEE_WALK_BUDGET: &str = "5s";

/// A module whose `counted` calls the standard library's `len`, the callee an outgoing
/// walk asks the global API to name.
const CALLEE_FILES: &[(&str, &str)] = &[("app.py", "def counted() -> int:\n    return len([1])\n")];

/// An outgoing walk from `counted` against a fixture holding `hold` past the request
/// deadline: the walk answers well before the held answer would have arrived, the
/// standard library callee drops, and the answer carries the timeout warning.
async fn outgoing_walk_past_a_held_global_answer(hold: Hold) -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        hold: Some(hold),
        python_collection: true,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"{CALLEE_WALK_BUDGET}\"\n\n\
         [global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"30s\"\nconnect_timeout = \"100ms\"\n\n\
         [languages.python.lsp]\nembedded = \"ty\"\n",
        fixture.endpoint
    );
    let (_directory, client, server_task) =
        served_workspace(CALLEE_FILES, Some(configuration)).await?;
    let walk = json!({
        "traversal": { "seed": "rift://symbol/python/app.py/counted", "direction": "outgoing" }
    });
    let started = tokio::time::Instant::now();
    let answer = call_retrying_acceptance(&client, tool_request("search", &walk)).await?;
    let elapsed = started.elapsed();
    assert!(
        elapsed < CALLEE_HOLD,
        "the request deadline must end the wait: elapsed={elapsed:?}"
    );
    let warnings = answer["warnings"].clone();
    assert_eq!(warnings[0]["code"], "callees_dropped", "{answer:#}");
    assert_eq!(warnings[0]["callees"], 1, "{answer:#}");
    assert_eq!(
        warnings[1],
        json!({"code": "global_api_unavailable", "failure_class": "timeout"}),
        "{answer:#}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A resolution held past the request deadline names no callee.
#[tokio::test]
async fn request_deadline_bounds_the_resolution_an_outgoing_walk_waits_for() -> TestResult {
    outgoing_walk_past_a_held_global_answer(Hold::Resolution(CALLEE_HOLD)).await
}

/// A declarations answer held past the request deadline names no callee.
#[tokio::test]
async fn request_deadline_bounds_the_declarations_an_outgoing_walk_waits_for() -> TestResult {
    outgoing_walk_past_a_held_global_answer(Hold::Declarations(CALLEE_HOLD)).await
}

/// A path dependency outside the workspace reaches no index: the package hits come from
/// the global index alone, and the answer names the path entry with the capability Rift
/// does not have yet, the exact package the collection lacks, and each requirement it
/// cannot resolve, the standard library entry the Rust source names included.
#[tokio::test]
async fn successful_resolution_reports_unserved_entries_and_missing_packages() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{THREE_PACKAGES}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = get_symbol(&client, json!({"name":"helper_beacon","scope":"all"})).await?;
    let hits = answer["hits"].as_array().ok_or("hits are an array")?;
    assert_eq!(
        hits.iter()
            .map(|hit| hit["unit"].clone())
            .collect::<Vec<_>>(),
        [json!(COLLECTED_UNIT)],
        "the helper outside the workspace is analyzed nowhere: {answer:#}"
    );
    let (warnings, page_warnings): (Vec<&Value>, Vec<&Value>) = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .partition(|warning| warning["code"] != "global_page_warning");
    let codes: Vec<&str> = warnings
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect();
    assert_eq!(
        codes,
        [
            "package_unavailable",
            "package_absent",
            "package_requirement_absent",
            "package_requirement_absent"
        ],
        "{answer:#}"
    );
    assert_eq!(
        page_warnings
            .iter()
            .map(|warning| &warning["warning_code"])
            .collect::<Vec<_>>(),
        [&json!("source_truncated")]
    );
    assert_eq!(
        *warnings[0],
        json!({
            "code": "package_unavailable",
            "entry": {
                "manager": "cargo",
                "name": "helper",
                "version": "0.1.0",
                "availability": "path"
            },
            "reason": "Currently rift doesn't support indexing dependencies by path. If you're \
                       interested in this capability, please upvote it at \
                       https://github.com/volarized/rift/issues/392."
        })
    );
    assert_eq!(
        warnings[1]["package"],
        json!({"manager":"cargo","name":"absent","version":"1.0.0"})
    );
    let requirements: Vec<&Value> = warnings[2..4]
        .iter()
        .map(|warning| &warning["entry"])
        .collect();
    assert_eq!(
        requirements,
        [
            &json!({
                "manager":"cargo",
                "name":"wanted",
                "requirement":"^1",
                "availability":"canonical"
            }),
            &json!({
                "manager":"stdlib",
                "name":"rust",
                "requirement":">=0",
                "availability":"canonical"
            }),
        ]
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The collected `demo` release answers an entry the collection holds no release for, and
/// the answer names the substitution: an exact version the collection lacks, and a
/// requirement the collected release lies outside. The package hits still answer.
#[tokio::test]
async fn a_nearest_release_answers_and_names_the_substitution() -> TestResult {
    let cases = [
        (
            "version = \"1.0.3\"".to_owned(),
            json!({"manager":"cargo","name":"demo","version":"1.0.3","availability":"canonical"}),
        ),
        (
            format!("requirement = \"{UNSATISFIED_REQUIREMENT}\""),
            json!({
                "manager":"cargo",
                "name":"demo",
                "requirement":UNSATISFIED_REQUIREMENT,
                "availability":"canonical"
            }),
        ),
    ];
    for (selector, entry) in cases {
        let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
        let configuration = format!(
            "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
             request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
             [[dependencies.packages]]\nmanager = \"cargo\"\nname = \"demo\"\n{selector}\n",
            fixture.endpoint
        );
        let (directory, client, server_task) = served_workspace(
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
                ),
                (
                    "Cargo.lock",
                    "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
                ),
                ("src/lib.rs", "pub fn local_beacon() {}\n"),
            ],
            Some(configuration),
        )
        .await?;
        let answer = get_symbol(&client, json!({"name":"helper_beacon","scope":"global"})).await?;
        assert_eq!(
            answer["hits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|hit| &hit["unit"])
                .collect::<Vec<_>>(),
            [&json!(COLLECTED_UNIT)],
            "{answer:#}"
        );
        let substituted: Vec<&Value> = answer["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|warning| warning["code"] == "package_substituted")
            .collect();
        assert_eq!(
            substituted,
            [&json!({
                "code": "package_substituted",
                "entry": entry,
                "package": {"manager":"cargo","name":"demo","version":"1.0.0"}
            })],
            "{answer:#}"
        );
        drop(directory);
        client.cancel().await?;
        server_task.await?;
    }
    Ok(())
}

#[tokio::test]
async fn successful_search_uses_remote_phases_without_local_request_fields() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{THREE_PACKAGES}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = call_tool(
        &client,
        "search",
        json!({"query":"helper_beacon demo","scope":"global"}),
    )
    .await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| { results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT) }),
        "{answer:#}"
    );
    assert!(answer["warnings"].as_array().is_some_and(|warnings| {
        warnings.iter().any(|warning| {
            warning["code"] == "global_page_warning" && warning["warning_code"] == "query_narrowed"
        })
    }));

    let requests = fixture.requests().await;
    let resolution = requests
        .iter()
        .find(|request| request.uri.ends_with("/v1/resolutions"))
        .and_then(|request| request.body.as_ref())
        .ok_or("resolution request is recorded")?;
    let resolution_text = resolution.to_string();
    assert!(resolution_text.contains("demo"));
    assert!(resolution_text.contains("absent"));
    assert!(resolution_text.contains("stdlib"));
    assert!(
        !resolution_text.contains("helper"),
        "a path entry reaches no index: {resolution_text}"
    );
    assert!(
        resolution["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|entry| entry["availability"] == "canonical"),
        "{resolution_text}"
    );

    let searches = requests
        .iter()
        .filter(|request| request.uri.contains("/v1/search?"))
        .collect::<Vec<_>>();
    assert_eq!(searches.len(), 2, "{requests:#?}");
    assert_eq!(
        searches[0].body.as_ref().map(|body| &body["phase"]),
        Some(&json!("precise"))
    );
    assert_eq!(
        searches[1].body.as_ref().map(|body| &body["phase"]),
        Some(&json!("broad"))
    );
    for request in searches {
        let body = request.body.as_ref().ok_or("search body is recorded")?;
        assert_eq!(
            body["packages"],
            json!([{"manager":"cargo","name":"demo","version":"1.0.0"}])
        );
        let rendered = body.to_string();
        assert!(!rendered.contains("src/lib.rs"));
        assert!(!rendered.contains("local_beacon"));
        assert!(!rendered.contains("RIFT_TEST_GLOBAL_TOKEN"));
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The search requests `fixture` received, in order.
async fn search_requests(fixture: &GlobalFixture) -> Vec<global_api::ObservedRequest> {
    fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.contains("/v1/search?"))
        .collect()
}

/// A search asks each phase for one page at the smaller of the client's bound and the
/// advertised `page_limit_max`: 200 against a service advertising 200, 1,000 against one
/// advertising 1,000, and one request per phase either way.
#[tokio::test]
async fn a_search_asks_one_page_per_phase_at_the_advertised_limit() -> TestResult {
    for (advertised, limit) in [(200, "limit=200"), (1_000, "limit=1000")] {
        let fixture = GlobalFixture::start_with(FixtureOptions {
            page_limit_max: advertised,
            ..FixtureOptions::default()
        })
        .await?;
        let configuration = format!(
            "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
             request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
            fixture.endpoint
        );
        let workspace = served_dependent_workspace(Some(&configuration)).await?;
        let (directory, client, server_task) = workspace.served;
        let answer = call_tool(
            &client,
            "search",
            json!({"query":"helper_beacon demo","scope":"global"}),
        )
        .await?;
        assert!(
            answer["results"]
                .as_array()
                .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
            "{answer:#}"
        );
        let searches = search_requests(&fixture).await;
        assert_eq!(searches.len(), 2, "{searches:#?}");
        for request in &searches {
            assert!(
                request.uri.ends_with(limit),
                "{advertised}: {}",
                request.uri
            );
        }
        drop(directory);
        client.cancel().await?;
        server_task.await?;
    }
    Ok(())
}

/// A search page the service stopped at its response body bound answers what fit, with the
/// page's `result_truncated` as a `global_page_warning`, and its cursor is not followed: the
/// search stays at one request per phase.
#[tokio::test]
async fn a_page_stopped_at_the_body_bound_answers_without_following_its_cursor() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        stopped_at_body_bound: true,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = call_tool(
        &client,
        "search",
        json!({"query":"helper_beacon demo","scope":"global","include":["source"]}),
    )
    .await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
        "{answer:#}"
    );
    let truncated: Vec<&Value> = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| {
            warning["code"] == "global_page_warning"
                && warning["warning_code"] == "result_truncated"
        })
        .collect();
    assert_eq!(
        truncated,
        [&json!({
            "code": "global_page_warning",
            "warning_code": "result_truncated",
            "detail": "the page stopped at response_body_bytes_max"
        })],
        "{answer:#}"
    );
    let searches = search_requests(&fixture).await;
    assert_eq!(searches.len(), 2, "{searches:#?}");
    assert!(
        searches
            .iter()
            .all(|request| !request.uri.contains(BODY_BOUND_CURSOR)),
        "the stopped page's cursor is never followed: {searches:#?}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The pattern requests `fixture` received, in order.
async fn pattern_requests(fixture: &GlobalFixture) -> Vec<global_api::ObservedRequest> {
    fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.contains("/v1/patterns?"))
        .collect()
}

/// What a `global` search for `fn helper_\w+` with `target: "all"` and `source` answers: the
/// collected declaration holding the match, then the match itself, both addressed by `unit`.
fn helper_beacon_pattern_hits() -> Value {
    json!([
        {
            "hit": {
                "target": "symbol",
                "symbol": {
                    "id": "rift://symbol/rust/cargo/demo@1.0.0/src/lib.rs/helper_beacon",
                    "language": "rust",
                    "name": "helper_beacon",
                    "kind": "function",
                    "origin": {
                        "location": "dependency",
                        "package": {"manager": "cargo", "name": "demo", "version": "1.0.0"},
                        "source_kind": "authored"
                    }
                }
            },
            "matched_by": ["content"],
            "source": "pub fn helper_beacon() {}",
            "range": {"start": 0, "end": 25},
            "line": 1,
            "unit": COLLECTED_UNIT
        },
        {
            "hit": {"target": "file", "size": 45},
            "matched_by": ["content"],
            "source": "pub fn helper_beacon() {}",
            "range": {"start": 4, "end": 20},
            "line": 1,
            "unit": COLLECTED_UNIT
        }
    ])
}

/// A `pattern` whose `scope` reaches packages answers the package matches from one page of
/// the global API's pattern search: `global` alone, `all` after the project's own. A package
/// match answers a file hit addressed by `unit` and a symbol hit for the declaration holding
/// it, each with its source when the request includes it.
#[tokio::test]
async fn a_package_scoped_pattern_answers_package_matches_from_one_request() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;

    let global = call_tool(
        &client,
        "search",
        json!({
            "pattern": r"fn helper_\w+",
            "scope": "global",
            "target": "all",
            "include": ["source"]
        }),
    )
    .await?;
    assert_eq!(
        global["results"],
        helper_beacon_pattern_hits(),
        "{global:#}"
    );

    let all = call_tool(
        &client,
        "search",
        json!({"pattern": r"fn \w*beacon", "scope": "all", "target": "file"}),
    )
    .await?;
    let addressed: Vec<(&Value, &Value)> = all["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| (&hit["path"], &hit["unit"]))
        .collect();
    let project = json!("src/lib.rs");
    let package = json!(COLLECTED_UNIT);
    assert_eq!(
        addressed,
        [
            (&project, &Value::Null),
            (&project, &Value::Null),
            (&Value::Null, &package),
            (&Value::Null, &package),
        ],
        "the project's matches first, then the package's: {all:#}"
    );

    let local = call_tool(&client, "search", json!({"pattern": r"fn \w*beacon"})).await?;
    assert!(
        local["results"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|hit| hit["path"] == project),
        "{local:#}"
    );

    let requests = pattern_requests(&fixture).await;
    assert_eq!(
        requests.len(),
        2,
        "one request per package-scoped search: {requests:#?}"
    );
    let bodies: Vec<&Value> = requests
        .iter()
        .filter_map(|request| request.body.as_ref())
        .collect();
    assert_eq!(
        bodies,
        [
            &json!({
                "pattern": r"fn helper_\w+",
                "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}],
                "include": ["source"]
            }),
            &json!({
                "pattern": r"fn \w*beacon",
                "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}]
            }),
        ]
    );
    assert!(
        requests
            .iter()
            .all(|request| request.uri.ends_with("limit=200")),
        "{requests:#?}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A pattern the server refuses spends no global request: the project side reads, and
/// refuses, before the package side is asked.
#[tokio::test]
async fn a_refused_package_scoped_pattern_makes_no_global_request() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &json!({"pattern": "beacon(", "scope": "global"}),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest, "{}", failure.text);
    assert!(fixture.requests().await.is_empty());
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn local_file_and_map_reads_make_no_global_request() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let _ = get_symbol(&client, json!({"name":"local_beacon"})).await?;
    let _ = call_tool(
        &client,
        "search",
        json!({"query":"local_beacon","scope":"global","target":"file"}),
    )
    .await?;
    let _ = client
        .read_resource(ReadResourceRequestParams::new("rift://map".to_owned()))
        .await?;
    assert!(fixture.requests().await.is_empty());
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A remote page that fails validation discards the remote lane: `beacon` is declared in
/// the project and in the collected package, and the `get_symbol` and `search` answers
/// carry the project hits alone, with the typed warning naming the failure and no package
/// counts.
#[tokio::test]
async fn invalid_remote_page_discards_the_lane_and_answers_project_hits() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::InvalidIdentity).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = get_symbol(&client, json!({"name":"beacon","scope":"all"})).await?;
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_response_invalid")
        .ok_or_else(|| format!("missing invalid-response warning: {answer:#}"))?;
    assert_eq!(
        warning,
        &json!({"code": "global_response_invalid", "failure_class": "invalid_response"})
    );
    let hits = answer["hits"].as_array().ok_or("hits are an array")?;
    let names: Vec<&Value> = hits.iter().map(|hit| &hit["symbol"]["name"]).collect();
    assert_eq!(
        names,
        [&json!("beacon"), &json!("local_beacon")],
        "{answer:#}"
    );
    assert!(
        hits.iter().all(|hit| hit["path"] == "src/lib.rs"),
        "every hit is a project hit: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;

    // Inside `failure_ttl` the client hands back the failure it recorded and asks the
    // global API nothing, so the search runs against a server of its own to meet the
    // invalid page.
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = call_tool(&client, "search", json!({"query":"beacon","scope":"all"})).await?;
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_response_invalid")
        .ok_or_else(|| format!("missing invalid-response warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "invalid_response");
    let results = answer["results"].as_array().ok_or("results are an array")?;
    assert!(!results.is_empty(), "{answer:#}");
    assert!(
        results.iter().all(|hit| hit["path"] == "src/lib.rs"),
        "every search hit is a project hit: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A failed global read marks the global API unavailable for `failure_ttl`, and a later
/// read inside it answers from that failure: it carries the same typed warning and class
/// the first read met, and sends the global API nothing.
#[tokio::test]
async fn a_read_after_an_invalid_page_repeats_its_failure_class() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::InvalidIdentity).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\nfailure_ttl = \"1h\"\n\n\
         {DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let expected = json!({"code": "global_response_invalid", "failure_class": "invalid_response"});
    let global_warnings = |answer: &Value| -> Vec<Value> {
        answer["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|warning| {
                warning["code"]
                    .as_str()
                    .is_some_and(|code| code.starts_with("global_"))
            })
            .cloned()
            .collect()
    };

    let first = get_symbol(&client, json!({"name":"beacon","scope":"all"})).await?;
    assert_eq!(
        global_warnings(&first),
        std::slice::from_ref(&expected),
        "{first:#}"
    );
    let sent = fixture.requests().await.len();

    let later = call_tool(
        &client,
        "search",
        json!({"query": "beacon", "scope": "all", "target": "symbol"}),
    )
    .await?;
    assert_eq!(global_warnings(&later), [expected], "{later:#}");
    assert_eq!(
        fixture.requests().await.len(),
        sent,
        "a read inside failure_ttl sends the global API nothing"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A resolution the global API answers with a problem status leaves the route without
/// packages: the read answers the project hits alone, with the warning naming the status
/// class, and asks for no package page.
#[tokio::test]
async fn a_refused_resolution_answers_project_hits_with_its_failure_class() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        resolution_problem: Some(StatusCode::INTERNAL_SERVER_ERROR),
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let answer = get_symbol(&client, json!({"name": "local_beacon", "scope": "all"})).await?;
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_api_unavailable")
        .ok_or_else(|| format!("missing global API warning: {answer:#}"))?;
    assert_eq!(
        warning,
        &json!({"code": "global_api_unavailable", "failure_class": "non_success_response"})
    );
    let hits = answer["hits"].as_array().ok_or("hits are an array")?;
    let located: Vec<(&Value, &Value)> = hits
        .iter()
        .map(|hit| (&hit["symbol"]["name"], &hit["path"]))
        .collect();
    assert_eq!(
        located,
        [(&json!("local_beacon"), &json!("src/lib.rs"))],
        "{answer:#}"
    );
    let requested: Vec<String> = fixture
        .requests()
        .await
        .into_iter()
        .map(|request| request.uri)
        .collect();
    assert_eq!(
        requested,
        ["/rift/rest/v1/capabilities", "/rift/rest/v1/resolutions"],
        "the refused resolution ends the read's global requests"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Package facts are served for the current tree alone, so a `scope` past `local` beside
/// `rev` refuses naming `scope`.
#[tokio::test]
async fn a_revision_search_with_a_global_scope_refuses_invalid_request() -> TestResult {
    let (directory, client, server_task) =
        served_workspace(&[("src/lib.rs", "pub fn local_beacon() {}\n")], None).await?;
    // A committed baseline, so `main` resolves and the scope rule is what refuses; the
    // workspace database stays out of the commit.
    fs::write(directory.path().join(".gitignore"), ".rift/\n")?;
    rift_history::fixture::init(directory.path());
    rift_history::fixture::commit_all(directory.path(), "fixture baseline");
    call_tool(&client, "search", json!({"query": "local_beacon"})).await?;

    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &json!({"query": "local_beacon", "scope": "all", "rev": "main"}),
            ))
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::InvalidRequest, "{}", failure.text);
    assert!(
        failure.message.contains("scope"),
        "the refusal names the field: {}",
        failure.message
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Each kind of entry no public registry serves answers with `package_unavailable` and the
/// sentence naming the capability Rift does not have yet: a path outside the workspace and
/// a git repository link the issue collecting demand for them, a private registry and a
/// URL link none. A path dependency inside the workspace is project source and answers
/// nothing, and a disabled global API says so once.
#[tokio::test]
async fn a_disabled_global_api_names_every_unserved_kind_and_leaves_project_source_out()
-> TestResult {
    let manifest = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                    [dependencies]\ninner = { path = \"crates/inner\" }\n\
                    outside = { path = \"../outside-probe\" }\n\
                    sourced = { git = \"https://example.test/sourced\" }\n\
                    private = { version = \"0.6\", registry = \"internal\" }\n";
    let lockfile = "version = 4\n\n\
                    [[package]]\nname = \"inner\"\nversion = \"0.1.0\"\n\n\
                    [[package]]\nname = \"outside\"\nversion = \"0.2.0\"\n\n\
                    [[package]]\nname = \"private\"\nversion = \"0.6.1\"\n\
                    source = \"registry+https://registry.example.test/index\"\n\n\
                    [[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n\n\
                    [[package]]\nname = \"sourced\"\nversion = \"0.3.0\"\n\
                    source = \"git+https://example.test/sourced#0123456789abcdef\"\n";
    let (directory, client, server_task) = served_workspace(
        &[
            ("Cargo.toml", manifest),
            ("Cargo.lock", lockfile),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
            (
                "crates/inner/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("crates/inner/src/lib.rs", "pub fn inner_beacon() {}\n"),
            (
                "package.json",
                "{\"dependencies\":{\"tarball\":\"https://example.test/tarball-1.0.0.tgz\"}}\n",
            ),
        ],
        None,
    )
    .await?;

    let answer = get_symbol(&client, json!({"name":"local_beacon","scope":"all"})).await?;

    assert_eq!(answer["hits"][0]["symbol"]["name"], "local_beacon");
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    assert_eq!(warnings[0], json!({"code": "global_access_disabled"}));
    let unavailable: Vec<(String, String, String)> = warnings[1..]
        .iter()
        .map(|warning| {
            (
                warning["entry"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                warning["entry"]["availability"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                warning["reason"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let upvote = |issue: u32| {
        format!(
            "If you're interested in this capability, please upvote it at \
             https://github.com/volarized/rift/issues/{issue}."
        )
    };
    let expected = [
        (
            "outside",
            "path",
            format!(
                "Currently rift doesn't support indexing dependencies by path. {}",
                upvote(392)
            ),
        ),
        (
            "private",
            "private_registry",
            "Currently rift doesn't support indexing dependencies from private registries."
                .to_owned(),
        ),
        (
            "sourced",
            "git",
            format!(
                "Currently rift doesn't support indexing dependencies from git repositories. {}",
                upvote(393)
            ),
        ),
        (
            "tarball",
            "url",
            "Currently rift doesn't support indexing dependencies from URLs.".to_owned(),
        ),
    ]
    .map(|(name, availability, reason)| (name.to_owned(), availability.to_owned(), reason));
    assert_eq!(unavailable, expected, "{answer:#}");
    assert!(
        !answer.to_string().contains("\"inner\""),
        "a path dependency inside the workspace is project source: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A served Rust workspace with no dependency of its own, under `configuration`.
async fn served_probe_workspace(configuration: String) -> TestResult<ServedWorkspace> {
    served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "Cargo.lock",
                "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await
}

/// The entries of every resolution request the fixture received, in arrival order.
async fn resolution_entries(fixture: &GlobalFixture) -> Vec<Vec<Value>> {
    fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.ends_with("/v1/resolutions"))
        .filter_map(|request| request.body)
        .map(|body| body["entries"].as_array().cloned().unwrap_or_default())
        .collect()
}

/// The entries of `entries` naming the `demo` package.
fn demo_entries(entries: &[Value]) -> Vec<&Value> {
    entries
        .iter()
        .filter(|entry| entry["name"] == "demo")
        .collect()
}

/// The versions of the `package_substituted` entries `answer` warns about.
fn substituted_versions(answer: &Value) -> Vec<&Value> {
    answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| warning["code"] == "package_substituted")
        .map(|warning| &warning["entry"]["version"])
        .collect()
}

/// A `packages` entry naming a package the context holds replaces its version for that
/// read alone: the context pins `demo` at a release the collection lacks, which answers
/// from the nearest release with `package_substituted`, and the lookup naming the
/// collected release sends that version in its place and answers from it exactly.
#[tokio::test]
async fn the_package_argument_replaces_a_context_version_for_one_read() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
         [[dependencies.packages]]\nmanager = \"cargo\"\nname = \"demo\"\nversion = \"0.9.0\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let pinned = get_symbol(&client, json!({"name":"helper_beacon","scope":"global"})).await?;
    assert_eq!(
        substituted_versions(&pinned),
        [&json!("0.9.0")],
        "{pinned:#}"
    );

    let replaced = get_symbol(
        &client,
        json!({
            "name": "helper_beacon",
            "scope": "global",
            "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}]
        }),
    )
    .await?;
    let units: Vec<&Value> = replaced["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| &hit["unit"])
        .collect();
    assert_eq!(units, [&json!(COLLECTED_UNIT)], "{replaced:#}");
    assert!(
        substituted_versions(&replaced).is_empty(),
        "the requested release answers exactly: {replaced:#}"
    );

    let resolutions = resolution_entries(&fixture).await;
    assert_eq!(resolutions.len(), 2, "{resolutions:#?}");
    assert_eq!(
        demo_entries(&resolutions[0]),
        [&json!({"manager":"cargo","name":"demo","version":"0.9.0","availability":"canonical"})]
    );
    assert_eq!(
        demo_entries(&resolutions[1]),
        [&json!({"manager":"cargo","name":"demo","version":"1.0.0","availability":"canonical"})],
        "the read sends the requested version in place of the pinned one"
    );
    assert!(
        resolutions[1]
            .iter()
            .any(|entry| entry["manager"] == "stdlib" && entry["name"] == "rust"),
        "the entries the request names nothing about stand: {resolutions:#?}"
    );

    let again = get_symbol(&client, json!({"name":"helper_beacon","scope":"global"})).await?;
    assert_eq!(
        substituted_versions(&again),
        [&json!("0.9.0")],
        "the next read resolves the workspace's own version: {again:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A `packages` entry naming a package the context lacks adds it for that search, and an
/// entry without a version goes out as the requirement `>=0`, which the collection
/// resolves to its release.
#[tokio::test]
async fn the_package_argument_adds_a_package_the_context_lacks() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let answer = call_tool(
        &client,
        "search",
        json!({
            "query": "helper_beacon",
            "scope": "all",
            "packages": [{"manager": "cargo", "name": "demo"}]
        }),
    )
    .await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
        "{answer:#}"
    );

    let resolutions = resolution_entries(&fixture).await;
    assert_eq!(resolutions.len(), 1, "{resolutions:#?}");
    assert_eq!(
        demo_entries(&resolutions[0]),
        [&json!({"manager":"cargo","name":"demo","requirement":">=0","availability":"canonical"})]
    );
    let searches: Vec<Value> = fixture
        .requests()
        .await
        .into_iter()
        .filter(|request| request.uri.contains("/v1/search?"))
        .filter_map(|request| request.body)
        .collect();
    assert!(!searches.is_empty());
    for body in searches {
        assert_eq!(
            body["packages"],
            json!([{"manager":"cargo","name":"demo","version":"1.0.0"}]),
            "the search reads the release the requirement resolved to"
        );
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A read the server refuses spends no global request, whichever route it takes: the page
/// arguments are accepted and the project side reads before the package side is asked, so
/// a zero `limit` is refused, never paged.
#[tokio::test]
async fn a_refused_package_scoped_read_makes_no_global_request() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let refused = [
        (
            "search",
            json!({"query": "beacon", "pattern": "beacon", "scope": "all"}),
            "pattern",
        ),
        (
            "search",
            json!({"query": "beacon", "scope": "all", "limit": 0}),
            "limit",
        ),
        (
            "search",
            json!({"query": "beacon", "scope": "all", "traversal": {"depth": 1}}),
            "seed",
        ),
        (
            "search",
            json!({"pattern": "beacon", "scope": "all", "limit": 0}),
            "limit",
        ),
        (
            "get_symbol",
            json!({"name": "beacon", "scope": "all", "limit": 0}),
            "limit",
        ),
    ];
    for (tool, request, field) in refused {
        let failure = failed_call(client.call_tool(tool_request(tool, &request)).await)?;
        assert_eq!(
            failure.code,
            ErrorCode::InvalidRequest,
            "{request}: {}",
            failure.text
        );
        assert!(
            failure.message.contains(&format!("field {field}")),
            "{request}: {}",
            failure.message
        );
    }
    assert!(fixture.requests().await.is_empty());
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A global API advertising a smaller `dependency_entries_max` than the client's own bound
/// still answers a read: the resolution request holds that many entries, the requested
/// package among them, and the context entries sorted last leave with a
/// `package_context_degraded` warning naming their package manager.
#[tokio::test]
async fn an_advertised_entry_bound_below_the_client_bound_cuts_the_context() -> TestResult {
    use std::fmt::Write as _;

    let advertised = 3;
    let fixture = GlobalFixture::start_with(FixtureOptions {
        dependency_entries_max: advertised,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let mut lockfile =
        "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n".to_owned();
    for index in 0..5 {
        write!(
            lockfile,
            "\n[[package]]\nname = \"pkg-{index:05}\"\nversion = \"1.0.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
        )?;
    }
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("Cargo.lock", lockfile.as_str()),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;

    let request = json!({
        "query": "helper_beacon",
        "scope": "global",
        "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}]
    });
    let answer = call_tool(&client, "search", request).await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
        "{answer:#}"
    );
    let sent = resolution_entries(&fixture).await;
    assert_eq!(sent.len(), 1, "one resolution request: {sent:#?}");
    assert_eq!(sent[0].len(), usize::try_from(advertised)?, "{sent:#?}");
    assert_eq!(demo_entries(&sent[0]).len(), 1, "{sent:#?}");
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let degraded: Vec<&Value> = warnings
        .iter()
        .filter(|warning| {
            warning["code"] == "package_context_degraded" && warning["resolver"] == "cargo"
        })
        .collect();
    assert_eq!(
        degraded,
        [&json!({
            "code": "package_context_degraded",
            "resolver": "cargo",
            "reason": format!(
                "3 of 5 packages were not reported: at most {advertised} are carried per \
                 read, the requested packages first"
            )
        })],
        "{answer:#}"
    );
    let unanswered = [
        "global_api_unavailable",
        "global_publication_incompatible",
        "global_response_invalid",
    ];
    assert!(
        warnings
            .iter()
            .all(|warning| !unanswered.iter().any(|code| warning["code"] == *code)),
        "the global API answered: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A global API advertising fewer entries than a read requests still answers it: every
/// lockfile entry leaves first, then the requested entries sorted last, and
/// `package_context_degraded` names the bound and the requested packages not reported. The
/// read answers the requested package that stayed, and no global warning says the global
/// API failed.
#[tokio::test]
async fn an_advertised_entry_bound_below_the_requested_count_cuts_requested_entries() -> TestResult
{
    let fixture = GlobalFixture::start_with(FixtureOptions {
        dependency_entries_max: 1,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let registry = "source = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
    let lockfile = format!(
        "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n\n\
         [[package]]\nname = \"pkg-00000\"\nversion = \"1.0.0\"\n{registry}\n\
         [[package]]\nname = \"pkg-00001\"\nversion = \"1.0.0\"\n{registry}"
    );
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("Cargo.lock", lockfile.as_str()),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;

    let request = json!({
        "query": "helper_beacon",
        "scope": "global",
        "packages": [
            {"manager": "cargo", "name": "zlib", "version": "1.3.1"},
            {"manager": "cargo", "name": "demo", "version": "1.0.0"}
        ]
    });
    let answer = call_tool(&client, "search", request).await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
        "{answer:#}"
    );
    let sent = resolution_entries(&fixture).await;
    assert_eq!(sent.len(), 1, "one resolution request: {sent:#?}");
    assert_eq!(
        sent[0],
        [json!({
            "availability": "canonical",
            "manager": "cargo",
            "name": "demo",
            "version": "1.0.0"
        })],
        "{sent:#?}"
    );
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let degraded: Vec<&Value> = warnings
        .iter()
        .filter(|warning| {
            warning["code"] == "package_context_degraded" && warning["resolver"] == "cargo"
        })
        .collect();
    assert_eq!(
        degraded,
        [
            &json!({
                "code": "package_context_degraded",
                "resolver": "cargo",
                "reason": "2 of 2 packages were not reported: at most 1 are carried per read, \
                           the requested packages first"
            }),
            &json!({
                "code": "package_context_degraded",
                "resolver": "cargo",
                "reason": "1 of 2 requested packages were not reported: at most 1 are carried \
                           per read, and every other package left first"
            })
        ],
        "{answer:#}"
    );
    let unanswered = [
        "global_api_unavailable",
        "global_publication_incompatible",
        "global_response_invalid",
    ];
    assert!(
        warnings
            .iter()
            .all(|warning| !unanswered.iter().any(|code| warning["code"] == *code)),
        "the global API answered: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A workspace whose lockfile fills the dependency context to its bound still answers a
/// read naming one more package: the requested package goes out, the context entry sorted
/// last leaves with a `package_context_degraded` warning naming its package manager, and
/// the resolution request stays within the entry bound the client holds it to.
#[tokio::test]
async fn a_requested_package_past_the_entry_bound_displaces_a_context_entry() -> TestResult {
    use std::fmt::Write as _;

    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let bound = rift_dependency::PACKAGES_MAX;
    let mut lockfile =
        "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n".to_owned();
    for index in 0..bound {
        write!(
            lockfile,
            "\n[[package]]\nname = \"pkg-{index:05}\"\nversion = \"1.0.0\"\n\
             source = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
        )?;
    }
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            ("Cargo.lock", lockfile.as_str()),
            ("src/lib.rs", "pub fn local_beacon() {}\n"),
        ],
        Some(configuration),
    )
    .await?;

    let request = json!({
        "query": "helper_beacon",
        "scope": "global",
        "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}]
    });
    let answer = call_tool(&client, "search", request).await?;
    assert!(
        answer["results"]
            .as_array()
            .is_some_and(|results| results.iter().any(|hit| hit["unit"] == COLLECTED_UNIT)),
        "{answer:#}"
    );
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let degraded: Vec<&Value> = warnings
        .iter()
        .filter(|warning| warning["code"] == "package_context_degraded")
        .collect();
    assert_eq!(
        degraded,
        [&json!({
            "code": "package_context_degraded",
            "resolver": "cargo",
            "reason": format!(
                "1 of {bound} packages were not reported: at most {bound} are carried per \
                 read, the requested packages first"
            )
        })],
        "{answer:#}"
    );
    let unanswered = [
        "global_api_unavailable",
        "global_publication_incompatible",
        "global_response_invalid",
    ];
    assert!(
        warnings
            .iter()
            .all(|warning| !unanswered.iter().any(|code| warning["code"] == *code)),
        "the global API answered: {answer:#}"
    );

    let resolutions = resolution_entries(&fixture).await;
    assert_eq!(resolutions.len(), 1);
    let sent = &resolutions[0];
    assert_eq!(sent.len(), rift_cloud_client::DEPENDENCY_ENTRIES_MAX);
    assert_eq!(
        demo_entries(sent).len(),
        1,
        "the requested package goes out"
    );
    let last = format!("pkg-{:05}", bound - 1);
    assert!(
        sent.iter().all(|entry| entry["name"] != last.as_str()),
        "the entry sorted last leaves"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A global API whose capabilities advertise none of `patterns`, `documentation_search`,
/// and `symbol_documentation` still answers the reads that need them: each carries the
/// project hits and one `capability_unavailable` warning naming the feature, and the
/// client asks the service for no read it does not serve.
#[tokio::test]
async fn an_unadvertised_feature_answers_project_hits_with_capability_unavailable() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        withheld_features: &["patterns", "documentation_search", "symbol_documentation"],
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;

    let reads = [
        (
            "search",
            json!({"pattern": "beacon", "scope": "all"}),
            "results",
            "patterns",
        ),
        (
            "search",
            json!({"query": "beacon", "scope": "all"}),
            "results",
            "documentation_search",
        ),
        (
            "get_symbol",
            json!({"name": "beacon", "scope": "all", "include": ["documentation"]}),
            "hits",
            "symbol_documentation",
        ),
    ];
    for (tool, request, hits, feature) in reads {
        let answer = call_tool(&client, tool, request.clone()).await?;
        let project_hits = answer[hits].as_array().ok_or("hits are an array")?;
        assert!(!project_hits.is_empty(), "{request}: {answer:#}");
        assert!(
            project_hits.iter().all(|hit| hit.get("unit").is_none()),
            "no package answers: {answer:#}"
        );
        let warnings = answer["warnings"]
            .as_array()
            .ok_or("warnings are an array")?;
        let capability: Vec<&Value> = warnings
            .iter()
            .filter(|warning| warning["code"] == "global_page_warning")
            .collect();
        assert_eq!(
            capability,
            [&json!({
                "code": "global_page_warning",
                "warning_code": "capability_unavailable",
                "detail": format!(
                    "the global API does not advertise the `{feature}` feature, so no \
                     package answers this read"
                )
            })],
            "{request}: {answer:#}"
        );
        assert!(
            !answer.to_string().contains("global_response_invalid"),
            "{request}: {answer:#}"
        );
    }
    let reads_asked: Vec<String> = fixture
        .requests()
        .await
        .into_iter()
        .map(|request| request.uri)
        .filter(|uri| !uri.ends_with("/v1/capabilities") && !uri.ends_with("/v1/resolutions"))
        .collect();
    assert!(reads_asked.is_empty(), "{reads_asked:#?}");
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A package declaration the global index matched inside its body, which the page names
/// with the `file_content` field and the `unknown` match class, answers as a `content`
/// match beside no global warning.
#[tokio::test]
async fn a_body_matched_package_declaration_answers_as_a_content_match() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let answer = call_tool(
        &client,
        "search",
        json!({"query": BODY_MATCH_QUERY, "scope": "global"}),
    )
    .await?;
    let package_hits: Vec<&Value> = answer["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hit| hit["unit"] == COLLECTED_UNIT)
        .collect();
    assert_eq!(package_hits.len(), 1, "{answer:#}");
    assert_eq!(
        package_hits[0]["matched_by"],
        json!(["content"]),
        "{answer:#}"
    );
    assert!(
        !answer.to_string().contains("\"global_"),
        "the page passes the client: {answer:#}"
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// `packages` beside a package-scoped `pattern` matches the pattern over the requested
/// release: a workspace depending on nothing names `demo` for one search, and the pattern
/// request carries it and answers its matches.
#[tokio::test]
async fn a_pattern_beside_packages_matches_the_requested_release() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let answer = call_tool(
        &client,
        "search",
        json!({
            "pattern": r"fn helper_\w+",
            "scope": "global",
            "target": "all",
            "include": ["source"],
            "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}]
        }),
    )
    .await?;
    assert_eq!(
        answer["results"],
        helper_beacon_pattern_hits(),
        "{answer:#}"
    );

    let bodies: Vec<Value> = pattern_requests(&fixture)
        .await
        .into_iter()
        .filter_map(|request| request.body)
        .collect();
    assert_eq!(
        bodies,
        [json!({
            "pattern": r"fn helper_\w+",
            "packages": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}],
            "include": ["source"]
        })]
    );
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Exact original spelling wins before full-text frequency chooses a different spelling.
#[tokio::test]
async fn test_identifier_case_precedes_documentation_frequency() -> TestResult {
    let source = "/// searchHit searchHit searchHit searchHit\npub fn SearchHit() {}\npub fn searchHit() {}\n";
    let (_directory, client, server_task) =
        served_workspace(&[("src/lib.rs", source)], None).await?;
    let answer = call_tool(
        &client,
        "search",
        json!({"query": "searchHit", "limit": 1, "include": ["source"]}),
    )
    .await?;
    assert_eq!(
        answer["results"][0]["hit"]["symbol"]["name"], "searchHit",
        "{answer:#}"
    );
    assert_eq!(
        answer["results"][0]["source"], "pub fn searchHit() {}",
        "{answer:#}"
    );
    let answer = call_tool(
        &client,
        "search",
        json!({"query": "searchHit", "limit": 10}),
    )
    .await?;
    let wrong_case = answer["results"]
        .as_array()
        .ok_or("search carries results")?
        .iter()
        .find(|hit| hit["hit"]["symbol"]["name"] == "SearchHit")
        .ok_or("full-text search keeps the other spelling")?;
    assert!(
        wrong_case["matched_by"]
            .as_array()
            .is_some_and(|fields| fields.contains(&json!("documentation"))),
        "{answer:#}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Exact original spelling wins through MCP before a one-result page is selected (#597).
#[tokio::test]
async fn test_identifier_case_pairs_rank_exact_spelling_through_mcp() -> TestResult {
    let fixture = GlobalFixture::start(SymbolFixture::CasePairs).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let source = global_api::CASE_SOURCE;
    let (_directory, client, server_task) =
        served_workspace(&[("src/lib.rs", source)], Some(configuration)).await?;
    for scope in ["local", "global", "all"] {
        for name in [
            "SearchHit",
            "searchHit",
            "createProgram",
            "createprogram",
            "load_config",
            "Load_config",
        ] {
            assert_original_identifier(&client, scope, name).await?;
        }
        let args =
            json!({"query": "SEARCHHIT", "scope": scope, "limit": 10, "include": ["source"]});
        let answer = call_tool(&client, "search", args).await?;
        let hits = answer["results"]
            .as_array()
            .ok_or("search carries results")?;
        let names: Vec<_> = hits
            .iter()
            .filter_map(|hit| hit["hit"]["symbol"]["name"].as_str())
            .collect();
        assert!(
            names.contains(&"SearchHit") && names.contains(&"searchHit"),
            "{answer:#}"
        );
    }
    let args = json!({"query": "SearchHit and searchHit", "scope": "global", "limit": 10});
    let answer = call_tool(&client, "search", args).await?;
    assert_eq!(
        answer["results"]
            .as_array()
            .ok_or("search carries results")?
            .len(),
        2,
        "{answer:#}"
    );
    let requests = search_requests(&fixture).await;
    let identifiers = requests
        .iter()
        .filter_map(|request| request.body.as_ref())
        .find(|body| body["query"] == "SearchHit and searchHit" && body["phase"] == "precise")
        .ok_or("both original candidates reach the global request")?;
    assert_eq!(
        identifiers["identifiers"],
        json!(["SearchHit", "searchHit"])
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Checks search and symbol lookup spelling, source, and identity on one page.
async fn assert_original_identifier(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    scope: &str,
    name: &str,
) -> TestResult {
    let args = json!({"query": name, "scope": scope, "limit": 1, "include": ["source"]});
    let answer = call_tool(client, "search", args).await?;
    let hits = answer["results"]
        .as_array()
        .ok_or("search carries results")?;
    assert_eq!(hits.len(), 1, "{scope}: {answer:#}");
    let hit = &hits[0];
    assert_eq!(hit["hit"]["symbol"]["name"], name, "{scope}: {answer:#}");
    let expected_source = format!("pub fn {name}() {{}}");
    assert_eq!(hit["source"], expected_source, "{scope}: {answer:#}");
    let id = hit["hit"]["symbol"]["id"]
        .as_str()
        .ok_or("symbol carries identity")?;
    assert!(id.ends_with(&format!("/{name}")), "{id}");
    let args = json!({"name": name, "scope": scope, "limit": 1, "include": ["source"]});
    let answer = get_symbol(client, args).await?;
    assert_eq!(
        answer["hits"][0]["symbol"]["name"], name,
        "{scope}: {answer:#}"
    );
    assert_eq!(
        answer["hits"][0]["source"], expected_source,
        "{scope}: {answer:#}"
    );
    Ok(())
}

/// Native validation accepts both original candidates and legacy normalized candidates (#597).
#[tokio::test]
async fn test_native_client_accepts_original_and_normalized_identifier_candidates() -> TestResult {
    use rift_cloud_client::{Config, GlobalClient, QueryTerm};
    let fixture = GlobalFixture::start(SymbolFixture::CasePairs).await?;
    let client = GlobalClient::new(Config {
        endpoint: fixture.endpoint.clone(),
        ..Config::default()
    })?;
    let mut request = case_search_request("SearchHit and searchHit")?;
    for identifiers in [vec!["SearchHit", "searchHit"], vec!["searchhit"]] {
        request.identifiers = identifiers.into_iter().map(str::to_owned).collect();
        let page = client.search_packages(&request, 10, None).await?;
        assert_eq!(page.items.len(), 2);
    }
    for name in ["SearchHit", "searchHit", "createProgram", "load_config"] {
        request.query = name.to_owned();
        request.terms = vec![QueryTerm {
            text: name.to_owned(),
            phrase: false,
            prefix: false,
        }];
        request.identifiers = vec![name.to_owned()];
        let page = client.search_packages(&request, 1, None).await?;
        let hit = rift_cloud_client::PackageSearchCandidate::try_from(page.items[0].clone())?;
        let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &hit.hit.hit else {
            return Err("search returns a declaration".into());
        };
        assert_eq!(symbol.name, name);
        assert_eq!(
            hit.hit.source.as_deref(),
            Some(format!("pub fn {name}() {{}}").as_str())
        );
    }
    Ok(())
}

/// Builds the package request from the same parser the MCP path uses.
fn case_search_request(query: &str) -> TestResult<rift_cloud_client::PackageSearchRequest> {
    use rift_cloud_client::{PackageSearchRequest, PackageSearchRequestPhase, QueryTerm};
    let parsed = rift_ranking::ParsedQuery::parse(query)?;
    Ok(PackageSearchRequest {
        query: query.to_owned(),
        terms: parsed
            .members()
            .iter()
            .map(|member| QueryTerm {
                text: member.text().to_owned(),
                phrase: member.is_phrase(),
                prefix: false,
            })
            .collect(),
        identifiers: parsed
            .candidates()
            .iter()
            .map(|candidate| candidate.text().to_owned())
            .collect(),
        include: Some(vec!["source".to_owned()]),
        packages: vec![rift_cloud_client::PackageIdentity {
            manager: "cargo".to_owned(),
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
        }],
        phase: PackageSearchRequestPhase::Precise,
        target: None,
    })
}

/// Qualified-name comparison keeps original spelling through the local caller surface (#597).
#[tokio::test]
async fn test_qualified_identifier_case_ranks_original_spelling() -> TestResult {
    let source =
        "pub struct Vault;\nimpl Vault { pub fn createProgram() {} pub fn createprogram() {} }\n";
    let (_directory, client, server_task) =
        served_workspace(&[("src/lib.rs", source)], None).await?;
    for name in ["createProgram", "createprogram"] {
        let args = json!({"query": format!("Vault::{name}"), "limit": 1, "include": ["source"]});
        let answer = call_tool(&client, "search", args).await?;
        assert_eq!(
            answer["results"][0]["hit"]["symbol"]["name"], name,
            "{answer:#}"
        );
        assert_eq!(
            answer["results"][0]["source"],
            format!("pub fn {name}() {{}}"),
            "{answer:#}"
        );
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
