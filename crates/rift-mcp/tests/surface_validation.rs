//! Validates and verifies the behaviour of every advertised tool on the MCP
//! surface: each corpus request against the tool's advertised input schema,
//! each structured result against its advertised output schema, and every
//! sub-variant a result can take. The walk follows a paginated result page
//! by page - `page_index` climbing under the result's own `total_pages` -
//! so a live multi-page answer and the empty page past the end are both
//! proven against the schema. Every `ChangeResult` arm is proven the same
//! way: applied (with and without parser findings), and refused for a failed
//! precondition and an unsupported file-level change - plus a live witnessed
//! `replace_node` that lands after the walk.

// `fake_engine` is a shared helper file compiled separately into every test binary that
// declares it; this binary calls only `engine_configuration`, so `counted` and `recorded`
// read as dead code here even though `rename_symbol.rs` and `move_file.rs` call them.
#[allow(dead_code)]
mod fake_engine;
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

/// Sample validation corpus with various scenarios: one request per
/// advertised tool behavior worth proving.
fn corpus() -> Vec<(&'static str, Value)> {
    let mut requests = vec![
        ("get_symbol", json!({ "name": "beacon_one" })),
        ("get_symbol", json!({ "name": "beacon", "limit": 1 })),
        (
            "get_symbol",
            json!({ "name": "beacon", "include_body": false }),
        ),
        // The fixture's committed baseline serves the timeline: one
        // `introduced` version from the walk's first commit.
        (
            "get_symbol",
            json!({ "name": "beacon_one", "include_history": true }),
        ),
        ("search", json!({ "query": "beacon" })),
        ("search", json!({ "query": "beacon", "limit": 1 })),
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
            "replace_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/beacon_two",
                "body": "pub fn beacon_two() -> u8 {\n    2\n}"
            }),
        ),
        (
            "replace_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/vanished",
                "body": "pub fn vanished() {}"
            }),
        ),
        (
            "insert_symbol",
            json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon_three",
                "position": "after",
                "body": "pub fn beacon_four() {}"
            }),
        ),
        (
            "replace_node",
            json!({
                "node": "rift://node/rust/lib.rs@0-18#00000000",
                "body": "pub fn beacon_one() {}"
            }),
        ),
        (
            "replace_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/beacon_three",
                "body": "pub fn beacon_three( {"
            }),
        ),
        // The shared fixture configures no `[engines]` tables, so the
        // rename refuses `unsupported` with the no-engine capability text.
        (
            "rename_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/beacon_two",
                "new_name": "beacon_renamed"
            }),
        ),
    ];
    requests.extend(patch_corpus());
    requests.extend(move_file_corpus());
    requests.extend(revision_read_corpus());
    requests.extend(insert_symbol_file_target_corpus());
    requests.extend(lexical_search_corpus());
    requests.extend(remove_corpus());
    requests
}

/// `remove_symbol` requests against the `[engines.fake]` references-only engine: a clean
/// removal with no reference to find, a refusal when one stands, and the same target applied
/// under `force` once the refusal has proven the tree stayed untouched.
fn remove_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "remove_symbol",
            json!({
                "symbol": "rift://symbol/rust/remove_lonely.rs/beacon_lonely",
                "force": false
            }),
        ),
        (
            "remove_symbol",
            json!({
                "symbol": "rift://symbol/rust/remove_watched.rs/beacon_watched",
                "force": false
            }),
        ),
        (
            "remove_symbol",
            json!({
                "symbol": "rift://symbol/rust/remove_watched.rs/beacon_watched",
                "force": true
            }),
        ),
        // A stale witness, proving `remove_node` reaches the same witness verification
        // `replace_node` shares through `resolve_node`; the live-fetched witness case runs in
        // `live_witnessed_remove_node_checks_references_and_validates`.
        (
            "remove_node",
            json!({
                "node": "rift://node/rust/lib.rs@0-18#00000000",
                "force": false
            }),
        ),
    ]
}

/// `patch` requests: modifying, creating, and renaming a file, each proving one
/// unified-diff arm the tool advertises.
fn patch_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "patch",
            json!({
                "patch": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon_one() {}\n+pub fn beacon_one() -> u8 { 1 }\n"
            }),
        ),
        (
            "patch",
            json!({
                "patch": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn never_there() {}\n+pub fn never_there() -> u8 { 0 }\n"
            }),
        ),
        (
            "patch",
            json!({
                "patch": "--- /dev/null\n+++ b/fresh.rs\n@@ -0,0 +1 @@\n+pub fn fresh() {}\n"
            }),
        ),
        (
            "patch",
            json!({
                // The header claims line 1; the unique match actually sits
                // at line 5, proving header line numbers are hints only.
                "patch": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon_three() {}\n+pub fn beacon_three() -> u8 { 3 }\n"
            }),
        ),
        (
            "patch",
            json!({
                // The header counts 9 old and 4 new lines over a body
                // carrying one of each, proving the counts are read from
                // the body the way `git apply` reads them.
                "patch": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,9 +1,4 @@\n-pub fn beacon_two() {}\n+pub fn beacon_two() -> u8 { 2 }\n"
            }),
        ),
        (
            "patch",
            json!({
                "patch": "--- a/lib.rs\n+++ b/renamed.rs\n@@ -1 +1 @@\n-pub fn beacon_one() -> u8 { 1 }\n+pub fn beacon_one() -> u8 { 1 }\n"
            }),
        ),
    ]
}

/// `move_file` requests: an applied move into a created directory - the
/// fixture configures no `[engines]` tables, so its summary carries the
/// references-not-updated warning - a missing source, and an occupied
/// destination. `fresh.rs` exists because the patch corpus created it.
fn move_file_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "move_file",
            json!({ "from": "fresh.rs", "to": "moved/fresh.rs" }),
        ),
        (
            "move_file",
            json!({ "from": "ghost.rs", "to": "ghost_two.rs" }),
        ),
        ("move_file", json!({ "from": "lib.rs", "to": "notes.txt" })),
    ]
}

/// `move_file` requests that must fail the advertised input schema before
/// any tool resolves them: a state path, an escaping path, and an unknown
/// field.
fn invalid_move_file_corpus() -> Vec<Value> {
    vec![
        json!({ "from": "lib.rs", "to": ".rift/x.rs" }),
        json!({ "from": "../escape.rs", "to": "lib2.rs" }),
        json!({ "from": "lib.rs", "to": "lib2.rs", "overwrite": true }),
    ]
}

/// Search requests only the lexical search-index tier can fully answer: a multi-word
/// prose query merging in hits identifier search alone would not surface, and a query
/// that only `notes.txt` (included by the default `[search.text]` extensions) answers,
/// since identifier search never reaches a non-source file.
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

/// `insert_symbol` file-target requests: an append to an existing file, a
/// created file with nested parent directories, and a missing-target refusal.
fn insert_symbol_file_target_corpus() -> Vec<(&'static str, Value)> {
    vec![
        (
            "insert_symbol",
            json!({
                "file": "lib.rs",
                "position": "after",
                "body": "pub fn beacon_extra() {}"
            }),
        ),
        (
            "insert_symbol",
            json!({
                "file": "docs/notes.md",
                "position": "before",
                "create_missing": true,
                "body": "# Notes"
            }),
        ),
        (
            "insert_symbol",
            json!({
                "file": "docs/missing.md",
                "position": "after",
                "body": "# Missing"
            }),
        ),
    ]
}

/// `insert_symbol` requests that must fail the advertised input schema before
/// any tool ever resolves them: each proves one refused target-shape rule.
fn invalid_insert_symbol_corpus() -> Vec<Value> {
    vec![
        json!({
            "anchor": "rift://symbol/rust/lib.rs/beacon_one",
            "file": "notes/extra.md",
            "position": "after",
            "body": "x"
        }),
        json!({
            "position": "after",
            "body": "x"
        }),
        json!({
            "anchor": "rift://symbol/rust/lib.rs/beacon_one",
            "position": "after",
            "body": "x",
            "create_missing": true
        }),
        json!({
            "file": ".rift/x.rs",
            "position": "after",
            "body": "x"
        }),
        json!({
            "file": "../escape.rs",
            "position": "after",
            "body": "x"
        }),
    ]
}

/// `rename_symbol` requests that must fail the advertised input schema
/// before any tool resolves them: a malformed address, an empty name, an
/// oversized name, and an unknown field.
fn invalid_rename_symbol_corpus() -> Vec<Value> {
    vec![
        json!({ "symbol": "not-an-address", "new_name": "beacon_renamed" }),
        json!({ "symbol": "rift://symbol/rust/lib.rs/beacon_one", "new_name": "" }),
        json!({
            "symbol": "rift://symbol/rust/lib.rs/beacon_one",
            "new_name": "n".repeat(257)
        }),
        json!({
            "symbol": "rift://symbol/rust/lib.rs/beacon_one",
            "new_name": "beacon_renamed",
            "dry_run": true
        }),
    ]
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
/// resolver, that every `search` hit names its project-relative path, and that a read
/// result warns only what it is entitled to.
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
fn assert_wire_hygiene(name: &str, structured: &Value) {
    let context = format!("{name} result");
    assert_no_bare_sha256_digest(structured, &context);
    assert_source_unit_ids_use_project_resolver(structured, &context);
    if matches!(name, "get_symbol" | "nodes") {
        assert_eq!(
            structured["warnings"],
            json!([]),
            "a live {name} result must carry empty warnings: {structured:#}"
        );
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
        for hit in results {
            assert!(
                !hit["path"].is_null(),
                "a search hit's path must not be null: {hit:#}"
            );
        }
    }
}

/// Walks `value`, proving every `rift://source/` identity uses the project resolver: the
/// only source resolver this release serves.
fn assert_source_unit_ids_use_project_resolver(value: &Value, context: &str) {
    match value {
        Value::String(text) => {
            if let Some(rest) = text.strip_prefix("rift://source/") {
                assert!(
                    rest.starts_with("project/"),
                    "{context} source-unit id must use the project resolver: {text}"
                );
            }
        }
        Value::Array(items) => {
            for item in items {
                assert_source_unit_ids_use_project_resolver(item, context);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                assert_source_unit_ids_use_project_resolver(item, context);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Builds the shared fixture workspace and serves it to one client.
async fn served_fixture() -> TestResult<(
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let directory = tempfile::tempdir()?;
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
    // Unreferenced anywhere, so removing it proves the checked-clean arm; `remove_watched.rs`
    // and `remove_caller.rs` give the removal corpus a standing reference to find instead.
    fs::write(
        directory.path().join("remove_lonely.rs"),
        "pub fn beacon_lonely() {}\n",
    )?;
    fs::write(
        directory.path().join("remove_watched.rs"),
        "pub fn beacon_watched() {}\n",
    )?;
    fs::write(
        directory.path().join("remove_caller.rs"),
        "pub fn calls_watched() {\n    beacon_watched();\n}\n",
    )?;
    // A committed baseline, so the corpus can prove revision-addressed reads:
    // `hidden.rs` stays gitignored and uncommitted, everything else lands in
    // the fixture's one commit on `main`.
    //
    // The `[engines.fake]` table claims `rust` and advertises only
    // `textDocument/references`, so `remove_symbol` and `remove_node` reach a real reference
    // check without disturbing what every other tool sees: neither `rename_symbol` nor
    // `move_file` finds a rename or will-rename capability on this engine, so both still
    // refuse or fall back exactly as they do with no engine configured at all.
    let mut configuration = hermetic_search::SEMANTIC_DISABLED.to_owned();
    configuration.push('\n');
    configuration.push_str(&fake_engine::engine_configuration("references-only", "10s"));
    fs::write(directory.path().join("rift.toml"), configuration)?;
    rift_history::fixture::init(directory.path());
    rift_history::fixture::commit_all(directory.path(), "fixture baseline");
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

/// Result-arm coverage the corpus walk accumulates: every arm a result
/// union can take must be proven by a live payload, and the walk fails when
/// one was never produced.
#[derive(Default)]
struct CorpusArms {
    multi_page_results: usize,
    past_end_pages: usize,
    applied_changes: usize,
    applied_with_findings: usize,
    refusal_reasons: BTreeSet<String>,
    precondition_kinds: BTreeSet<String>,
}

impl CorpusArms {
    /// Records which change-status arms one structured result proves.
    fn observe(&mut self, structured: &Value) {
        match structured["status"].as_str() {
            Some("applied") => {
                self.applied_changes += 1;
                if structured["summary"]["diagnostics"]
                    .as_array()
                    .is_some_and(|findings| !findings.is_empty())
                {
                    self.applied_with_findings += 1;
                }
            }
            Some("refused") => {
                if let Some(reason) = structured["reason"].as_str() {
                    self.refusal_reasons.insert(reason.to_owned());
                }
                for precondition in structured["preconditions"].as_array().into_iter().flatten() {
                    if let Some(kind) = precondition["kind"].as_str() {
                        self.precondition_kinds.insert(kind.to_owned());
                    }
                }
            }
            _ => {}
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
            self.applied_changes >= 3 && self.applied_with_findings >= 1,
            "the corpus must prove the applied arm with and without parser findings: \
             applied={}, with_findings={}",
            self.applied_changes,
            self.applied_with_findings
        );
        for reason in ["unmet_precondition", "unsupported"] {
            assert!(
                self.refusal_reasons.contains(reason),
                "the corpus must prove the {reason} refusal arm; proven: {:?}",
                self.refusal_reasons
            );
        }
        assert!(
            self.precondition_kinds.contains("no_references"),
            "the corpus must prove the no_references precondition; proven: {:?}",
            self.precondition_kinds
        );
    }
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
/// `retry: same_request`: the workspace's own filesystem watcher can
/// observe a corpus change's write and move the index between one
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

#[tokio::test]
async fn every_tool_result_validates_against_served_output_schema() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
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
            assert_wire_hygiene(name, &structured);
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

#[tokio::test]
async fn insert_symbol_schema_rejects_invalid_target_combinations() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;
    let (input_validator, _) = &validators["insert_symbol"];
    for request in invalid_insert_symbol_corpus() {
        assert!(
            input_validator.iter_errors(&request).next().is_some(),
            "insert_symbol request must fail its advertised schema: {request:#}"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn move_file_schema_rejects_invalid_requests() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;
    let (input_validator, _) = &validators["move_file"];
    for request in invalid_move_file_corpus() {
        assert!(
            input_validator.iter_errors(&request).next().is_some(),
            "move_file request must fail its advertised schema: {request:#}"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn rename_symbol_schema_rejects_invalid_requests() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;
    let (input_validator, _) = &validators["rename_symbol"];
    for request in invalid_rename_symbol_corpus() {
        assert!(
            input_validator.iter_errors(&request).next().is_some(),
            "rename_symbol request must fail its advertised schema: {request:#}"
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A hunk whose header counts disagree with its body applies, and the
/// applied summary names the hunk's own region rather than the whole file.
#[tokio::test]
async fn miscounted_hunk_header_applies_and_reports_its_own_region() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;

    // The body carries one old and one new line under a header claiming 9
    // and 4. `git apply` reads the counts from the body; so does the server.
    let miscounted = json!({
        "patch": "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,9 +1,4 @@\n-pub fn beacon_two() {}\n+pub fn beacon_two() -> u8 { 2 }\n"
    });
    let arguments = arguments(&miscounted)?;
    let request = CallToolRequestParams::new("patch").with_arguments(arguments);
    let applied = client.call_tool(request).await?;
    let applied = applied
        .structured_content
        .ok_or("patch must return structured content")?;
    assert_eq!(
        applied["status"],
        json!("applied"),
        "a miscounted header must apply on its context alone: {applied:#}"
    );

    let edits = applied["summary"]["edits"]
        .as_array()
        .ok_or("an applied patch must carry its edits")?;
    assert_eq!(edits.len(), 1, "one hunk mints one edit: {applied:#}");
    let span = &edits[0]["span"]["range"];
    assert_eq!(
        (span["start"].as_u64(), span["end"].as_u64()),
        (Some(23), Some(46)),
        "the edit names the second declaration's own line: {applied:#}"
    );
    assert_eq!(edits[0]["text"], json!("pub fn beacon_two() -> u8 { 2 }\n"));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A file `insert_symbol` creates lands in the rebuilt snapshot, so a later
/// read sees the symbol it declares.
#[tokio::test]
async fn insert_symbol_file_target_creation_is_visible_to_a_later_read() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;

    let created = client
        .call_tool(
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "file": "extra.rs",
                "position": "after",
                "create_missing": true,
                "body": "pub fn beacon_extra_read() {}"
            }))?),
        )
        .await?;
    let created = created
        .structured_content
        .ok_or("insert_symbol must return structured content")?;
    assert_eq!(
        created["status"],
        json!("applied"),
        "a missing file target with create_missing must land: {created:#}"
    );

    let found = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({ "name": "beacon_extra_read" }))?),
        )
        .await?;
    let found = found
        .structured_content
        .ok_or("get_symbol must return structured content")?;
    let hits = found["hits"]
        .as_array()
        .ok_or("get_symbol must return hits")?;
    assert!(
        !hits.is_empty(),
        "a file insert_symbol just created must be visible to a later read: {found:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn live_witnessed_replace_node_lands_and_validates() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;

    let listing = client
        .call_tool(
            CallToolRequestParams::new("nodes")
                .with_arguments(arguments(&json!({ "path": "lib.rs", "position": 3 }))?),
        )
        .await?;
    let listing = listing
        .structured_content
        .ok_or("nodes must return structured content")?;
    let witnessed = listing["nodes"][0]["id"]
        .as_str()
        .ok_or("listing must carry a node id")?
        .to_owned();
    let replaced = client
        .call_tool(
            CallToolRequestParams::new("replace_node").with_arguments(arguments(
                &json!({ "node": witnessed, "body": "pub fn beacon_one() {}" }),
            )?),
        )
        .await?;
    let replaced = replaced
        .structured_content
        .ok_or("replace_node must return structured content")?;
    let (_, output_validator) = &validators["replace_node"];
    assert_validates(
        output_validator,
        &replaced,
        "live witnessed replace_node result",
    );
    assert_eq!(
        replaced["status"],
        json!("applied"),
        "a fresh witnessed address must land: {replaced:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A fresh witnessed node address round-trips through `remove_node` the way
/// [`live_witnessed_replace_node_lands_and_validates`] proves it for `replace_node`: the
/// listed node names `beacon_lonely` - unreferenced anywhere in the fixture - so the
/// `[engines.fake]` references-only engine checks it clean and the removal applies with no
/// warning.
#[tokio::test]
async fn live_witnessed_remove_node_checks_references_and_validates() -> TestResult {
    let (directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;

    let listing = client
        .call_tool(
            CallToolRequestParams::new("nodes").with_arguments(arguments(
                &json!({ "path": "remove_lonely.rs", "position": 3 }),
            )?),
        )
        .await?;
    let listing = listing
        .structured_content
        .ok_or("nodes must return structured content")?;
    let witnessed = listing["nodes"][0]["id"]
        .as_str()
        .ok_or("listing must carry a node id")?
        .to_owned();
    let removed = client
        .call_tool(
            CallToolRequestParams::new("remove_node")
                .with_arguments(arguments(&json!({ "node": witnessed, "force": false }))?),
        )
        .await?;
    let removed = removed
        .structured_content
        .ok_or("remove_node must return structured content")?;
    let (_, output_validator) = &validators["remove_node"];
    assert_validates(
        output_validator,
        &removed,
        "live witnessed remove_node result",
    );
    assert_eq!(
        removed["status"],
        json!("applied"),
        "an unreferenced declaration removes cleanly: {removed:#}"
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("remove_lonely.rs"))?,
        "",
        "the sole declaration leaves the file empty: {removed:#}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A configured hook's verdicts ride the applied change: a passing hook's
/// guarantees become validated evidence, a failing hook an error finding.
#[cfg(unix)]
#[tokio::test]
async fn hooked_change_carries_validated_guarantees_and_findings() -> TestResult {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon_one() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        hermetic_search::SEMANTIC_DISABLED.to_owned()
            + r#"
[[hooks]]
type = "command"
id = "echoes"
kind = "other"
program = "echo"
arguments = ["checked"]
changed_paths = "append"
working_directory = ""
environment = {}
timeout = "30s"
output_limit = "4kb"
guarantees = [
    { kind = "behavior_checked", scope = { kind = "reach", reach = "project" }, detail = "echo ran over the changed paths" },
]
determinism = "deterministic"

[[hooks]]
type = "command"
id = "refuses"
kind = "other"
program = "false"
arguments = []
changed_paths = "none"
working_directory = ""
environment = {}
timeout = "30s"
output_limit = "4kb"
guarantees = []
determinism = "deterministic"
"#,
    )?;
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
    let tools = client.list_all_tools().await?;
    let validators = tool_validators(&tools)?;

    let changed = client
        .call_tool(
            CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
                "symbol": "rift://symbol/rust/lib.rs/beacon_one",
                "body": "pub fn beacon_one() -> u8 { 1 }"
            }))?),
        )
        .await?;
    let structured = changed
        .structured_content
        .ok_or("replace_symbol must return structured content")?;
    let (_, output_validator) = &validators["replace_symbol"];
    assert_validates(
        output_validator,
        &structured,
        "hooked replace_symbol result",
    );
    assert_eq!(structured["status"], json!("applied"));

    let guarantees = structured["summary"]["guarantees"]
        .as_array()
        .ok_or("summary must carry guarantees")?;
    assert_eq!(
        guarantees,
        &vec![json!({
            "kind": "behavior_checked",
            "scope": { "kind": "reach", "reach": "project" },
            "hook": "echoes",
            "detail": "echo ran over the changed paths"
        })],
        "the passing hook's configured guarantee must ride the change"
    );

    let findings = structured["summary"]["diagnostics"]
        .as_array()
        .ok_or("summary must carry diagnostics")?;
    let failure = findings
        .iter()
        .find(|finding| finding["code"] == json!("rift.hook.failed"))
        .ok_or("the failing hook must contribute a rift.hook.failed finding")?;
    assert_eq!(failure["severity"], json!("error"));
    let message = failure["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("refuses") && message.contains("exited 1"),
        "the finding must name the hook and what ended it: {message}"
    );

    client.cancel().await?;
    server_task.await?;
    Ok(())
}
