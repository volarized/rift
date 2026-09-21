//! Served read requests and responses validate against their advertised schemas.

mod hermetic_search;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;

use jsonschema::Validator;
use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Most pages one corpus request may walk before the gate fails.
const FOLLOWED_PAGES_MAX: usize = 16;

/// The engine that resolves this fixture's references, embedded in the binary.
const ENGINE: &str = "\
[languages.python.lsp]\nembedded = \"ty\"\n\
retry = { attempts = 2, delay = \"1ms\", delay_limit = \"1ms\" }\n";

/// Sample validation corpus with various scenarios: one request per
/// advertised tool behavior worth proving.
fn corpus() -> Vec<(&'static str, Value)> {
    let mut requests = vec![
        ("get_symbol", json!({ "name": "beacon_one" })),
        ("get_symbol", json!({ "name": "beacon", "limit": 1 })),
        ("get_symbol", json!({ "name": "beacon", "include": [] })),
        // The fixture's path dependency `helper` answers these: a hit served from
        // the dependency index carries `unit` in place of `path`.
        (
            "get_symbol",
            json!({ "name": "helper_beacon", "scope": "global" }),
        ),
        ("get_symbol", json!({ "name": "beacon", "scope": "all" })),
        // The fixture's committed baseline serves the timeline: one
        // `introduced` version from the walk's first commit.
        (
            "get_symbol",
            json!({ "name": "beacon_one", "include": ["history"] }),
        ),
        ("search", json!({ "query": "beacon" })),
        (
            "search",
            json!({ "query": "beacon", "include": ["source"] }),
        ),
        ("search", json!({ "query": "beacon", "include": ["score"] })),
        (
            "search",
            json!({
                "query": "beacon",
                "limit": 1,
                "paths": { "include": ["lib.rs"] }
            }),
        ),
        (
            "search",
            json!({ "query": "beacon", "limit": 1, "page_index": 100 }),
        ),
        (
            "get_symbol",
            json!({ "name": "beacon", "limit": 1, "page_index": 50 }),
        ),
        (
            "search",
            json!({ "query": "beacon", "paths": { "include": ["lib.rs"] } }),
        ),
        (
            "search",
            json!({
                "query": "phantom",
                "target": "symbol",
                "paths": { "force_include": ["hidden.rs"] }
            }),
        ),
        ("nodes", json!({ "path": "lib.rs", "position": 0 })),
        ("nodes", json!({ "path": "lib.rs", "position": 8 })),
        (
            "nodes",
            json!({ "path": "packages/@scope/name/package.json", "position": 4 }),
        ),
        (
            "nodes",
            json!({ "path": "notes for café.json", "position": 4 }),
        ),
    ];
    requests.extend(dependency_scope_search_corpus());
    requests.extend(revision_read_corpus());
    requests.extend(change_search_corpus());
    requests.extend(lexical_search_corpus());
    requests.extend(traversal_search_corpus());
    requests
}

/// `scope` on `search` reaches the same helper `get_symbol`'s scoped requests reach: a
/// dependency hit carries `unit` in place of `path`, `all` merges it with the project
/// hits, and a `file` target answers empty from the package side since a package
/// contributes declarations alone.
fn dependency_scope_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({ "query": "helper_beacon", "scope": "global" }),
        ),
        (
            "search",
            json!({ "query": "beacon", "scope": "all", "include": ["source"] }),
        ),
        (
            "search",
            json!({
                "query": "beacon",
                "scope": "global",
                "target": "file"
            }),
        ),
    ]
}

/// `traversal` requests over the one real call edge the fixture already carries -
/// `traversal_caller.py`'s `calls_callee` calling `traversal_callee.py`'s `callee` - so
/// this corpus proves the params without perturbing any other corpus entry's fixture
/// source. `rev` combined with `traversal` is proven refused, not accepted, by
/// `search_traversal_with_rev_refuses_capability_unavailable` below; a runtime refusal has no
/// structured content to validate against this corpus's output schema.
fn traversal_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "search",
            json!({
                "traversal": { "seed": TRAVERSAL_CALLEE }
            }),
        ),
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "direction": "incoming",
                    "depth": 2
                }
            }),
        ),
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "to": TRAVERSAL_CALLER
                }
            }),
        ),
        // The engine lane resolves references alone, so `implements` beside them has no
        // lane: this answer carries the `relationship_coverage_missing` warning and
        // validates the schema arm serving it.
        (
            "search",
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLEE,
                    "facets": ["implements", "references"]
                }
            }),
        ),
    ]
}

/// The declaration every walk in this corpus starts at.
const TRAVERSAL_CALLEE: &str = "rift://symbol/python/traversal_callee.py/callee";
/// The one declaration referencing it.
const TRAVERSAL_CALLER: &str = "rift://symbol/python/traversal_caller.py/calls_callee";

/// Search requests only the lexical search-index tier can fully answer: a multi-word
/// prose query merging in hits identifier search alone would not surface, and a query
/// that only `notes.txt` answers, since identifier search never reaches its content.
fn lexical_search_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({ "query": "beacon two three" })),
        ("search", json!({ "query": "rotating legacy sensor unit" })),
    ]
}

/// Revision-addressed requests, one per read tool: each answers from the
/// fixture's committed baseline.
fn revision_read_corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("get_symbol", json!({ "name": "beacon_one", "rev": "main" })),
        ("search", json!({ "query": "beacon", "rev": "main" })),
        (
            "nodes",
            json!({ "path": "lib.rs", "position": 0, "rev": "main" }),
        ),
    ]
}

/// A comparison of the fixture's two committed revisions: the `baseline` tag holds
/// everything before `change_witness.rs` arrived, so this answer carries the two
/// `introduced` hits that file brought and validates the `change` arm of the served
/// output schema against a real payload.
///
fn change_search_corpus() -> Vec<(&'static str, Value)> {
    vec![(
        "search",
        json!({
            "change": { "base": "baseline", "head": "HEAD" },
            "include": ["source"]
        }),
    )]
}

fn arguments(value: &Value) -> TestResult<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// The result rows a paginated answer pages: search hits or symbol hits. None for a tool
/// whose result carries no pagination.
fn paged_rows<'result>(name: &str, structured: &'result Value) -> Option<&'result [Value]> {
    let rows = match name {
        "search" => "results",
        "get_symbol" => "hits",
        _ => return None,
    };
    structured[rows].as_array().map(Vec::as_slice)
}

fn assert_validates(validator: &Validator, instance: &Value, context: &str) {
    let failures: Vec<String> = validator
        .iter_errors(instance)
        .map(|failure| failure.to_string())
        .collect();
    assert!(
        failures.is_empty(),
        "{context} must validate against the advertised schema: {failures:#?}\ninstance: {instance:#}"
    );
}

/// Whether every byte of `text` is a lowercase hex digit.
fn is_lowercase_hex(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Walks `value`, refusing a bare 64-character lowercase-hex string anywhere on the wire: the
/// only digest form the wire now carries is the eight-character witness.
fn assert_no_bare_sha256_digest(value: &Value, context: &str) {
    match value {
        Value::String(text) => assert!(
            !(text.len() == 64 && is_lowercase_hex(text)),
            "{context} must not carry a bare 64-character digest, only the 8-character wire \
             form: {text}"
        ),
        Value::Array(items) => {
            for item in items {
                assert_no_bare_sha256_digest(item, context);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                assert_no_bare_sha256_digest(item, context);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Proves one tool result carries no oversized digest and no non-project source-unit
/// resolver, that every `search` hit names exactly one location, and that a read result
/// warns only what it is entitled to.
///
/// `get_symbol` and `nodes` carry empty `warnings`: the live server resolves one published
/// workspace per request, so no request can observe a lagging index, and neither tool
/// consults the search index at all.
///
/// `search` does consult it, and the search tier is prepared behind the answers, so this
/// fixture's default `[search.semantic]` table legitimately produces
/// `semantic_index_preparing` while the corpus runs. What it must never produce is
/// `lexical_ranking_unavailable`: that warning is reserved for a tier that will not answer
/// without operator action, and one that fired in ordinary operation would be one every
/// caller learned to ignore.
///
/// A `get_symbol` or `search` request whose `scope` reaches dependencies runs after the
/// lane has indexed the fixture's helper, so no warning names the helper. The machine's
/// toolchain decides what else the catalog holds: a standard library cataloged with a
/// source root is reported pending while the lane walks it and skipped once the walk
/// crosses the index's bounds, so such a request may carry the dependency warnings and no
/// other.
fn assert_wire_hygiene(name: &str, request: &Value, structured: &Value) {
    let context = format!("{name} result");
    let reaches_dependencies = matches!(request["scope"].as_str(), Some("global" | "all"));
    assert_no_bare_sha256_digest(structured, &context);
    assert_source_unit_ids_use_served_resolvers(structured, &context, reaches_dependencies);
    if reaches_dependencies {
        assert_dependency_warnings_only(structured);
    }
    if matches!(name, "get_symbol" | "nodes") && !reaches_dependencies {
        assert!(
            structured.get("warnings").is_none(),
            "a live {name} result must omit warnings when there is nothing to warn about: \
             {structured:#}"
        );
    }
    if name == "get_symbol" {
        let include = request.get("include").and_then(Value::as_array);
        let expects_source = include.is_none_or(|entries| entries.contains(&Value::from("source")));
        let expects_history =
            include.is_some_and(|entries| entries.contains(&Value::from("history")));
        for hit in structured["hits"].as_array().into_iter().flatten() {
            assert_eq!(
                hit.get("source").is_some(),
                expects_source,
                "a hit carries source exactly when the request includes it: {request:#} {hit:#}"
            );
            // A dependency hit carries its package source location.
            let addressable = hit.get("unit").is_none();
            assert_eq!(
                hit.get("node").is_some(),
                expects_source && addressable,
                "the node address rides with source on a project hit: {request:#} {hit:#}"
            );
            if !expects_history {
                assert!(
                    hit.get("history").is_none(),
                    "a hit omits history unless the request includes it: {request:#} {hit:#}"
                );
            }
        }
    }
    if name == "search"
        && let Some(warnings) = structured["warnings"].as_array()
    {
        for warning in warnings {
            assert_ne!(
                warning["code"],
                json!("lexical_ranking_unavailable"),
                "an ordinary search must never spend the operator-action warning: \
                 {structured:#}"
            );
        }
    }
    if name == "search"
        && let Some(results) = structured["results"].as_array()
    {
        let source_requested = request["include"]
            .as_array()
            .is_some_and(|include| include.iter().any(|value| value == "source"));
        let score_requested = request["include"]
            .as_array()
            .is_some_and(|include| include.iter().any(|value| value == "score"));
        for hit in results {
            // A symbol hit is addressed by exactly one of `path` and `unit`; a file hit
            // by `path` alone.
            assert!(
                hit.get("path").is_some() != hit.get("unit").is_some(),
                "a search hit carries exactly one of path and unit: {hit:#}"
            );
            if hit["hit"]["target"] == json!("file") {
                assert!(
                    hit.get("path").is_some(),
                    "a file hit carries its project path: {hit:#}"
                );
            }
            if source_requested {
                assert!(
                    !hit["source"].is_null(),
                    "a hit must carry source once the request names \
                     include: [\"source\"]: {hit:#}"
                );
            } else {
                assert!(
                    hit["source"].is_null(),
                    "a hit must omit source when the request never names it: {hit:#}"
                );
            }
            if score_requested {
                assert!(
                    !hit["score"].is_null(),
                    "a hit must carry score once the request names \
                     include: [\"score\"]: {hit:#}"
                );
            } else {
                assert!(
                    hit["score"].is_null(),
                    "a hit must omit score when the request never names it: {hit:#}"
                );
            }
        }
    }
}

/// Every warning on a package-scoped answer is one of the package warnings, and none
/// reports the fixture's helper skipped.
fn assert_dependency_warnings_only(structured: &Value) {
    for warning in structured["warnings"].as_array().into_iter().flatten() {
        assert!(
            matches!(
                warning["code"].as_str(),
                Some("global_index_unavailable" | "package_skipped" | "package_context_degraded")
            ),
            "a package-scoped answer warns of the package branch alone: {warning:#}"
        );
        assert_ne!(
            warning["package"]["name"],
            json!("helper"),
            "the indexed helper is never reported skipped: {warning:#}"
        );
    }
}

/// Walks `value`, proving every `rift://source/` identity uses a resolver the request can
/// reach: the project resolver always, and the Cargo resolver when the request's `scope`
/// reaches the cataloged packages.
fn assert_source_unit_ids_use_served_resolvers(
    value: &Value,
    context: &str,
    reaches_dependencies: bool,
) {
    match value {
        Value::String(text) => {
            if let Some(rest) = text.strip_prefix("rift://source/") {
                let served = rest.starts_with("project/")
                    || (reaches_dependencies && rest.starts_with("cargo/"));
                assert!(
                    served,
                    "{context} source-unit id must use a served resolver: {text}"
                );
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_source_unit_ids_use_served_resolvers(item, context, reaches_dependencies);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                assert_source_unit_ids_use_served_resolvers(item, context, reaches_dependencies);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The fixture's directories: the served workspace and, beside it, the helper crate its
/// manifest depends on by path.
struct FixtureDirectories {
    _workspace: tempfile::TempDir,
    /// Held for the life of the server: the catalog roots the helper here.
    _helper: tempfile::TempDir,
}

/// The helper crate the fixture depends on: two public declarations and a private one,
/// so the corpus can prove a hit answered from the dependency index.
const HELPER_SOURCE: &str =
    "pub fn helper_beacon() {}\nfn helper_private() {}\npub fn beacon() {}\n";

/// A v4 lockfile naming the fixture and its path dependency, which
/// `cargo metadata --locked --offline` accepts as it stands.
const LOCK_WITH_HELPER: &str = "\
# This file is automatically @generated by Cargo.
# It is not intended for manual editing.
version = 4

[[package]]
name = \"fixture\"
version = \"0.1.0\"
dependencies = [
 \"helper\",
]

[[package]]
name = \"helper\"
version = \"0.1.0\"
";

/// Builds the shared fixture workspace and serves it to one client.
async fn served_fixture() -> TestResult<(
    FixtureDirectories,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let helper = tempfile::tempdir()?;
    fs::create_dir_all(helper.path().join("src"))?;
    fs::write(
        helper.path().join("Cargo.toml"),
        "[package]\nname = \"helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(helper.path().join("src/lib.rs"), HELPER_SOURCE)?;
    let directory = tempfile::tempdir()?;
    // The manifest names the helper by path, spelled as a TOML literal string so no
    // separator needs escaping; `cargo metadata` catalogs it as a direct dependency
    // rooted in its own directory.
    fs::write(
        directory.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [lib]\npath = \"lib.rs\"\n\n[dependencies]\nhelper = {{ path = '{}' }}\n",
            helper.path().display()
        ),
    )?;
    fs::write(directory.path().join("Cargo.lock"), LOCK_WITH_HELPER)?;
    fs::write(
        directory.path().join("lib.rs"),
        "pub fn beacon_one() {}\npub fn beacon_two() {}\npub fn beacon_three() {}\n",
    )?;
    // Gitignored, so a plain search never reaches it; `paths.force_include` is the only way
    // in, proving that arm of the surface end to end.
    fs::write(directory.path().join(".gitignore"), "hidden.rs\n")?;
    fs::write(
        directory.path().join("hidden.rs"),
        "pub fn phantom_signal() {}\n",
    )?;
    // Included into the lexical search-index tier by the default `[search.text]`
    // extensions, so the corpus can prove a text-file search hit end to end.
    fs::write(
        directory.path().join("notes.txt"),
        "Beacon telemetry guidance covers rotating every legacy sensor unit safely.\n",
    )?;
    // The one real call edge the traversal corpus walks. A configured language engine
    // resolves the references a walk follows, and this fixture selects the embedded `ty`
    // engine, so the pair is Python.
    fs::write(
        directory.path().join("traversal_callee.py"),
        "def callee() -> int:\n    return 1\n",
    )?;
    fs::write(
        directory.path().join("traversal_caller.py"),
        "from traversal_callee import callee\n\n\ndef calls_callee() -> int:\n    return callee()\n",
    )?;
    // No syntax provider claims it; `nodes` names the missing extension.
    fs::write(directory.path().join("justfile"), "default:\n    echo hi\n")?;
    // An npm scoped package directory, so the corpus mints an identity whose path holds the
    // `@` RFC 3986 keeps literal. The served `NodeId` pattern left `@` out and refused the
    // identity the server had just minted.
    fs::create_dir_all(directory.path().join("packages/@scope/name"))?;
    fs::write(
        directory.path().join("packages/@scope/name/package.json"),
        "{\n  \"name\": \"@scope/name\"\n}\n",
    )?;
    // A path carrying bytes `encode_path` escapes rather than keeps, so the corpus validates a
    // percent-encoded identity against the served pattern beside the literal one above.
    fs::write(
        directory.path().join("notes for café.json"),
        "{\n  \"beacon\": \"escaped\"\n}\n",
    )?;
    // A committed baseline, so the corpus can prove revision-addressed reads:
    // `hidden.rs` stays gitignored and uncommitted, everything else lands in
    // the fixture's one commit on `main`.
    //
    let configuration = format!("{}{ENGINE}", hermetic_search::SEMANTIC_DISABLED);
    fs::write(directory.path().join("rift.toml"), configuration)?;
    rift_history::fixture::init(directory.path());
    rift_history::fixture::commit_all(directory.path(), "fixture baseline");
    // A second commit, tagged apart from it, so a `change` request compares two
    // committed revisions that really differ.
    rift_history::fixture::git(directory.path(), &["tag", "baseline"]);
    // Both the declaration and its caller arrive in this commit and live in one file, so
    // the comparison answers two `introduced` hits.
    fs::write(
        directory.path().join("change_witness.rs"),
        "pub fn change_witness() {}\npub fn calls_change_witness() {\n    change_witness();\n}\n",
    )?;
    rift_history::fixture::commit_all(directory.path(), "introduce the change witness");
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
    Ok((
        FixtureDirectories {
            _workspace: directory,
            _helper: helper,
        },
        client,
        server_task,
    ))
}

/// Result-arm coverage the corpus walk accumulates: every arm a result
/// union can take must be proven by a live payload, and the walk fails when
/// one was never produced.
#[derive(Default)]
struct CorpusArms {
    multi_page_results: usize,
    past_end_pages: usize,

    dependency_units: usize,
    search_dependency_units: usize,
    literal_at_identities: usize,
    escaped_identities: usize,
}

impl CorpusArms {
    /// Records how many hits
    /// answered from the dependency index, on a `get_symbol` and a `search` answer alike.
    fn observe(&mut self, structured: &Value) {
        self.dependency_units += dependency_unit_count(&structured["hits"]);
        self.search_dependency_units += dependency_unit_count(&structured["results"]);
        for identity in node_identities(structured) {
            if identity.contains('@') && !identity.contains("%40") {
                self.literal_at_identities += 1;
            }
            if identity.contains('%') {
                self.escaped_identities += 1;
            }
        }
    }

    /// Fails the walk unless every tracked arm was produced live.
    fn assert_proven(&self) {
        assert!(
            self.multi_page_results > 0 && self.past_end_pages > 0,
            "the corpus must prove a multi-page result set and an empty page past the end: \
             multi_page_results={}, past_end_pages={}",
            self.multi_page_results,
            self.past_end_pages
        );
        assert!(
            self.dependency_units > 0,
            "the corpus must prove a get_symbol hit answered from the dependency index, \
             carrying unit in place of path"
        );
        assert!(
            self.search_dependency_units > 0,
            "the corpus must prove a search hit answered from the dependency index, \
             carrying unit in place of path"
        );
        assert!(
            self.literal_at_identities > 0 && self.escaped_identities > 0,
            "the corpus must mint one identity whose path keeps `@` literal and one whose \
             path carries a percent escape, and validate both against the served pattern: \
             literal_at_identities={}, escaped_identities={}",
            self.literal_at_identities,
            self.escaped_identities
        );
    }
}

/// Every node identity one answer carries, whatever tool produced it.
///
/// A `nodes` answer lists them under `id`; a search hit carries one under `node`.
fn node_identities(structured: &Value) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![structured];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    match member {
                        Value::String(identity)
                            if matches!(key.as_str(), "id" | "node")
                                && identity.starts_with("rift://node/") =>
                        {
                            found.push(identity.clone());
                        }
                        _ => pending.push(member),
                    }
                }
            }
            Value::Array(entries) => pending.extend(entries),
            _ => {}
        }
    }
    found
}

/// How many of `rows`, the hits of one paginated answer, carry `unit` in place of `path`.
fn dependency_unit_count(rows: &Value) -> usize {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter(|hit| hit.get("unit").is_some())
        .count()
}

/// Compiles one input and one output validator per advertised tool.
fn tool_validators(
    tools: &[rmcp::model::Tool],
) -> TestResult<BTreeMap<String, (Validator, Validator)>> {
    let mut validators = BTreeMap::new();
    for tool in tools {
        let input = Value::Object(tool.input_schema.as_ref().clone());
        let output = tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object(schema.as_ref().clone()))
            .ok_or_else(|| format!("tool {} must advertise an output schema", tool.name))?;
        validators.insert(
            tool.name.to_string(),
            (
                jsonschema::validator_for(&input)?,
                jsonschema::validator_for(&output)?,
            ),
        );
    }
    Ok(validators)
}

/// Most attempts one corpus request retries before giving up on acceptance.
const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// Calls one tool, retrying the refusal the server advertises as
/// `retry: same_request`: the refusal reports movement between one
/// request's snapshot and its acceptance, and the wire contract answers
/// that race with a bounded retry rather than a failure.
async fn call_tool_retrying_acceptance(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    params: CallToolRequestParams,
) -> TestResult<rmcp::model::CallToolResult> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        match client.call_tool(params.clone()).await {
            Ok(result) => return Ok(result),
            Err(rmcp::ServiceError::McpError(error))
                if error
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("the server kept refusing a retryable corpus request".into())
}

/// Most lookups the walk issues while the dependency lane still indexes the helper.
const INDEX_ATTEMPTS_MAX: usize = 60;
/// Pause between two of those lookups.
const INDEX_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Waits until the dependency lane has indexed the fixture's helper, so the corpus's
/// dependency-scoped requests answer from a held package rather than a pending one.
async fn helper_indexed(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> TestResult {
    let request = json!({ "name": "helper_beacon", "scope": "global" });
    for _attempt in 0..INDEX_ATTEMPTS_MAX {
        let result = call_tool_retrying_acceptance(
            client,
            CallToolRequestParams::new("get_symbol").with_arguments(arguments(&request)?),
        )
        .await?;
        let structured = result
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        if structured["hits"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty())
        {
            return Ok(());
        }
        tokio::time::sleep(INDEX_POLL).await;
    }
    Err("the dependency lane never indexed the fixture's helper within the poll bound".into())
}

#[tokio::test]
async fn every_tool_result_validates_against_served_output_schema() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    helper_indexed(&client).await?;
    let tools = client.list_all_tools().await?;

    let advertised: BTreeSet<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let covered: BTreeSet<&str> = corpus().iter().map(|(name, _)| *name).collect();
    assert_eq!(
        advertised, covered,
        "every advertised tool needs a validation corpus entry, and every \
         corpus entry an advertised tool: extend `corpus` alongside the surface"
    );

    let validators = tool_validators(&tools)?;

    let mut arms = CorpusArms::default();
    for (name, request) in corpus() {
        let (input_validator, output_validator) = validators
            .get(name)
            .ok_or_else(|| format!("corpus names unadvertised tool {name}"))?;
        let mut request = request;
        let mut followed_pages = 0_usize;
        loop {
            assert!(
                followed_pages <= FOLLOWED_PAGES_MAX,
                "page walk for {name} exceeded {FOLLOWED_PAGES_MAX} pages: \
                 the fixture is too large or the page count never converges"
            );
            assert_validates(input_validator, &request, &format!("{name} request"));
            let result = call_tool_retrying_acceptance(
                &client,
                CallToolRequestParams::new(name).with_arguments(arguments(&request)?),
            )
            .await?;
            let structured = result
                .structured_content
                .ok_or_else(|| format!("{name} must return structured content"))?;
            assert_validates(output_validator, &structured, &format!("{name} result"));
            assert_wire_hygiene(name, &request, &structured);
            arms.observe(&structured);
            let Some(pagination) = structured.get("pagination") else {
                break;
            };
            let page_index = pagination["page_index"]
                .as_u64()
                .unwrap_or_else(|| panic!("{name} pagination.page_index must be an integer"));
            let total_pages = pagination["total_pages"]
                .as_u64()
                .unwrap_or_else(|| panic!("{name} pagination.total_pages must be an integer"));
            let requested_page = request
                .get("page_index")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            assert_eq!(
                page_index, requested_page,
                "{name} must answer the page the request asked for"
            );
            if total_pages > 1 {
                arms.multi_page_results += 1;
            }
            if page_index >= total_pages {
                let rows =
                    paged_rows(name, &structured).ok_or_else(|| format!("{name} result rows"))?;
                assert!(
                    rows.is_empty(),
                    "{name} page {page_index} past total_pages {total_pages} must be empty: \
                     {structured:#}"
                );
                arms.past_end_pages += 1;
                break;
            }
            if page_index + 1 >= total_pages {
                break;
            }
            followed_pages += 1;
            request["page_index"] = json!(page_index + 1);
        }
    }
    arms.assert_proven();

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Every advertised tool carries at least one authored request example and
/// one authored result example, and each example validates against the
/// schema that carries it. The examples ride the exported document into the
/// docs, so a drifted example is a contract defect the same way a drifted
/// schema is - and a future tool cannot ship exampleless.
#[test]
fn every_tool_example_validates_against_its_advertised_schemas() -> TestResult {
    let document: Value = serde_json::from_str(&rift_mcp::schema::schema_document())?;
    let tools = document["tools"]
        .as_array()
        .ok_or("the exported document must list tools")?;
    assert!(!tools.is_empty(), "the exported document must list tools");
    for tool in tools {
        let name = tool["name"]
            .as_str()
            .ok_or("every exported tool must carry a name")?;
        for plane in ["input_schema", "output_schema"] {
            let schema = &tool[plane];
            let examples = schema["examples"]
                .as_array()
                .unwrap_or_else(|| panic!("tool {name} must carry at least one {plane} example"));
            assert!(
                !examples.is_empty(),
                "tool {name} must carry at least one {plane} example"
            );
            let validator = jsonschema::validator_for(schema)?;
            for (index, example) in examples.iter().enumerate() {
                assert_validates(
                    &validator,
                    example,
                    &format!("{name} {plane} example {index}"),
                );
            }
        }
    }
    Ok(())
}

/// `traversal` combined with `rev` is a schema-valid, runtime-refused request: the edge lane
/// serves the current tree alone, so the server refuses `capability_unavailable` rather than
/// answering. This is the one traversal case the schema-validating corpus above cannot carry,
/// since a refusal produces no structured content to validate.
#[tokio::test]
async fn search_traversal_with_rev_refuses_capability_unavailable() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let request = tools_call_request(
        "search",
        &json!({
            "rev": "main",
            "traversal": { "seed": TRAVERSAL_CALLEE }
        }),
    )?;
    let error = client
        .call_tool(request)
        .await
        .expect_err("traversal combined with rev must be refused");
    let rmcp::ServiceError::McpError(error) = error else {
        return Err(format!("expected an McpError, found {error:?}").into());
    };
    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!("capability_unavailable")),
        "{error:?}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn tools_call_request(name: &'static str, value: &Value) -> TestResult<CallToolRequestParams> {
    Ok(CallToolRequestParams::new(name).with_arguments(arguments(value)?))
}

/// A visible path no syntax provider parses refuses `capability_unavailable`, naming
/// the extension - `nodes` can never serve it whatever tree it is pointed at.
#[tokio::test]
async fn nodes_on_an_unparsed_visible_path_names_the_extension() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;

    let error = client
        .call_tool(
            CallToolRequestParams::new("nodes")
                .with_arguments(arguments(&json!({ "path": "justfile", "position": 0 }))?),
        )
        .await
        .expect_err("an unparsed extension must be rejected");
    let rmcp::ServiceError::McpError(data) = error else {
        panic!("expected protocol-level McpError, got {error:?}");
    };
    let wire = data.data.ok_or("wire error data must be present")?;
    assert_eq!(wire["code"], json!("capability_unavailable"));
    let message = wire["message"].as_str().ok_or("message must be a string")?;
    assert!(
        message.contains("files with no extension"),
        "the refusal must name what governs the missing provider: {message}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// MCP types a tool's `outputSchema` as an object schema, `type: "object"` at the top; a
/// client that validates the listing, such as the MCP Python SDK, refuses `tools/list`
/// without it, so a tagged-union result declares the type beside its `oneOf`.
#[tokio::test]
async fn every_advertised_output_schema_declares_the_object_type() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let mut missing = Vec::new();
    for tool in &tools {
        let Some(schema) = tool.output_schema.as_deref() else {
            continue;
        };
        if schema.get("type").and_then(Value::as_str) != Some("object") {
            missing.push(tool.name.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "tools/list advertises output schemas without `type: object`: {missing:?}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
