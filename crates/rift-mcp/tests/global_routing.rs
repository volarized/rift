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

use global_api::{COLLECTED_UNIT, GlobalFixture, Hold, SymbolFixture};
use rmcp::model::ReadResourceRequestParams;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use workspace_client::{ServedWorkspace, TestResult, served_workspace, tool_request};

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

    // The client answers every request as unavailable for `failure_ttl` after a failed
    // one, so the search runs against a server of its own to meet the invalid page.
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

    let error = client
        .call_tool(tool_request(
            "search",
            &json!({"query": "local_beacon", "scope": "all", "rev": "main"}),
        ))
        .await
        .expect_err("rev pairs with the project scope alone");
    let rmcp::ServiceError::McpError(error) = error else {
        panic!("the refusal must arrive as an MCP error: {error}");
    };

    let wire = error.data.ok_or("a refusal carries its wire data")?;
    assert_eq!(wire["code"], json!("invalid_request"), "{wire:#}");
    assert!(
        error.message.contains("scope"),
        "the refusal names the field: {}",
        error.message
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

/// A `packages` entry naming a package the context holds replaces its version for that
/// read alone: the context pins `demo` at a release the collection lacks, and the lookup
/// naming the collected release sends that version in its place and answers from it.
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
    assert_eq!(pinned["hits"], json!([]), "{pinned:#}");
    assert!(
        pinned["warnings"].as_array().is_some_and(|warnings| {
            warnings.iter().any(|warning| {
                warning["code"] == "package_absent" && warning["package"]["version"] == "0.9.0"
            })
        }),
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
        !replaced.to_string().contains("package_absent"),
        "the replaced release is not asked for: {replaced:#}"
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
        again["hits"],
        json!([]),
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
