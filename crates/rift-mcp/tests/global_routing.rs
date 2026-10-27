//! Global package routing through served `get_symbol` and `search` reads: the project hits
//! come from the served snapshot, the package hits from a fixture global API, and every
//! context entry no public registry serves answers with its typed warning.

#[allow(
    dead_code,
    reason = "shared integration fixture exposes helpers this suite does not use"
)]
mod fake_engine;

#[path = "announced_work/mod.rs"]
mod announced_work;
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

async fn complete_fixture(symbol: SymbolFixture) -> TestResult<GlobalFixture> {
    Ok(GlobalFixture::start_with(FixtureOptions {
        symbol,
        query_narrowed: false,
        ..FixtureOptions::default()
    })
    .await?)
}

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
    result.structured_content.ok_or_else(|| {
        format!(
            "{name} returned no structured content: {:?}",
            result.content
        )
        .into()
    })
}

async fn search_symbols(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: Value,
) -> TestResult<Value> {
    let request = serde_json::from_value::<rift_protocol::read::SearchParams>(args)?;
    let query = workspace_client::query::retrieval(&request)?;
    call_tool(client, "search", serde_json::to_value(query)?).await
}

async fn ranked_search_symbols(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: Value,
) -> TestResult<Value> {
    let request = serde_json::from_value::<rift_protocol::read::SearchParams>(args)?;
    let query = workspace_client::query::retrieval(&request)?;
    workspace_client::search_after_population(client, &query).await
}

async fn pattern_with_view(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    args: Value,
) -> TestResult<Value> {
    let request = serde_json::from_value::<rift_protocol::read::SearchParams>(args)?;
    let query = workspace_client::query::projected_retrieval(
        &request,
        "?upstream ?upstream_expiry",
        "?hit :view/:id ?upstream; :view/:expires_at ?upstream_expiry .",
    )?;
    call_tool(client, "search", serde_json::to_value(query)?).await
}

async fn assert_pattern_view_readback(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &GlobalFixture,
    answer: &Value,
) -> TestResult {
    for row in workspace_client::query::rows(answer)? {
        assert_eq!(
            workspace_client::query::literal(row, "upstream_expiry")?.parse::<jiff::Timestamp>()?,
            view_expiry(fixture.captured_view())?
        );
        assert_eq!(
            workspace_client::query::literal(row, "upstream")?,
            "a".repeat(64)
        );
    }
    let view = answer["view"]["id"]
        .as_str()
        .ok_or("captured pattern view")?;
    let source = call_tool(
        client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({"unit":COLLECTED_UNIT,"view":view}),
        )?)?)?,
    )
    .await?;
    let [source_row] = workspace_client::query::rows(&source)? else {
        return Err(format!("source query must return one original result: {source:#}").into());
    };
    assert_eq!(
        workspace_client::query::literal(source_row, "outcome")?,
        "found"
    );
    assert_eq!(
        workspace_client::query::iri(source_row, "unit")?,
        COLLECTED_UNIT
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "complete")?,
        "true"
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_view")?,
        view
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("pattern expiry")?
    );
    assert!(
        workspace_client::query::literal(source_row, "text")?.contains("pub fn helper_beacon() {}")
    );
    let symbol = global_api::collected_identity(SymbolFixture::Valid, "helper_beacon");
    let exact = call_tool(
        client,
        "get_symbol",
        json!({"id":symbol,"view":view,"include":["source"]}),
    )
    .await?;
    assert_eq!(exact["outcome"], "found", "{exact:#}");
    assert_eq!(exact["view"], answer["view"]);
    assert_eq!(
        exact["declarations"][0]["source"],
        "pub fn helper_beacon() {}"
    );
    for request in fixture
        .requests()
        .await
        .iter()
        .filter(|request| request.uri.contains("/symbols") || request.uri.contains("/source"))
    {
        assert_eq!(
            request.body.as_ref().ok_or("exact read body")?["view"],
            "a".repeat(64)
        );
    }
    Ok(())
}

async fn read_producer(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &'static str,
    args: Value,
) -> TestResult<Value> {
    if name == "search" {
        search_symbols(client, args).await
    } else {
        call_tool(client, name, args).await
    }
}

fn literals<'a>(answer: &'a Value, field: &str) -> TestResult<Vec<&'a str>> {
    workspace_client::query::rows(answer)?
        .iter()
        .map(|row| workspace_client::query::literal(row, field))
        .collect()
}

fn view_expiry(view: &Value) -> TestResult<jiff::Timestamp> {
    Ok(view["expires_at"].as_str().ok_or("view expiry")?.parse()?)
}

fn assert_unavailable(answer: &Value) {
    assert_eq!(answer["outcome"], "unavailable", "{answer:#}");
    assert!(answer.get("answer").is_none(), "{answer:#}");
}

fn has_unit(answer: &Value, unit: &str) -> TestResult<bool> {
    Ok(workspace_client::query::rows(answer)?
        .iter()
        .any(|row| workspace_client::query::iri(row, "unit").is_ok_and(|value| value == unit)))
}

/// The configured package list naming the collected `demo` release. The fixture tables
/// already open `[dependencies]`, so the list lands as an array of tables below it.
const DEMO_PACKAGE: &str = "[[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"demo\"\nversion = \"1.0.0\"\n";

/// The configured package list naming `demo`, an exact `absent` the fixture collection
/// lacks, and a `wanted` requirement it cannot resolve.
const THREE_PACKAGES: &str = "[[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"demo\"\nversion = \"1.0.0\"\n\n\
     [[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"absent\"\nversion = \"1.0.0\"\n\n\
     [[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"wanted\"\nrequirement = \"^1\"\n";

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
    let answer = search_symbols(
        &client,
        json!({"target":"symbol", "query":"local_beacon","scope":"global"}),
    )
    .await?;
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
    assert_eq!(answer["outcome"], "unavailable");
    assert!(answer.get("answer").is_none());
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
    let params = serde_json::from_value(json!({
        "target":"symbol", "query":"local_beacon", "scope":"global"
    }))?;
    let query = workspace_client::query::retrieval(&params)?;
    let failure = failed_call(
        client
            .call_tool(tool_request("search", &serde_json::to_value(query)?))
            .await,
    )?;
    let elapsed = started.elapsed();
    assert!(
        elapsed < RESOLUTION_DELAY,
        "the request deadline must end the resolution wait: elapsed={elapsed:?}"
    );
    assert_eq!(
        failure.code,
        ErrorCode::TemporarilyUnavailable,
        "{}",
        failure.text
    );
    assert!(
        fixture
            .requests()
            .await
            .iter()
            .any(|request| request.uri.ends_with("/resolutions"))
    );
    assert_eq!(fixture.requests().await.len(), 2);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// How long the fixture holds a symbol or search page: past the one-second request
/// deadline, however slow the machine running the test.
const READ_DELAY: Duration = Duration::from_secs(10);

/// A package read past the request deadline refuses the complete search before its held page arrives.
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
            "search",
            json!({"query": "beacon", "target": "symbol", "scope": "all"}),
        ),
        ("search", json!({"query": "beacon", "scope": "all"})),
    ];
    for (tool, args) in reads {
        let started = tokio::time::Instant::now();
        let params = serde_json::from_value(args)?;
        let query = workspace_client::query::retrieval(&params)?;
        let failure = failed_call(
            client
                .call_tool(tool_request(tool, &serde_json::to_value(query)?))
                .await,
        )?;
        let elapsed = started.elapsed();
        assert!(
            elapsed < READ_DELAY,
            "{tool}: the request deadline must end the read wait: elapsed={elapsed:?}"
        );
        assert_eq!(
            failure.code,
            ErrorCode::TemporarilyUnavailable,
            "{tool}: {}",
            failure.text
        );
        assert!(fixture.requests().await.iter().any(|request| {
            request.uri.contains("/search?") || request.uri.ends_with("/search")
        }));
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
/// under: the scripted engine's walk fits in the nine tenths of it the walk may spend,
/// and the held answer outlasts all of it.
const CALLEE_WALK_BUDGET: &str = "5s";

/// A module whose `counted` calls the standard library's `len`, the callee an outgoing
/// walk asks the global API to name.
const CALLEE_FILES: &[(&str, &str)] = &[
    (
        "pyproject.toml",
        "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules=['app']\n",
    ),
    ("app.py", "def counted() -> int:\n    return len([1])\n"),
];

/// Confirms the local engine answers callers before a deadline test waits on the global API.
async fn await_local_caller(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &GlobalFixture,
) -> TestResult {
    let incoming = serde_json::to_value(workspace_client::query::path(
        "rift://symbol/local/rust/walk/beacon",
        "^:references",
    )?)?;
    let answer = call_retrying_acceptance(client, tool_request("search", &incoming)).await?;
    let results = workspace_client::query::rows(&answer)?;
    assert_eq!(results.len(), 1, "{answer:#}");
    assert_eq!(
        workspace_client::query::iri(&results[0], "symbol")?,
        "rift://symbol/local/rust/walk/caller",
        "{answer:#}"
    );
    if let Some(warnings) = answer.get("warnings") {
        let warnings = warnings.as_array().ok_or("warnings are an array")?;
        assert!(
            warnings.iter().all(|warning| !warning["code"]
                .as_str()
                .is_some_and(|code| code.starts_with("engine_"))),
            "the local engine answers without warnings: {answer:#}"
        );
    }
    assert!(
        fixture.requests().await.is_empty(),
        "the local caller walk makes no global request"
    );
    Ok(())
}

async fn assert_admitted_outgoing_source(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &GlobalFixture,
    callee: &CalleePosition,
    runtime: bool,
) -> TestResult {
    let (callee_uri, callee_line, callee_start, _, callee_name) = callee;
    let (callee_line, callee_start, callee_name) = (*callee_line, *callee_start, *callee_name);
    let original_source = if runtime {
        let path = reqwest::Url::parse(callee_uri)?
            .to_file_path()
            .map_err(|()| "the runtime fixture has an absolute source path")?;
        fs::read_to_string(path)?
    } else {
        "function beacon() {}\n".to_owned()
    };
    let name_start = original_source
        .split_inclusive('\n')
        .take(callee_line)
        .map(str::len)
        .sum::<usize>()
        + callee_start;
    let answer = call_tool(
        client,
        "search",
        json!({"query":
            "SELECT ?name ?unit ?start ?end ?source { \
             <rift://symbol/local/rust/walk/caller> :calls ?callee . \
             ?callee :name ?name; :declarations ?declaration . \
             ?declaration :unit ?unit; :range ?range . \
             ?range :start ?start; :end ?end . ?unit :source ?source }"
        }),
    )
    .await?;
    let rows = workspace_client::query::rows(&answer)?;
    assert_eq!(rows.len(), 1, "{answer:#}");
    let row = &rows[0];
    assert_eq!(workspace_client::query::literal(row, "name")?, callee_name);
    let start = workspace_client::query::literal(row, "start")?.parse::<usize>()?;
    let end = workspace_client::query::literal(row, "end")?.parse::<usize>()?;
    assert!(start <= name_start && name_start + callee_name.len() <= end);
    assert!(original_source.get(start..end).is_some());
    if !runtime {
        assert_eq!((start, end), (0, 20));
    }
    assert_eq!(
        workspace_client::query::literal(row, "source")?,
        original_source
    );
    let unit = workspace_client::query::iri(row, "unit")?;
    let (owner, path) = rift_protocol::identity::parse_source_unit_identity(unit)?;
    if runtime {
        assert_eq!(path.as_str(), "core/src/cmp.rs");
        assert!(matches!(
            owner,
            Some(rift_protocol::identity::SymbolOwner::Runtime { runtime, version })
            if runtime == "rustc" && version.starts_with("1.98.")
        ));
    } else {
        assert_eq!(path.as_str(), "index.js");
        assert_eq!(
            owner,
            Some(rift_protocol::identity::SymbolOwner::Package {
                manager: "npm".to_owned(),
                registry: "npmjs.org".to_owned(),
                name: "demo".to_owned(),
                version: "1.0.0".to_owned(),
            })
        );
    }
    assert_admitted_source_read(client, &answer, unit, &original_source).await?;
    let requests = fixture.requests().await;
    assert!(
        requests
            .iter()
            .all(|request| request.uri.ends_with("/capabilities")),
        "admitted source facts require no global naming request: {requests:#?}"
    );
    Ok(())
}

async fn assert_admitted_source_read(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    answer: &Value,
    unit: &str,
    original_source: &str,
) -> TestResult {
    let source = call_tool(
        client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({"unit":unit,"view":answer["view"]["id"]}),
        )?)?)?,
    )
    .await?;
    let [source_row] = workspace_client::query::rows(&source)? else {
        return Err(format!("source query must return one original result: {source:#}").into());
    };
    assert_eq!(
        workspace_client::query::literal(source_row, "outcome")?,
        "found"
    );
    assert_eq!(workspace_client::query::iri(source_row, "unit")?, unit);
    assert_eq!(
        workspace_client::query::literal(source_row, "text")?,
        original_source
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "complete")?,
        "true"
    );
    assert_eq!(workspace_client::query::literal(source_row, "start")?, "0");
    assert_eq!(
        workspace_client::query::literal(source_row, "end")?.parse::<usize>()?,
        original_source.len()
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_view")?,
        answer["view"]["id"]
            .as_str()
            .ok_or("original outgoing view")?
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("original outgoing expiry")?
    );
    Ok(())
}

/// An outgoing walk keeps admitted source facts while a global answer is held.
async fn outgoing_walk_past_a_held_global_answer(hold: Hold) -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        hold: Some(hold),
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[server]\nreadiness_timeout = \"{CALLEE_WALK_BUDGET}\"\n\n\
         [global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"30s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let runtime = matches!(hold, Hold::Declarations(_));
    let callee = if runtime {
        runtime_callee_position()?
    } else {
        (
            "${uri%/lib.rs}/node_modules/demo/index.js".to_owned(),
            0,
            9,
            15,
            "beacon",
        )
    };
    let (callee_uri, callee_line, callee_start, callee_end, callee_name) = &callee;
    let source_uri =
        "\\\"uri\\\":\\\"$uri\\\",\\\"range\\\":{\\\"start\\\":{\\\"line\\\":CALLEE_LINE";
    let scripted_uri = format!(
        "\\\"uri\\\":\\\"{callee_uri}\\\",\\\"range\\\":{{\\\"start\\\":{{\\\"line\\\":CALLEE_LINE"
    );
    let script = fake_engine::SCRIPTED_CALLS_ENGINE
        .replace(source_uri, &scripted_uri)
        .replace(
            "\\\"name\\\":\\\"beacon\\\"",
            &format!("\\\"name\\\":\\\"{callee_name}\\\""),
        )
        .replace(
            "\\\"selectionRange\\\":{\\\"start\\\":{\\\"line\\\":CALLEE_LINE,\\\"character\\\":7},\\\"end\\\":{\\\"line\\\":CALLEE_LINE,\\\"character\\\":13}}",
            &format!("\\\"selectionRange\\\":{{\\\"start\\\":{{\\\"line\\\":CALLEE_LINE,\\\"character\\\":{callee_start}}},\\\"end\\\":{{\\\"line\\\":CALLEE_LINE,\\\"character\\\":{callee_end}}}}}"),
        )
        .replace("PROGRESS", announced_work::ANNOUNCED_WORK)
        .replace("CALLEE_LINE", &callee_line.to_string())
        .replace("CALLER_LINE", "3");
    let (_engine, engine_configuration) = fake_engine::rust_engine(&script)?;
    let configuration = format!("{configuration}\n{engine_configuration}");
    let mut files = vec![
        (
            "Cargo.toml",
            "[package]\nname = \"walk\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\npath = \"lib.rs\"\n",
        ),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"1.98\"\n"),
        (
            "lib.rs",
            "pub fn beacon() {}\n\npub fn caller() {\n    std::cmp::max(1, 2);\n}\n",
        ),
    ];
    if !runtime {
        files.extend([
            ("package.json", r#"{"name":"walk","dependencies":{"demo":"1.0.0"}}"#),
            (".npmrc", "registry=https://registry.npmjs.org/\n"),
            ("package-lock.json", r#"{"packages":{"node_modules/demo":{"version":"1.0.0","resolved":"https://registry.npmjs.org/demo/-/demo-1.0.0.tgz"}}}"#),
            ("node_modules/demo/package.json", r#"{"name":"demo","version":"1.0.0"}"#),
            ("node_modules/demo/index.js", "function beacon() {}\n"),
        ]);
    }
    let (_directory, client, server_task) =
        workspace_client::served_prepared_workspace(&files, Some(configuration), |root| {
            if runtime {
                enable_runtime_resolution(root);
            }
        })
        .await?;
    await_local_caller(&client, &fixture).await?;
    assert_admitted_outgoing_source(&client, &fixture, &callee, runtime).await?;
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn enable_runtime_resolution(root: &std::path::Path) {
    let path = root.join("rift.toml");
    let text = fs::read_to_string(&path).expect("the fixture configuration reads");
    fs::write(
        path,
        text.replace("resolution = \"static\"", "resolution = \"auto\""),
    )
    .expect("the fixture enables the existing runtime resolver");
}

type CalleePosition = (String, usize, usize, usize, &'static str);

fn runtime_callee_position() -> TestResult<CalleePosition> {
    let sysroot = std::process::Command::new("rustup")
        .args(["run", "1.98", "rustc", "--print", "sysroot"])
        .output()?;
    assert!(
        sysroot.status.success(),
        "the installed runtime names its sysroot"
    );
    let source = std::path::PathBuf::from(String::from_utf8(sysroot.stdout)?.trim())
        .join("lib/rustlib/src/rust/library/core/src/cmp.rs");
    let text = fs::read_to_string(&source)?;
    let (line, declaration) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.starts_with("pub ") && line.contains("fn max<"))
        .ok_or("the installed runtime source declares max")?;
    let start = declaration
        .find("fn max<")
        .ok_or("the runtime function name")?
        + 3;
    Ok((
        reqwest::Url::from_file_path(&source)
            .map_err(|()| "the installed runtime source has an absolute path")?
            .to_string(),
        line,
        start,
        start + "max".len(),
        "max",
    ))
}

/// An admitted call target retains its original source without global naming.
#[tokio::test]
async fn an_admitted_call_target_keeps_physical_facts_without_global_resolution() -> TestResult {
    outgoing_walk_past_a_held_global_answer(Hold::Resolution(CALLEE_HOLD)).await
}

/// An admitted runtime call target retains its original source without global naming.
#[tokio::test]
async fn an_admitted_runtime_call_target_keeps_physical_facts_without_global_naming() -> TestResult
{
    outgoing_walk_past_a_held_global_answer(Hold::Declarations(CALLEE_HOLD)).await
}

/// A Python version pin does not establish an installed runtime owner for bundled stubs.
#[tokio::test]
async fn unversioned_python_runtime_positions_remain_unasked() -> TestResult {
    let fixture =
        GlobalFixture::start_holding(SymbolFixture::Valid, Some(Hold::Declarations(CALLEE_HOLD)))
            .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\n\
         [languages.python.lsp]\nembedded = \"ty\"\n",
        fixture.endpoint
    );
    let mut files = CALLEE_FILES.to_vec();
    files.push((".python-version", "3.12.3\n"));
    let (_directory, client, server_task) = served_workspace(&files, Some(configuration)).await?;
    let walk = serde_json::to_value(workspace_client::query::path(
        "rift://symbol/local/python/app/counted",
        ":calls",
    )?)?;
    let failure = failed_call(client.call_tool(tool_request("search", &walk)).await)?;
    assert_eq!(
        failure.code,
        ErrorCode::TemporarilyUnavailable,
        "{}",
        failure.text
    );
    let requests = fixture.requests().await;
    assert!(
        requests.len() <= 1,
        "one operation metadata read: {requests:#?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| request.uri.ends_with("/capabilities")),
        "unversioned positions make no resolution, declaration or relationship read: {requests:#?}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
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
    let answer = search_symbols(
        &client,
        json!({"target":"symbol", "query":"\"helper_beacon\"","scope":"all"}),
    )
    .await?;
    assert_unavailable(&answer);
    let (warnings, page_warnings): (Vec<&Value>, Vec<&Value>) = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| warning["code"] != "lexical_ranking_unavailable")
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
        [&json!("query_narrowed")]
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
        json!({"manager":"cargo", "registry": "crates.io","name":"absent","version":"1.0.0"})
    );
    let requirements: Vec<&Value> = warnings[2..4]
        .iter()
        .map(|warning| &warning["entry"])
        .collect();
    assert_eq!(
        requirements,
        [
            &json!({
                "manager":"cargo", "registry": "crates.io",
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

/// A substituted release cannot answer the selected domain. The unavailable answer names
/// both an absent exact version and a requirement the collected release lies outside.
#[tokio::test]
async fn a_nearest_release_answers_and_names_the_substitution() -> TestResult {
    let cases = [
        (
            "version = \"1.0.3\"".to_owned(),
            json!({"manager":"cargo", "registry": "crates.io","name":"demo","version":"1.0.3","availability":"canonical"}),
        ),
        (
            format!("requirement = \"{UNSATISFIED_REQUIREMENT}\""),
            json!({
                "manager":"cargo", "registry": "crates.io",
                "name":"demo",
                "requirement":UNSATISFIED_REQUIREMENT,
                "availability":"canonical"
            }),
        ),
    ];
    for (selector, entry) in cases {
        let fixture = complete_fixture(SymbolFixture::Valid).await?;
        let configuration = format!(
            "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
             request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
             [[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"demo\"\n{selector}\n",
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
        let answer = search_symbols(
            &client,
            json!({"target":"symbol", "query":"helper_beacon","scope":"global"}),
        )
        .await?;
        assert_unavailable(&answer);
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
                "package": {"manager":"cargo", "registry": "crates.io","name":"demo","version":"1.0.0"}
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
async fn remote_phases_keep_request_fields_when_context_is_missing() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{THREE_PACKAGES}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = search_symbols(
        &client,
        json!({"query":"helper_beacon demo","scope":"global"}),
    )
    .await?;
    assert_unavailable(&answer);
    assert!(answer["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|warning| warning["code"] == "package_absent")
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
            json!([{"manager":"cargo", "registry": "crates.io","name":"demo","version":"1.0.0"}])
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

/// Ordinary prose keeps the broad full-text order before the requested result limit.
#[tokio::test]
async fn test_global_prose_search_preserves_broad_full_text_order() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    for limit in [1_usize, 2] {
        let request = json!({
            "query": global_api::BROAD_BODY_MATCH_QUERY,
            "scope": "global",
            "limit": limit,
            "include": ["source", "score"]
        });
        let answer = search_symbols(&client, request).await?;
        let results = workspace_client::query::rows(&answer)?;
        assert_eq!(results.len(), limit, "{answer:#}");
        for (hit, name) in results.iter().zip(["helper_beacon", "beacon"]) {
            assert_eq!(
                workspace_client::query::literal(hit, "name")?,
                name,
                "{answer:#}"
            );
            let identity = global_api::collected_identity(SymbolFixture::Valid, name);
            assert_eq!(
                workspace_client::query::iri(hit, "symbol")?,
                identity.as_str(),
                "{answer:#}"
            );
            assert_eq!(
                workspace_client::query::literal(hit, "source")?,
                format!("pub fn {name}() {{}}"),
                "{answer:#}"
            );
            assert!(
                workspace_client::query::literal(hit, "score")?
                    .parse::<f64>()
                    .is_ok(),
                "{answer:#}"
            );
        }
    }
    let requests = search_requests(&fixture).await;
    assert_eq!(requests.len(), 4, "{requests:#?}");
    for (request, phase) in requests
        .iter()
        .zip(["precise", "broad", "precise", "broad"])
    {
        let body = request.body.as_ref().ok_or("search body is recorded")?;
        assert_eq!(body["identifiers"], json!([]));
        assert_eq!(body["phase"], phase);
        assert_eq!(body["query"], global_api::BROAD_BODY_MATCH_QUERY);
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn global_broad_search_keeps_short_terms_exact_and_phrases_unprefixed() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        view_lifetime: Duration::from_mins(5),
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled=true\nendpoint={:?}\nattempts=1\n{DEMO_PACKAGE}\n[languages.rust]\nstdlib=false\n",
        fixture.endpoint,
    );
    let (_directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"probe\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
            ),
            ("src/lib.rs", "pub fn local_probe() {}\n"),
        ],
        Some(configuration),
    )
    .await?;
    let answer = ranked_search_symbols(
        &client,
        json!({"query":"fn helper_beacon \"pub fn\"", "scope":"global"}),
    )
    .await?;
    let requests = search_requests(&fixture).await;
    let rows = workspace_client::query::rows(&answer)?;
    assert!(
        rows.iter().any(|row| {
            workspace_client::query::literal(row, "name").is_ok_and(|name| name == "helper_beacon")
        }),
        "{answer:#}; requests={requests:#?}"
    );
    assert_eq!(
        view_expiry(&answer["view"])?,
        view_expiry(fixture.captured_view())?
    );
    assert_eq!(requests.len(), 2, "{requests:#?}");
    for (request, broad) in requests.iter().zip([false, true]) {
        let body = request.body.as_ref().ok_or("search body is recorded")?;
        assert_eq!(body["phase"], if broad { "broad" } else { "precise" });
        assert_eq!(body["identifiers"], json!(["helper_beacon"]));
        if broad {
            assert_eq!(body["view"], fixture.captured_view()["id"]);
        } else {
            assert!(body.get("view").is_none());
        }
        assert_eq!(
            body["terms"],
            json!([
                {"text":"fn", "phrase":false, "prefix":false},
                {"text":"helper", "phrase":false, "prefix":broad},
                {"text":"beacon", "phrase":false, "prefix":broad},
                {"text":"pub fn", "phrase":true, "prefix":false}
            ])
        );
    }
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
            query_narrowed: false,
            ..FixtureOptions::default()
        })
        .await?;
        let configuration = format!(
            "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
             request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
            fixture.endpoint
        );
        let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
        let workspace = served_dependent_workspace(Some(&configuration)).await?;
        let (directory, client, server_task) = workspace.served;
        let answer = search_symbols(
            &client,
            json!({"query":"helper_beacon demo","scope":"global"}),
        )
        .await?;
        assert!(
            workspace_client::query::rows(&answer)?.iter().any(|hit| {
                workspace_client::query::iri(hit, "unit").is_ok_and(|unit| unit == COLLECTED_UNIT)
            }),
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

/// A search page stopped at its response body bound cannot answer a complete query. Its
/// `result_truncated` warning remains, and its cursor is not followed.
#[tokio::test]
async fn a_page_stopped_at_the_body_bound_answers_without_following_its_cursor() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        stopped_at_body_bound: true,
        query_narrowed: false,
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
    let answer = search_symbols(
        &client,
        json!({"query":"helper_beacon demo","scope":"global","include":["source"]}),
    )
    .await?;
    assert_unavailable(&answer);
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
fn assert_helper_beacon_pattern_hits(answer: &Value) -> TestResult {
    let rows = workspace_client::query::rows(answer)?;
    assert_eq!(rows.len(), 2, "{answer:#}");
    let identity = global_api::collected_identity(SymbolFixture::Valid, "helper_beacon");
    assert_eq!(
        workspace_client::query::iri(&rows[0], "symbol")?,
        identity.as_str()
    );
    assert!(
        !rows[1]
            .as_object()
            .ok_or("row is an object")?
            .contains_key("symbol")
    );
    for (row, start, end) in [(&rows[0], "0", "25"), (&rows[1], "4", "20")] {
        assert_eq!(workspace_client::query::iri(row, "unit")?, COLLECTED_UNIT);
        assert_eq!(
            workspace_client::query::literal(row, "source")?,
            "pub fn helper_beacon() {}"
        );
        assert_eq!(workspace_client::query::literal(row, "start")?, start);
        assert_eq!(workspace_client::query::literal(row, "end")?, end);
        assert_eq!(workspace_client::query::literal(row, "line")?, "1");
    }
    Ok(())
}

/// A `pattern` whose `scope` reaches packages answers the package matches from one page of
/// the global API's pattern search: `global` alone, `all` after the project's own. A package
/// match answers a file hit addressed by `unit` and a symbol hit for the declaration holding
/// it, each with its source when the request includes it.
#[tokio::test]
async fn a_package_scoped_pattern_answers_package_matches_from_one_request() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;

    let global = pattern_with_view(
        &client,
        json!({
            "pattern": r"fn helper_\w+",
            "scope": "global",
            "target": "all",
            "include": ["source"]
        }),
    )
    .await?;
    assert_helper_beacon_pattern_hits(&global)?;
    assert_pattern_view_readback(&client, &fixture, &global).await?;

    let all = search_symbols(
        &client,
        json!({"pattern": r"fn \w*beacon", "scope": "all", "target": "file"}),
    )
    .await?;
    let addressed: Vec<(Option<&str>, Option<&str>)> = workspace_client::query::rows(&all)?
        .iter()
        .map(|row| (row["path"]["value"].as_str(), row["unit"]["value"].as_str()))
        .collect();
    assert_eq!(
        addressed,
        [
            (Some("src/lib.rs"), None),
            (Some("src/lib.rs"), None),
            (None, Some(COLLECTED_UNIT)),
            (None, Some(COLLECTED_UNIT)),
        ],
        "the project's matches first, then the package's: {all:#}"
    );

    let local = search_symbols(&client, json!({"pattern": r"fn \w*beacon"})).await?;
    assert!(
        workspace_client::query::rows(&local)?
            .iter()
            .all(|row| row["path"]["value"] == "src/lib.rs"),
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
                "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}],
                "include": ["source"]
            }),
            &json!({
                "pattern": r"fn \w*beacon",
                "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}]
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

fn mixed_local_source_error(error: &dyn std::error::Error, mixed: &Value) -> String {
    let rows = mixed["answer"]["bindings"].as_array();
    let locators: Vec<_> = rows
        .into_iter()
        .flatten()
        .take(4)
        .map(|row| json!({"path": row.get("path"), "unit": row.get("unit")}))
        .collect();
    format!(
        "local Source read: {error}; mixed_view={}; row_count={:?}; locators={locators:?}",
        mixed["view"],
        rows.map(Vec::len)
    )
}

fn mixed_source_query(args: Value) -> TestResult<rift_protocol::query::QueryParams> {
    let parameters: rift_protocol::read::SearchParams = serde_json::from_value(args)?;
    let limit = parameters
        .limit
        .unwrap_or(u64::try_from(rift_core::constants::SEARCH_RESULTS_DEFAULT)?);
    let limit = oxigraph::model::Literal::from(limit);
    let mut query = workspace_client::query::projected_retrieval(
        &parameters,
        "",
        &format!("?hit :limit {limit} ."),
    )?;
    query.query = format!(
        "SELECT * {{ {{ {} }} OPTIONAL {{ ?local_unit a :SourceUnitId; :path ?catalog_path . FILTER(BOUND(?path) && ?catalog_path = ?path) }} }} ORDER BY ?index",
        query.query
    );
    Ok(query)
}

fn mixed_local_unit(mixed: &Value) -> TestResult<&str> {
    let mut selected = None;
    for row in workspace_client::query::rows(mixed)? {
        if row["path"]["value"] == "src/lib.rs" {
            let unit = workspace_client::query::iri(row, "local_unit")?;
            if let Some(previous) = selected {
                assert_eq!(unit, previous, "one original local source unit: {mixed:#}");
            }
            selected = Some(unit);
        } else {
            assert!(
                row.get("local_unit").is_none(),
                "remote hits do not bind the local source catalog: {mixed:#}"
            );
        }
    }
    selected.ok_or_else(|| "the original mixed query must expose its local source unit".into())
}

async fn assert_mixed_source_readback(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &GlobalFixture,
    mixed: &Value,
) -> TestResult {
    let before = fixture.requests().await.len();
    let local_unit = mixed_local_unit(mixed)?;
    let local = call_tool(
        client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({
                "unit":local_unit, "view":mixed["view"]["id"]
            }),
        )?)?)?,
    )
    .await
    .map_err(|error| mixed_local_source_error(error.as_ref(), mixed))?;
    let [local_row] = workspace_client::query::rows(&local)? else {
        return Err(format!("source query must return one original result: {local:#}").into());
    };
    assert_eq!(
        workspace_client::query::literal(local_row, "outcome")?,
        "found"
    );
    assert_eq!(workspace_client::query::iri(local_row, "unit")?, local_unit);
    assert_eq!(
        workspace_client::query::literal(local_row, "complete")?,
        "true"
    );
    assert_eq!(
        workspace_client::query::literal(local_row, "source_view")?,
        mixed["view"]["id"].as_str().ok_or("original mixed view")?
    );
    assert_eq!(
        workspace_client::query::literal(local_row, "source_expiry")?,
        mixed["view"]["expires_at"]
            .as_str()
            .ok_or("original mixed expiry")?
    );
    assert!(
        workspace_client::query::literal(local_row, "text")?.contains("pub fn local_beacon() {}")
    );
    assert_eq!(
        fixture.requests().await.len(),
        before,
        "local source stays with its original workspace"
    );
    let remote = call_tool(
        client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({
                "unit":COLLECTED_UNIT, "view":mixed["view"]["id"]
            }),
        )?)?)?,
    )
    .await?;
    let [remote_row] = workspace_client::query::rows(&remote)? else {
        return Err(format!("source query must return one original result: {remote:#}").into());
    };
    assert_eq!(
        workspace_client::query::literal(remote_row, "outcome")?,
        "found"
    );
    assert_eq!(
        workspace_client::query::iri(remote_row, "unit")?,
        COLLECTED_UNIT
    );
    assert_eq!(
        workspace_client::query::literal(remote_row, "complete")?,
        "true"
    );
    assert_eq!(
        workspace_client::query::literal(remote_row, "source_view")?,
        mixed["view"]["id"].as_str().ok_or("original mixed view")?
    );
    assert_eq!(
        workspace_client::query::literal(remote_row, "source_expiry")?,
        mixed["view"]["expires_at"]
            .as_str()
            .ok_or("original mixed expiry")?
    );
    assert!(
        workspace_client::query::literal(remote_row, "text")?.contains("pub fn helper_beacon() {}")
    );
    Ok(())
}

#[tokio::test]
async fn global_pattern_empty_and_mixed_results_keep_original_views_and_source_owners() -> TestResult
{
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        view_lifetime: Duration::from_mins(5),
        ..FixtureOptions::default()
    })
    .await?;
    let original_view = fixture.captured_view().clone();
    let configuration = format!(
        "[global]\nenabled=true\nendpoint={:?}\nattempts=1\n{DEMO_PACKAGE}\n[languages.rust]\nstdlib=false\n",
        fixture.endpoint,
    );
    let (directory, client, server_task) = served_workspace(
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"probe\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn local_beacon() {}\npub fn beacon() {}\n",
            ),
        ],
        Some(configuration),
    )
    .await?;
    for selector in ["pattern", "query"] {
        let mut fields = json!({
            "scope":"global",
            "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]
        });
        fields[selector] = json!("absent_producer_match_here");
        if selector == "pattern" {
            fields["target"] = json!("file");
        }
        let parameters = serde_json::from_value(fields)?;
        let inner = workspace_client::query::projected_retrieval(&parameters, "", "")?;
        let request = rift_protocol::query::QueryParams {
            query: format!("SELECT (COUNT(?hit) AS ?count) {{ {{ {} }} }}", inner.query),
            language: rift_protocol::query::QueryLanguage::Sparql,
        };
        let empty = workspace_client::search_after_population(&client, &request).await?;
        let requests = fixture.requests().await;
        let rows = workspace_client::query::rows(&empty)
            .map_err(|error| format!("{selector}: {error}; requests={requests:#?}"))?;
        assert_eq!(
            workspace_client::query::literal(&rows[0], "count")?,
            "0",
            "{selector}: {empty:#}"
        );
        assert_eq!(
            view_expiry(&empty["view"])?,
            view_expiry(&original_view)?,
            "{selector}"
        );
    }
    let mixed = call_tool(
        &client,
        "search",
        serde_json::to_value(mixed_source_query(json!({
            "pattern":r"fn \w*beacon", "scope":"all", "target":"file",
            "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]
        }))?)?,
    )
    .await?;
    assert_eq!(workspace_client::query::rows(&mixed)?.len(), 4, "{mixed:#}");
    assert_eq!(view_expiry(&mixed["view"])?, view_expiry(&original_view)?);
    let ranked = workspace_client::search_after_population(
        &client,
        &mixed_source_query(json!({
            "query":"beacon", "scope":"all", "include":["source"],
            "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]
        }))?,
    )
    .await?;
    let requests = fixture.requests().await;
    let rows = workspace_client::query::rows(&ranked)
        .map_err(|error| format!("{error}; requests={requests:#?}"))?;
    assert!(
        rows.iter().any(|row| row["path"]["value"] == "src/lib.rs"),
        "{ranked:#}"
    );
    assert!(
        rows.iter()
            .any(|row| row["unit"]["value"] == COLLECTED_UNIT),
        "{ranked:#}"
    );
    assert_eq!(view_expiry(&ranked["view"])?, view_expiry(&original_view)?);
    fs::write(directory.path().join("src/lib.rs"), "pub fn changed() {}\n")?;
    for mixed in [&mixed, &ranked] {
        assert_mixed_source_readback(&client, &fixture, mixed).await?;
    }
    assert_eq!(pattern_requests(&fixture).await.len(), 2);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn empty_global_package_pages_validate_original_metadata_in_both_phases() -> TestResult {
    use rift_cloud_client::{
        Config, GlobalClient, PackageIdentity, PackageSearchRequest, PackageSearchRequestPhase,
        PackageSearchRequestTarget, QueryTerm,
    };
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        view_lifetime: Duration::from_mins(5),
        ..FixtureOptions::default()
    })
    .await?;
    let client = GlobalClient::new(Config {
        endpoint: fixture.endpoint.clone(),
        token_env: "RIFT_GLOBAL_TEST_UNUSED_TOKEN".to_owned(),
        ..Config::default()
    })?;
    let parsed = rift_ranking::ParsedQuery::parse("absent_producer_match_here")?;
    for phase in [
        PackageSearchRequestPhase::Precise,
        PackageSearchRequestPhase::Broad,
    ] {
        let broad = matches!(phase, PackageSearchRequestPhase::Broad);
        let request = PackageSearchRequest {
            query: Some(parsed.source().to_owned()),
            terms: Some(
                parsed
                    .members()
                    .iter()
                    .map(|member| QueryTerm {
                        text: member.text().to_owned(),
                        phrase: member.is_phrase(),
                        prefix: broad && !member.is_phrase(),
                    })
                    .collect(),
            ),
            identifiers: Some(
                parsed
                    .candidates()
                    .into_iter()
                    .map(|candidate| candidate.text().to_owned())
                    .collect(),
            ),
            include: None,
            packages: Some(vec![PackageIdentity {
                manager: "cargo".to_owned(),
                registry: "crates.io".to_owned(),
                name: "demo".to_owned(),
                version: "1.0.0".to_owned(),
            }]),
            phase: Some(phase),
            target: Some(PackageSearchRequestTarget::All),
            traversal: None,
            view: None,
        };
        let page = client
            .search_packages(&request, 1, None)
            .await
            .map_err(|error| format!("empty package page, broad {broad}: {error:?}"))?;
        assert!(page.items.is_empty());
        let original = serde_json::to_value(fixture.captured_view())?;
        let selected = serde_json::to_value(page.view)?;
        assert_eq!(selected["id"], original["id"]);
        assert_eq!(view_expiry(&selected)?, view_expiry(&original)?);
    }
    Ok(())
}

#[tokio::test]
async fn global_pattern_without_original_view_refuses_even_when_no_match_exists() -> TestResult {
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        pattern_view: global_api::PatternView::Missing,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled=true\nendpoint={:?}\nattempts=1\n{DEMO_PACKAGE}\n[languages.rust]\nstdlib=false\n",
        fixture.endpoint,
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (_directory, client, server_task) = workspace.served;
    let request = workspace_client::query::retrieval(&serde_json::from_value(json!({
        "pattern":"no_pattern_match_here", "scope":"global", "target":"file",
        "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]
    }))?)?;
    let failure = failed_call(
        client
            .call_tool(tool_request("search", &serde_json::to_value(request)?))
            .await,
    )?;
    assert_eq!(
        failure.code,
        ErrorCode::CapabilityUnavailable,
        "{}",
        failure.text
    );
    assert!(failure.text.contains("rift.server.read_unsupported"));
    assert_eq!(pattern_requests(&fixture).await.len(), 1);
    assert!(
        fixture
            .requests()
            .await
            .iter()
            .all(|request| !request.uri.contains("/symbols") && !request.uri.contains("/source"))
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A pattern the server refuses spends no global request: the project side reads, and
/// refuses, before the package side is asked.
#[tokio::test]
async fn a_refused_package_scoped_pattern_makes_no_global_request() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
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
                &serde_json::to_value(workspace_client::query::retrieval(
                    &serde_json::from_value(json!({"pattern":"beacon(", "scope":"global"}))?,
                )?)?,
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
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
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
    let _ = search_symbols(&client, json!({"target":"symbol", "query":"local_beacon"})).await?;
    let _ = search_symbols(
        &client,
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
/// the project and in the collected package, and symbol and full-text `search` answers
/// carry the project hits alone, with the typed warning naming the failure and no package
/// counts.
#[tokio::test]
async fn invalid_remote_page_discards_the_lane_and_answers_project_hits() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::InvalidIdentity).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = search_symbols(
        &client,
        json!({"target":"symbol", "query":"beacon","scope":"all"}),
    )
    .await?;
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
    assert_unavailable(&answer);
    drop(directory);
    client.cancel().await?;
    server_task.await?;

    // Inside `failure_ttl` the client hands back the failure it recorded and asks the
    // global API nothing, so the search runs against a server of its own to meet the
    // invalid page.
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let answer = search_symbols(&client, json!({"query":"beacon","scope":"all"})).await?;
    let warning = answer["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|warning| warning["code"] == "global_response_invalid")
        .ok_or_else(|| format!("missing invalid-response warning: {answer:#}"))?;
    assert_eq!(warning["failure_class"], "invalid_response");
    assert_unavailable(&answer);
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
    let fixture = complete_fixture(SymbolFixture::InvalidIdentity).await?;
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

    let first = search_symbols(
        &client,
        json!({"target":"symbol", "query":"beacon","scope":"all"}),
    )
    .await?;
    assert_eq!(
        global_warnings(&first),
        std::slice::from_ref(&expected),
        "{first:#}"
    );
    let sent = fixture.requests().await.len();

    let later = search_symbols(
        &client,
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
    let answer = search_symbols(
        &client,
        json!({"target":"symbol", "query": "local_beacon", "scope": "all"}),
    )
    .await?;
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
    assert_unavailable(&answer);
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
    search_symbols(&client, json!({"query": "local_beacon"})).await?;

    let failure = failed_call(
        client
            .call_tool(tool_request(
                "search",
                &serde_json::to_value(workspace_client::query::retrieval(
                    &serde_json::from_value(
                        json!({"query":"local_beacon", "scope":"all", "rev":"main"}),
                    )?,
                )?)?,
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
                    [[package]]\nname = \"private\"\nversion = \"0.6.1\"\n\
                    source = \"registry+https://registry.foreign.test/index\"\n\n\
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

    let answer = search_symbols(
        &client,
        json!({"target":"symbol", "query":"local_beacon","scope":"all"}),
    )
    .await?;

    assert_unavailable(&answer);
    assert_unserved_dependency_warnings(&answer)?;
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn assert_unserved_dependency_warnings(answer: &Value) -> TestResult {
    let warnings = answer["warnings"]
        .as_array()
        .ok_or("warnings are an array")?;
    let disabled: Vec<&Value> = warnings
        .iter()
        .filter(|warning| warning["code"] == "global_access_disabled")
        .collect();
    assert_eq!(disabled, [&json!({"code": "global_access_disabled"})]);
    let unavailable: Vec<(String, String, String)> = warnings
        .iter()
        .filter(|warning| {
            warning["code"] != "global_access_disabled"
                && warning["code"] != "lexical_ranking_unavailable"
        })
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
            "registry_unresolved",
            "Package registry endpoint is unresolved.".to_owned(),
        ),
        (
            "private",
            "private_registry",
            "Currently rift doesn't support indexing dependencies from private registries."
                .to_owned(),
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
    assert_distinct_registry_observations(warnings, answer);
    assert!(
        !answer.to_string().contains("\"inner\""),
        "a path dependency inside the workspace is project source: {answer:#}"
    );
    Ok(())
}

fn assert_distinct_registry_observations(warnings: &[Value], answer: &Value) {
    let private: Vec<&Value> = warnings
        .iter()
        .filter(|warning| warning["entry"]["name"] == "private")
        .map(|warning| &warning["entry"])
        .collect();
    assert_eq!(
        private,
        [
            &json!({
                "manager": "cargo",
                "name": "private",
                "requirement": "0.6",
                "availability": "registry_unresolved"
            }),
            &json!({
                "manager": "cargo",
                "registry": "registry.example.test/index",
                "name": "private",
                "version": "0.6.1",
                "availability": "private_registry"
            }),
            &json!({
                "manager": "cargo",
                "registry": "registry.foreign.test/index",
                "name": "private",
                "version": "0.6.1",
                "availability": "private_registry"
            })
        ],
        "an unresolved registry alias cannot identify the locked registry owner: {answer:#}"
    );
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
/// read alone: the context pins `demo` at a release the collection lacks, which reports
/// `package_substituted`, and the lookup naming the collected release sends that version
/// in its place. Missing context still prevents a complete answer.
#[tokio::test]
async fn the_package_argument_replaces_a_context_version_for_one_read() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n\
         [[dependencies.packages]]\nmanager = \"cargo\"\nregistry = \"crates.io\"\nname = \"demo\"\nversion = \"0.9.0\"\n",
        fixture.endpoint
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let pinned = search_symbols(
        &client,
        json!({"target":"symbol", "query":"helper_beacon","scope":"global"}),
    )
    .await?;
    assert_unavailable(&pinned);
    assert_eq!(
        substituted_versions(&pinned),
        [&json!("0.9.0")],
        "{pinned:#}"
    );

    let replaced = search_symbols(
        &client,
        json!({
            "target": "symbol",
            "query": "helper_beacon",
            "scope": "global",
            "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}]
        }),
    )
    .await?;
    assert_unavailable(&replaced);
    assert!(
        substituted_versions(&replaced).is_empty(),
        "the requested release answers exactly: {replaced:#}"
    );

    let resolutions = resolution_entries(&fixture).await;
    assert_eq!(resolutions.len(), 2, "{resolutions:#?}");
    assert_eq!(
        demo_entries(&resolutions[0]),
        [
            &json!({"manager":"cargo", "registry": "crates.io","name":"demo","version":"0.9.0","availability":"canonical"})
        ]
    );
    assert_eq!(
        demo_entries(&resolutions[1]),
        [
            &json!({"manager":"cargo", "registry": "crates.io","name":"demo","version":"1.0.0","availability":"canonical"})
        ],
        "the read sends the requested version in place of the pinned one"
    );
    assert!(
        resolutions[1]
            .iter()
            .any(|entry| entry["manager"] == "stdlib" && entry["name"] == "rust"),
        "the entries the request names nothing about stand: {resolutions:#?}"
    );

    let again = search_symbols(
        &client,
        json!({"target":"symbol", "query":"helper_beacon","scope":"global"}),
    )
    .await?;
    assert_unavailable(&again);
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
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let answer = ranked_search_symbols(
        &client,
        json!({
            "query": "helper_beacon",
            "scope": "all",
            "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo"}]
        }),
    )
    .await?;
    assert!(has_unit(&answer, COLLECTED_UNIT)?, "{answer:#}");

    let resolutions = resolution_entries(&fixture).await;
    assert_eq!(resolutions.len(), 1, "{resolutions:#?}");
    assert_eq!(
        demo_entries(&resolutions[0]),
        [
            &json!({"manager":"cargo", "registry": "crates.io","name":"demo","requirement":">=0","availability":"canonical"})
        ]
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
            json!([{"manager":"cargo", "registry": "crates.io","name":"demo","version":"1.0.0"}]),
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
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (directory, client, server_task) = workspace.served;
    let id = workspace_client::captured_seed(&client, "beacon", "src/lib.rs").await?;
    let refused = [
        (
            "search",
            json!({"query": "SELECT ?hit { SERVICE :search { ?hit :query 'beacon'; :pattern 'beacon'; :scope 'all' } }"}),
            "pattern",
        ),
        (
            "search",
            json!({"query": "SELECT ?hit { SERVICE :search { ?hit :query 'beacon'; :scope 'all' } } LIMIT -1"}),
            "query",
        ),
        (
            "search",
            json!({"query": "SELECT ?symbol { <rift://symbol/local/rust/walk/caller :calls ?symbol }"}),
            "query",
        ),
        (
            "search",
            json!({"query": "SELECT ?hit { SERVICE :search { ?hit :pattern 42; :scope 'all' } }"}),
            "query",
        ),
        (
            "get_symbol",
            json!({"id": id, "declaration_limit": 0}),
            "declaration_limit",
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
/// cannot answer a complete query: the resolution request holds that many entries, the requested
/// package among them, and the context entries sorted last leave with a
/// `package_context_degraded` warning naming their package manager.
#[tokio::test]
async fn an_advertised_entry_bound_below_the_client_bound_cuts_the_context() -> TestResult {
    use std::fmt::Write as _;

    let advertised = 3;
    let fixture = GlobalFixture::start_with(FixtureOptions {
        dependency_entries_max: advertised,
        query_narrowed: false,
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
        "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}]
    });
    let answer = search_symbols(&client, request).await?;
    assert_unavailable(&answer);
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

/// A global API advertising fewer entries than a read requests cannot answer it: every
/// lockfile entry leaves first, then the requested entries sorted last, and
/// `package_context_degraded` names the bound and the requested packages not reported. The
/// read reports insufficient coverage, and no global warning says the global
/// API failed.
#[tokio::test]
async fn an_advertised_entry_bound_below_the_requested_count_cuts_requested_entries() -> TestResult
{
    let fixture = GlobalFixture::start_with(FixtureOptions {
        dependency_entries_max: 1,
        query_narrowed: false,
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
            {"manager": "cargo", "registry": "crates.io", "name": "zlib", "version": "1.3.1"},
            {"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}
        ]
    });
    let answer = search_symbols(&client, request).await?;
    assert_unavailable(&answer);
    let sent = resolution_entries(&fixture).await;
    assert_eq!(sent.len(), 1, "one resolution request: {sent:#?}");
    assert_eq!(
        sent[0],
        [json!({
            "availability": "canonical",
            "manager": "cargo", "registry": "crates.io",
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

/// A workspace whose lockfile fills the dependency context cannot answer a complete
/// query naming one more package: the requested package goes out, the context entry sorted
/// last leaves with a `package_context_degraded` warning naming its package manager, and
/// the resolution request stays within the entry bound the client holds it to.
#[tokio::test]
async fn a_requested_package_past_the_entry_bound_displaces_a_context_entry() -> TestResult {
    use std::fmt::Write as _;

    let fixture = complete_fixture(SymbolFixture::Valid).await?;
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
        "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}]
    });
    let answer = search_symbols(&client, request).await?;
    assert_unavailable(&answer);
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
            "patterns",
        ),
        (
            "search",
            json!({"query": "beacon", "scope": "all"}),
            "documentation_search",
        ),
    ];
    for (tool, request, feature) in reads {
        let answer = read_producer(&client, tool, request.clone()).await?;
        assert_unavailable(&answer);
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
    let id = global_api::collected_identity(SymbolFixture::Valid, "beacon");
    let refused = failed_call(
        client
            .call_tool(tool_request(
                "get_symbol",
                &json!({"id": id, "include": ["documentation"]}),
            ))
            .await,
    )?;
    assert!(!refused.message.is_empty(), "{}", refused.text);
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
    let fixture = GlobalFixture::start_with(FixtureOptions {
        query_narrowed: false,
        ..FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let parameters = serde_json::from_value(json!({"query": BODY_MATCH_QUERY, "scope": "global"}))?;
    let query = workspace_client::query::projected_retrieval(
        &parameters,
        "?matched",
        "?hit :matched_by/:value ?matched .",
    )?;
    let answer = call_tool(&client, "search", serde_json::to_value(query)?).await?;
    let package_hits: Vec<&Value> = workspace_client::query::rows(&answer)?
        .iter()
        .filter(|hit| {
            workspace_client::query::iri(hit, "unit").is_ok_and(|unit| unit == COLLECTED_UNIT)
        })
        .collect();
    assert_eq!(package_hits.len(), 1, "{answer:#}");
    assert_eq!(
        workspace_client::query::literal(package_hits[0], "matched")?,
        "content",
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
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"1s\"\nconnect_timeout = \"100ms\"\n",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;

    let answer = pattern_with_view(
        &client,
        json!({
            "pattern": r"fn helper_\w+",
            "scope": "global",
            "target": "all",
            "include": ["source"],
            "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}]
        }),
    )
    .await?;
    assert_helper_beacon_pattern_hits(&answer)?;
    assert_pattern_view_readback(&client, &fixture, &answer).await?;

    let bodies: Vec<Value> = pattern_requests(&fixture)
        .await
        .into_iter()
        .filter_map(|request| request.body)
        .collect();
    assert_eq!(
        bodies,
        [json!({
            "pattern": r"fn helper_\w+",
            "packages": [{"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"}],
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
    let request = workspace_client::query::retrieval(&serde_json::from_value(json!({
        "query":"searchHit", "limit":1, "include":["source"]
    }))?)?;
    let answer = workspace_client::search_after_population(&client, &request).await?;
    let row = workspace_client::query::rows(&answer)?
        .first()
        .ok_or("exact original spelling")?;
    assert_eq!(
        workspace_client::query::literal(row, "name")?,
        "searchHit",
        "{answer:#}"
    );
    assert_eq!(
        workspace_client::query::literal(row, "source")?,
        "pub fn searchHit() {}",
        "{answer:#}"
    );
    let parameters = serde_json::from_value(json!({"query": "searchHit", "limit": 10}))?;
    let query = workspace_client::query::projected_retrieval(
        &parameters,
        "?matched",
        "OPTIONAL { ?hit :matched_by/:value ?matched }",
    )?;
    let answer = call_tool(&client, "search", serde_json::to_value(query)?).await?;
    assert!(
        workspace_client::query::rows(&answer)?.iter().any(|hit| {
            workspace_client::query::literal(hit, "name").is_ok_and(|name| name == "SearchHit")
                && workspace_client::query::literal(hit, "matched")
                    .is_ok_and(|matched| matched == "documentation")
        }),
        "{answer:#}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn global_service_query_view_retains_exact_provider_reads_and_refuses_changed_configuration()
-> TestResult {
    let complete = FixtureOptions {
        query_narrowed: false,
        ..FixtureOptions::default()
    };
    let first = GlobalFixture::start_with(complete).await?;
    let second = GlobalFixture::start_with(complete).await?;
    let configuration = |endpoint: &str| {
        format!(
            "[global]\nenabled=true\nendpoint={endpoint:?}\nattempts=1\nrequest_timeout='1s'\nconnect_timeout='100ms'\n{DEMO_PACKAGE}\n[languages.rust]\nstdlib=false\n"
        )
    };
    let workspace = served_dependent_workspace(Some(&configuration(&first.endpoint))).await?;
    let (directory, client, server_task) = workspace.served;
    let parameters =
        serde_json::from_value(json!({"query":"beacon","scope":"global","include":["source"]}))?;
    let request = workspace_client::query::retrieval(&parameters)?;
    let answer = workspace_client::search_after_population(&client, &request).await?;
    let row = workspace_client::query::rows(&answer)?
        .first()
        .ok_or("global projected hit")?;
    let identity = workspace_client::query::iri(row, "symbol")?.to_owned();
    let view = answer["view"]["id"]
        .as_str()
        .ok_or("query serving view")?
        .to_owned();
    assert_ne!(view, "a".repeat(64));
    let exact = call_tool(
        &client,
        "get_symbol",
        json!({"id":identity,"view":view,"include":["source"]}),
    )
    .await?;
    assert_eq!(exact["outcome"], "found", "{exact}");
    assert_eq!(exact["view"], answer["view"]);
    assert_global_query_source(&client, &answer, &view).await?;
    for request in first
        .requests()
        .await
        .iter()
        .filter(|request| request.uri.contains("/symbols") || request.uri.contains("/source"))
    {
        assert_eq!(
            request.body.as_ref().ok_or("exact request body")?["view"],
            "a".repeat(64)
        );
    }
    fs::write(
        directory.path().join("rift.toml"),
        format!(
            "{}\n{}",
            hermetic_search::HERMETIC_TABLES,
            configuration(&second.endpoint)
        ),
    )?;
    call_retrying_acceptance(
        &client,
        tool_request(
            "search",
            &json!({
                "query":"ASK { ?symbol :name 'local_beacon' }"
            }),
        ),
    )
    .await?;
    let refused = call_tool(
        &client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({"unit":COLLECTED_UNIT,"view":view}),
        )?)?)?,
    )
    .await?;
    assert_eq!(refused["outcome"], "unavailable", "{refused}");
    assert_eq!(refused["reason"], "view_context");
    assert!(
        !second
            .requests()
            .await
            .iter()
            .any(|request| request.uri.contains("/source") || request.uri.contains("/symbols"))
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

async fn assert_global_query_source(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    answer: &Value,
    view: &str,
) -> TestResult {
    let source = call_tool(
        client,
        "search",
        serde_json::to_value(workspace_client::query::source(&serde_json::from_value(
            json!({"unit":COLLECTED_UNIT,"view":view}),
        )?)?)?,
    )
    .await?;
    let [source_row] = workspace_client::query::rows(&source)? else {
        return Err(format!("source query must return one original result: {source:#}").into());
    };
    assert_eq!(
        workspace_client::query::literal(source_row, "outcome")?,
        "found"
    );
    assert_eq!(
        workspace_client::query::iri(source_row, "unit")?,
        COLLECTED_UNIT
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "complete")?,
        "true"
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_view")?,
        view
    );
    assert_eq!(
        workspace_client::query::literal(source_row, "source_expiry")?,
        answer["view"]["expires_at"]
            .as_str()
            .ok_or("original global expiry")?
    );
    assert!(workspace_client::query::literal(source_row, "text")?.contains("pub fn beacon()"));
    Ok(())
}

/// Exact original spelling wins through MCP before a one-result page is selected (#597).
#[tokio::test]
async fn test_identifier_case_pairs_rank_exact_spelling_through_mcp() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::CasePairs).await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let source = global_api::CASE_SOURCE;
    let (_directory, client, server_task) =
        served_workspace(
            &[
                (
                    "Cargo.toml",
                    "[package]\nname='case_pairs'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='src/lib.rs'\n",
                ),
                ("src/lib.rs", source),
            ],
            Some(configuration),
        )
        .await?;
    for scope in ["local", "global", "all"] {
        for name in [
            "SearchHit",
            "searchHit",
            "createProgram",
            "createprogram",
            "load_config",
            "Load_config",
            "Foo",
            "foo",
        ] {
            assert_original_identifier(&client, scope, name).await?;
        }
        let args = json!({"query": "SEARCHHIT", "scope": scope, "target":"symbol", "limit": 10, "include": ["source"]});
        let answer = ranked_search_symbols(&client, args).await?;
        let names = literals(&answer, "name")?;
        assert!(
            names.contains(&"SearchHit") && names.contains(&"searchHit"),
            "{answer:#}"
        );
    }
    for (query, identifiers) in [
        ("SearchHit and searchHit", ["SearchHit", "searchHit"]),
        ("Foo foo", ["Foo", "foo"]),
        ("foo Foo", ["foo", "Foo"]),
    ] {
        assert_identifier_request(&client, &fixture, query, identifiers).await?;
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Both original spellings reach the package request in query order.
async fn assert_identifier_request(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &GlobalFixture,
    query: &str,
    expected: [&str; 2],
) -> TestResult {
    let args = json!({"query": query, "scope": "global", "limit": 10});
    let answer = ranked_search_symbols(client, args).await?;
    assert_eq!(
        workspace_client::query::rows(&answer)?.len(),
        2,
        "{answer:#}"
    );
    let requests = search_requests(fixture).await;
    let identifiers = requests
        .iter()
        .filter_map(|request| request.body.as_ref())
        .find(|body| body["query"] == query && body["phase"] == "precise")
        .ok_or("both original candidates reach the global request")?;
    assert_eq!(identifiers["identifiers"], json!(expected));
    Ok(())
}

/// Checks search and symbol lookup spelling, source, and identity on one page.
async fn assert_original_identifier(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    scope: &str,
    name: &str,
) -> TestResult {
    let args = json!({"query": name, "scope": scope, "limit": 1, "include": ["source"]});
    let answer = ranked_search_symbols(client, args).await?;
    let hits = workspace_client::query::rows(&answer)?;
    assert_eq!(hits.len(), 1, "{scope}: {answer:#}");
    let hit = &hits[0];
    assert_eq!(
        workspace_client::query::literal(hit, "name")?,
        name,
        "{scope}: {answer:#}"
    );
    let expected_source = format!("pub fn {name}() {{}}");
    assert_eq!(
        workspace_client::query::literal(hit, "source")?,
        expected_source,
        "{scope}: {answer:#}"
    );
    let id = workspace_client::query::iri(hit, "symbol")?;
    assert!(id.ends_with(&format!("/{name}")), "{id}");
    let args = json!({"id": id, "include": ["source"]});
    let answer = call_tool(client, "get_symbol", args).await?;
    assert_eq!(answer["outcome"], "found", "{scope}: {answer:#}");
    assert_eq!(answer["symbol"]["id"], id, "{scope}: {answer:#}");
    assert_eq!(answer["symbol"]["name"], name, "{scope}: {answer:#}");
    assert_eq!(
        answer["declarations"][0]["source"], expected_source,
        "{scope}: {answer:#}"
    );
    Ok(())
}

/// Native validation accepts original candidates and package results (#597).
#[tokio::test]
async fn test_native_client_accepts_original_identifier_candidates() -> TestResult {
    use rift_cloud_client::{Config, GlobalClient, QueryTerm};
    let fixture = complete_fixture(SymbolFixture::CasePairs).await?;
    let client = GlobalClient::new(Config {
        endpoint: fixture.endpoint.clone(),
        ..Config::default()
    })?;
    let mut request = case_search_request("SearchHit and searchHit")?;
    let page = client.search_packages(&request, 10, None).await?;
    assert_eq!(page.items.len(), 2);
    for name in ["SearchHit", "searchHit", "createProgram", "load_config"] {
        request.query = Some(name.to_owned());
        request.terms = Some(vec![QueryTerm {
            text: name.to_owned(),
            phrase: false,
            prefix: false,
        }]);
        request.identifiers = Some(vec![name.to_owned()]);
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
        query: Some(query.to_owned()),
        terms: Some(
            parsed
                .members()
                .iter()
                .map(|member| QueryTerm {
                    text: member.text().to_owned(),
                    phrase: member.is_phrase(),
                    prefix: false,
                })
                .collect(),
        ),
        identifiers: Some(
            parsed
                .candidates()
                .iter()
                .map(|candidate| candidate.text().to_owned())
                .collect(),
        ),
        include: Some(vec!["source".to_owned()]),
        packages: Some(vec![rift_cloud_client::PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
        }]),
        phase: Some(PackageSearchRequestPhase::Precise),
        target: None,
        traversal: None,
        view: None,
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
        let answer = ranked_search_symbols(&client, args).await?;
        assert_eq!(
            workspace_client::query::literal(&workspace_client::query::rows(&answer)?[0], "name")?,
            name,
            "{answer:#}"
        );
        assert_eq!(
            workspace_client::query::literal(
                &workspace_client::query::rows(&answer)?[0],
                "source"
            )?,
            format!("pub fn {name}() {{}}"),
            "{answer:#}"
        );
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn test_served_package_reads_apply_workspace_request_opt_out() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[telemetry]\ndisabled = true\n[global]\nendpoint = \"{}\"\n\n{DEMO_PACKAGE}",
        fixture.endpoint
    );
    let configuration = format!("{configuration}\n[languages.rust]\nstdlib=false\n");
    let workspace = served_dependent_workspace(Some(&configuration)).await?;
    let (_directory, client, server_task) = workspace.served;
    let answer = search_symbols(
        &client,
        json!({"query": "beacon", "target": "symbol", "scope": "global"}),
    )
    .await?;
    assert!(!workspace_client::query::rows(&answer)?.is_empty());
    let requests = fixture.requests().await;
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| request.telemetry_disabled));
    assert!(
        requests
            .iter()
            .all(|request| request.uri.starts_with("/rift/rest/v1/"))
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// The recorded package examples use complete answers from the served MCP router.
#[tokio::test]
async fn recorded_global_query_examples_keep_source_and_substitution_contracts() -> TestResult {
    let fixture = complete_fixture(SymbolFixture::Valid).await?;
    let configuration = format!(
        "[global]\nenabled=true\nendpoint={:?}\nattempts=1\n{DEMO_PACKAGE}\n[languages.rust]\nstdlib=false\n",
        fixture.endpoint,
    );
    let (directory, client, server_task) = served_probe_workspace(configuration).await?;
    let parameters = serde_json::from_value(json!({
        "query":"beacon", "scope":"global", "target":"symbol", "include":["source"],
        "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"}]
    }))?;
    let search = workspace_client::query::retrieval(&parameters)?;
    workspace_client::search_after_population(&client, &search).await?;
    let mut captured_view = Value::Null;
    for example in ["global-search", "global-source", "requested-served"] {
        let arguments = match example {
            "global-search" => serde_json::to_value(&search)?,
            "global-source" => serde_json::to_value(workspace_client::query::source(
                &serde_json::from_value(json!({"unit":COLLECTED_UNIT,"view":captured_view["id"]}))?,
            )?)?,
            _ => serde_json::to_value(workspace_client::query::retrieval(
                &serde_json::from_value(json!({
                    "query":"beacon", "scope":"global", "target":"symbol", "include":["source"],
                    "packages":[{"manager":"cargo","registry":"crates.io","name":"demo","version":"1.1.0"}]
                }))?,
            )?)?,
        };
        let result = client.call_tool(tool_request("search", &arguments)).await?;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let answer = result
            .structured_content
            .as_ref()
            .ok_or("recorded query returns structured content")?;
        match example {
            "global-search" => {
                let rows = workspace_client::query::rows(answer)?;
                let identity = global_api::collected_identity(SymbolFixture::Valid, "beacon");
                assert!(
                    rows.iter().any(|row| {
                        workspace_client::query::iri(row, "symbol")
                            .is_ok_and(|symbol| symbol == identity.as_str())
                            && workspace_client::query::iri(row, "unit")
                                .is_ok_and(|unit| unit == COLLECTED_UNIT)
                            && workspace_client::query::literal(row, "source")
                                .is_ok_and(|source| source == "pub fn beacon() {}")
                    }),
                    "{answer:#}"
                );
                captured_view = answer["view"].clone();
                assert!(captured_view["id"].as_str().is_some());
            }
            "global-source" => assert_recorded_global_source(answer, &captured_view)?,
            _ => {
                assert_unavailable(answer);
                assert_eq!(substituted_versions(answer), [&json!("1.1.0")]);
                assert!(answer["warnings"].as_array().ok_or("query warnings")?.iter().any(|warning| {
                    warning["code"] == "package_substituted"
                        && warning["package"] == json!({"manager":"cargo","registry":"crates.io","name":"demo","version":"1.0.0"})
                }), "{answer:#}");
            }
        }
        println!(
            "RIFT_READ_EXAMPLE {}",
            serde_json::to_string(&json!({
                "id":example, "tool":"search", "arguments":arguments, "response":result
            }))?
        );
    }
    drop(directory);
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn assert_recorded_global_source(answer: &Value, captured: &Value) -> TestResult {
    let [row] = workspace_client::query::rows(answer)? else {
        return Err("recorded source query returns one result".into());
    };
    let text = "pub fn helper_beacon() {}\npub fn beacon() {}\n";
    assert_eq!(workspace_client::query::literal(row, "outcome")?, "found");
    assert_eq!(workspace_client::query::iri(row, "unit")?, COLLECTED_UNIT);
    assert_eq!(workspace_client::query::literal(row, "text")?, text);
    assert_eq!(workspace_client::query::literal(row, "start")?, "0");
    assert_eq!(
        workspace_client::query::literal(row, "end")?,
        text.len().to_string()
    );
    assert_eq!(workspace_client::query::literal(row, "complete")?, "true");
    assert_eq!(
        workspace_client::query::literal(row, "source_view")?,
        captured["id"].as_str().ok_or("original query view")?
    );
    assert_eq!(
        workspace_client::query::literal(row, "source_expiry")?.parse::<jiff::Timestamp>()?,
        view_expiry(captured)?
    );
    assert!(view_expiry(&answer["view"])? <= view_expiry(captured)?);
    Ok(())
}
