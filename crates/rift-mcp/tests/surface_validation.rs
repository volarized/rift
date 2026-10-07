//! Served read requests and responses validate against their advertised schemas.

#[cfg(unix)]
mod fake_engine;
#[allow(
    dead_code,
    reason = "shared fixture global API exposes helpers this suite does not use"
)]
mod global_api;
mod hermetic_search;
// This binary serves its own fixture and uses `workspace_client` only to wait for map readiness.
#[expect(dead_code, reason = "the relative-root helpers serve other suites")]
mod workspace_client;
// The text checks of `nodes`, `search`, and `get_symbol` results, and their own tests; this
// binary alone declares it.
mod text_complete;

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use global_api::GlobalFixture;
use jsonschema::Validator;
use rift_index::WorkspaceIndexLimits;
use rift_mcp::RiftMcp;
use rift_protocol::error::{ErrorCode, RetryDirective};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Most pages one corpus request may walk before the gate fails.
const FOLLOWED_PAGES_MAX: usize = 16;

#[path = "surface_corpus.rs"]
mod surface_corpus;
use surface_corpus::{TRAVERSAL_CALLEE, TRAVERSAL_CALLER, corpus};

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

/// Proves one tool result carries no non-project source-unit
/// resolver, that every `search` hit names exactly one location, and that a read result
/// warns only what it is entitled to.
///
/// `get_symbol` and `nodes` carry empty `warnings`: the live server resolves one published
/// workspace per request, so no request can observe a lagging index, and neither tool
/// consults the search index at all.
///
/// `search` does consult it, and the search tier is prepared behind the answers, so this
/// fixture's default `[search.vector]` table legitimately produces
/// `vector_index_preparing` while the corpus runs. What it must never produce is
/// `lexical_ranking_unavailable`: a search answers a lexical commit still in flight with
/// that warning instead of waiting for it, and waits for a busy connection pool within
/// `[search] busy_timeout`. This suite asserts the warning never fires, so a commit in
/// flight while it runs, or a pool busy past that bound, fails it rather than becoming a
/// warning every caller learned to ignore.
///
/// A `get_symbol` or `search` request whose `scope` reaches packages carries the package
/// warnings and no other: the fixture's path dependency `helper` answers
/// `package_unavailable`, the standard library entry the fixture global API cannot
/// resolve answers `package_requirement_absent`, and the global API's own page warnings
/// arrive as `global_page_warning`.
fn assert_wire_hygiene(name: &str, request: &Value, structured: &Value) {
    let context = format!("{name} result");
    let reaches_dependencies = matches!(request["scope"].as_str(), Some("global" | "all"));
    // Schema validation above checks the separate revision and documentation digest forms.
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
            // A package hit carries `unit` and no node address.
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
                "an ordinary search must never rank without the full-text tier: \
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
            assert_search_hit_address(hit, reaches_dependencies);
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

/// A search hit in a file is addressed by exactly one of `path` and `unit`, and a file hit
/// carries `unit` only for a package file, which a package scope alone reaches. A commit
/// hit carries neither.
fn assert_search_hit_address(hit: &Value, reaches_dependencies: bool) {
    if hit["hit"]["target"] == json!("commit") {
        assert!(
            hit.get("path").is_none() && hit.get("unit").is_none(),
            "a commit hit carries neither path nor unit: {hit:#}"
        );
        return;
    }
    assert!(
        hit.get("path").is_some() != hit.get("unit").is_some(),
        "a search hit in a file carries exactly one of path and unit: {hit:#}"
    );
    if hit["hit"]["target"] == json!("file") {
        assert!(
            hit.get("path").is_some() || reaches_dependencies,
            "a project-scoped file hit carries its project path: {hit:#}"
        );
    }
}

/// Every warning on a package-scoped answer is one of the package warnings.
fn assert_dependency_warnings_only(structured: &Value) {
    for warning in structured["warnings"].as_array().into_iter().flatten() {
        assert!(
            matches!(
                warning["code"].as_str(),
                Some(
                    "global_access_disabled"
                        | "global_api_unavailable"
                        | "global_publication_incompatible"
                        | "global_response_invalid"
                        | "global_page_warning"
                        | "package_absent"
                        | "package_requirement_absent"
                        | "package_substituted"
                        | "package_unavailable"
                        | "package_context_degraded"
                )
            ),
            "a package-scoped answer warns of the packages alone: {warning:#}"
        );
    }
}

/// Walks `value`, proving every `rift://source/` identity uses a resolver the request can
/// reach: the project resolver always, and the Cargo resolver when the request's `scope`
/// reaches the packages the global API serves.
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

/// Builds the shared fixture workspace and serves it to one client.
async fn served_fixture() -> TestResult<(
    surface_corpus::SurfaceFixture,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let fixture = surface_corpus::SurfaceFixture::start().await?;
    let server = RiftMcp::build(fixture.root(), WorkspaceIndexLimits::default()).await?;
    let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(async move {
        let service = server
            .serve(server_transport)
            .await
            .expect("server must initialize");
        service.waiting().await.expect("server must stop cleanly");
    });
    let client = ().serve(client_transport).await?;
    workspace_client::await_workspace_ready(&client).await?;
    Ok((fixture, client, server_task))
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
    unavailable_entries: usize,
    replaced_entries: usize,
    added_entries: usize,
    literal_at_identities: usize,
    escaped_identities: usize,
    outgoing_hops: usize,
    callees_dropped: usize,
}

impl CorpusArms {
    /// Records how many hits the global API answered, on a `get_symbol` and a `search`
    /// answer alike, and how many entries the answer names `package_unavailable`.
    fn observe(&mut self, structured: &Value) {
        self.dependency_units += dependency_unit_count(&structured["hits"]);
        self.search_dependency_units += dependency_unit_count(&structured["results"]);
        self.unavailable_entries += structured["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|warning| warning["code"] == "package_unavailable")
            .count();
        for warning in structured["warnings"].as_array().into_iter().flatten() {
            if warning["code"] == "package_absent" && warning["package"]["name"] == "helper" {
                self.replaced_entries += 1;
            }
            if warning["code"] == "package_requirement_absent"
                && warning["entry"]["name"] == "extra"
            {
                self.added_entries += 1;
            }
        }
        for identity in node_identities(structured) {
            if identity.contains('@') && !identity.contains("%40") {
                self.literal_at_identities += 1;
            }
            if identity.contains('%') {
                self.escaped_identities += 1;
            }
        }
        self.outgoing_hops += outgoing_call_hops(structured);
        self.callees_dropped += warning_count(structured, "callees_dropped");
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
            "the corpus must prove a get_symbol hit answered by the global API, \
             carrying unit in place of path"
        );
        assert!(
            self.search_dependency_units > 0,
            "the corpus must prove a search hit answered by the global API, \
             carrying unit in place of path"
        );
        assert!(
            self.unavailable_entries > 0,
            "the corpus must prove a package_unavailable warning naming the fixture's \
             path dependency"
        );
        assert!(
            self.replaced_entries > 0 && self.added_entries > 0,
            "the corpus must prove a `packages` entry replacing the path dependency's \
             version and one adding a package the context lacks: replaced_entries={}, \
             added_entries={}",
            self.replaced_entries,
            self.added_entries
        );
        assert!(
            self.literal_at_identities > 0 && self.escaped_identities > 0,
            "the corpus must mint one identity whose path keeps `@` literal and one whose \
             path carries a percent escape, and validate both against the served pattern: \
             literal_at_identities={}, escaped_identities={}",
            self.literal_at_identities,
            self.escaped_identities
        );
        assert!(
            self.outgoing_hops > 0 && self.callees_dropped > 0,
            "the corpus must prove an outgoing walk's `calls` hop and its `callees_dropped` \
             warning against the served schema: outgoing_hops={}, callees_dropped={}",
            self.outgoing_hops,
            self.callees_dropped
        );
    }
}

/// How many hops across one answer's traversal paths an outgoing walk followed through a
/// `calls` relationship.
fn outgoing_call_hops(structured: &Value) -> usize {
    structured["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hit| hit["traversal_path"].as_array())
        .flatten()
        .filter(|hop| {
            hop["direction"] == json!("outgoing")
                && hop["relationship"]["facets"] == json!(["calls"])
        })
        .count()
}

/// How many of one answer's warnings carry `code`.
fn warning_count(structured: &Value, code: &str) -> usize {
    structured["warnings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|warning| warning["code"] == json!(code))
        .count()
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

/// Calls one tool, retrying the failure the server advertises as
/// `retry: same_request`: the failure reports movement between one
/// request's snapshot and its acceptance, and the wire contract answers
/// that race with a bounded retry rather than a failure.
async fn call_tool_retrying_acceptance(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    params: CallToolRequestParams,
) -> TestResult<rmcp::model::CallToolResult> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        let result = client.call_tool(params.clone()).await?;
        if result.is_error != Some(true) {
            return Ok(result);
        }
        let failure = workspace_client::tool_failure(&result)?;
        if failure.retry != RetryDirective::SameRequest {
            return Err(format!("the tool failed: {}", failure.text).into());
        }
    }
    Err("the server kept refusing a retryable corpus request".into())
}

#[tokio::test]
async fn every_tool_result_validates_against_served_output_schema() -> TestResult {
    let (_fixture, client, server_task) = served_fixture().await?;
    // Map readiness covers local file preparation; wait for lexical population before the corpus.
    workspace_client::search_after_population(&client, &json!({ "query": "beacon" })).await?;
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
            text_complete::assert_text_states(name, &result);
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

/// The served input schema's verdict on a request the server refuses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputSchema {
    /// The schema admits the request, and the server alone refuses it.
    Admits,
    /// The schema states the rule too, and the server enforces it for a caller that
    /// validates nothing.
    Refuses,
}

/// `traversal` requests the server refuses, with the input schema's verdict on each and the
/// code its refusal carries. An engine session serves the current tree alone, so a walk
/// beside `rev` or `change` refuses; the relationship graph serves the project alone, so a
/// walk under `scope: "global"` refuses; and call hierarchy names calls alone, so an
/// outgoing walk asking for `references` alone has no lane.
fn traversal_refusal_corpus() -> Vec<(Value, InputSchema, ErrorCode)> {
    let outgoing = json!({ "seed": TRAVERSAL_CALLER, "direction": "outgoing" });
    vec![
        (
            json!({ "rev": "main", "traversal": { "seed": TRAVERSAL_CALLEE } }),
            InputSchema::Admits,
            ErrorCode::CapabilityUnavailable,
        ),
        (
            json!({ "rev": "main", "traversal": outgoing }),
            InputSchema::Admits,
            ErrorCode::CapabilityUnavailable,
        ),
        (
            json!({ "scope": "global", "traversal": outgoing }),
            InputSchema::Admits,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({
                "traversal": {
                    "seed": TRAVERSAL_CALLER,
                    "direction": "outgoing",
                    "facets": ["references"]
                }
            }),
            InputSchema::Admits,
            ErrorCode::CapabilityUnavailable,
        ),
        (
            json!({ "change": { "base": "baseline", "head": "HEAD" }, "traversal": outgoing }),
            InputSchema::Refuses,
            ErrorCode::CapabilityUnavailable,
        ),
    ]
}

/// Every refused walk draws its code, and the served input schema admits exactly the ones
/// the server alone refuses. A refusal carries no structured content, so the corpus walk
/// above cannot hold these requests.
#[tokio::test]
async fn every_refused_traversal_carries_its_code_and_the_schema_verdict() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, _output_validator) = validators
        .get("search")
        .ok_or("search must be advertised")?;
    for (request, verdict, code) in traversal_refusal_corpus() {
        assert_eq!(
            input_validator.is_valid(&request),
            verdict == InputSchema::Admits,
            "the served input schema's verdict on {request:#}"
        );
        let failure = workspace_client::failed_call(
            client
                .call_tool(tools_call_request("search", &request)?)
                .await,
        )?;
        assert_eq!(failure.code, code, "{request:#} {failure:?}");
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A language engine serving call hierarchy answers the two outgoing arms the embedded
/// engine above never does. A seed the ready engine prepares no item at refuses
/// `capability_unavailable`, naming the seed's kind, and an engine that announced no work
/// answers with `engine_readiness_unconfirmed`. Each request validates against the served
/// input schema, and the answer against the served output schema.
#[cfg(unix)]
#[tokio::test]
async fn outgoing_walks_over_scripted_engines_match_the_served_schemas() -> TestResult {
    let (_engine, configuration) = fake_engine::rust_engine(fake_engine::READY_UNPREPARED_ENGINE)?;
    let (_directory, client, _server_task) = workspace_client::served_workspace(
        &[("lib.rs", "pub struct Beacon;\n")],
        Some(configuration),
    )
    .await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, output_validator) = validators
        .get("search")
        .ok_or("search must be advertised")?;
    workspace_client::call_retrying_acceptance(
        &client,
        workspace_client::tool_request("search", &json!({ "query": "Beacon" })),
    )
    .await?;
    let unprepared = json!({
        "traversal": { "seed": "rift://symbol/rust/lib.rs/Beacon", "direction": "outgoing" }
    });
    assert_validates(
        input_validator,
        &unprepared,
        "an outgoing walk from a struct",
    );
    let failure = workspace_client::failed_call(
        client
            .call_tool(tools_call_request("search", &unprepared)?)
            .await,
    )?;
    assert_eq!(
        failure.code,
        ErrorCode::CapabilityUnavailable,
        "{failure:?}"
    );
    assert!(failure.message.contains("of kind `struct`"), "{failure:?}");
    client.cancel().await?;

    let (_engine, (_directory, client, _server_task)) = fake_engine::scripted_calls_workspace(
        fake_engine::UNANNOUNCED_WORK,
        fake_engine::SERVED_LINES,
    )
    .await?;
    let unconfirmed = json!({
        "traversal": { "seed": "rift://symbol/rust/lib.rs/caller", "direction": "outgoing" }
    });
    assert_validates(input_validator, &unconfirmed, "an outgoing walk");
    let structured = workspace_client::call_retrying_acceptance(
        &client,
        workspace_client::tool_request("search", &unconfirmed),
    )
    .await?;
    assert_validates(
        output_validator,
        &structured,
        "an unconfirmed engine's walk",
    );
    assert_eq!(
        warning_count(&structured, "engine_readiness_unconfirmed"),
        1,
        "{structured:#}"
    );
    client.cancel().await?;
    Ok(())
}

/// A standard library callee the global API names answers as a hit carrying the global
/// identity and a `unit` in place of `path`, and a callee it names nothing at drops: the
/// answer validates against the served output schema.
#[cfg(unix)]
#[tokio::test]
async fn an_outgoing_walk_naming_package_callees_matches_the_served_schema() -> TestResult {
    let global = GlobalFixture::start_with(global_api::FixtureOptions {
        python_collection: true,
        ..global_api::FixtureOptions::default()
    })
    .await?;
    let configuration = format!(
        "[global]\nenabled = true\nendpoint = \"{}\"\nattempts = 1\n\
         request_timeout = \"10s\"\nconnect_timeout = \"5s\"\n\n\
         [languages.python.lsp]\nembedded = \"ty\"\n",
        global.endpoint
    );
    let (_directory, client, _server_task) = workspace_client::served_workspace(
        &[(
            "caller.py",
            "import json\n\n\ndef caller() -> int:\n    return len(json.dumps(1))\n",
        )],
        Some(configuration),
    )
    .await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, output_validator) = validators
        .get("search")
        .ok_or("search must be advertised")?;
    let walk = json!({
        "traversal": { "seed": "rift://symbol/python/caller.py/caller", "direction": "outgoing" }
    });
    assert_validates(input_validator, &walk, "an outgoing walk");
    let structured = workspace_client::call_retrying_acceptance(
        &client,
        workspace_client::tool_request("search", &walk),
    )
    .await?;
    assert_validates(
        output_validator,
        &structured,
        "an outgoing walk naming package callees",
    );
    let units: Vec<&Value> = structured["results"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hit| &hit["unit"])
        .collect();
    assert_eq!(
        units,
        [&json!("rift://source/stdlib/python@3.12.9/builtins.pyi")],
        "{structured:#}"
    );
    assert_eq!(
        warning_count(&structured, "callees_dropped"),
        1,
        "`json.dumps` is named nothing: {structured:#}"
    );
    client.cancel().await?;
    Ok(())
}

/// `packages` beside the `local` scope and beside `rev` are schema-valid, runtime-refused
/// requests on both reads: a project read consults no package, and package facts follow
/// the current tree alone, so the server refuses `invalid_request` naming `packages`.
/// A refusal carries no structured content, so the corpus above cannot hold them.
#[tokio::test]
async fn packages_beside_the_local_scope_or_rev_refuse_naming_packages() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let packages = json!([{ "manager": "cargo", "name": "demo", "version": "1.0.0" }]);
    let requests = [
        (
            "get_symbol",
            json!({ "name": "helper_beacon", "packages": packages }),
        ),
        (
            "get_symbol",
            json!({ "name": "helper_beacon", "scope": "all", "rev": "main", "packages": packages }),
        ),
        (
            "search",
            json!({ "query": "helper_beacon", "scope": "local", "packages": packages }),
        ),
        (
            "search",
            json!({ "query": "helper_beacon", "scope": "global", "rev": "main", "packages": packages }),
        ),
    ];
    for (name, request) in requests {
        let (input_validator, _) = validators
            .get(name)
            .ok_or_else(|| format!("{name} is advertised"))?;
        assert_validates(input_validator, &request, &format!("{name} request"));
        let failure = workspace_client::failed_call(
            client.call_tool(tools_call_request(name, &request)?).await,
        )?;
        assert_eq!(
            failure.code,
            ErrorCode::InvalidRequest,
            "{request}: {}",
            failure.text
        );
        assert!(
            failure.message.contains("field packages"),
            "the refusal names the field: {request}: {}",
            failure.message
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A `pattern` the server refuses is a schema-valid request answered with the refusal the
/// read path names: beside another result-set selector or a tree the trigram index does
/// not hold, out of syntax, and past the compiled-size bound it is `invalid_request`;
/// beside `target: "documentation"` it is `capability_unavailable`.
#[tokio::test]
async fn search_pattern_refusals_carry_their_codes() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let refused = [
        (
            json!({ "pattern": "beacon", "query": "beacon" }),
            ErrorCode::InvalidRequest,
        ),
        (
            json!({ "pattern": "beacon", "rev": "main" }),
            ErrorCode::InvalidRequest,
        ),
        (json!({ "pattern": "beacon(" }), ErrorCode::InvalidRequest),
        (json!({ "pattern": r"\w{2000}" }), ErrorCode::InvalidRequest),
        (
            json!({ "pattern": "beacon", "target": "documentation" }),
            ErrorCode::CapabilityUnavailable,
        ),
    ];
    for (arguments, code) in refused {
        let failure = workspace_client::failed_call(
            client
                .call_tool(tools_call_request("search", &arguments)?)
                .await,
        )?;
        assert_eq!(failure.code, code, "{arguments}: {failure:?}");
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// How long a test waits for the history store to hold the fixture's two commits.
const COMMIT_FILL_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// The poll interval a test asks again at while the history store lags.
const COMMIT_FILL_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// A commit hit, once the background fill holds the fixture's second commit, validates
/// against the served output schema and carries the commit's message, author, and paths.
#[tokio::test]
async fn a_commit_search_hit_validates_against_the_served_output_schema() -> TestResult {
    let (_fixture, client, server_task) = served_fixture().await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, output_validator) =
        validators.get("search").ok_or("search is advertised")?;
    let request = json!({ "target": "commit", "query": "witness" });
    assert_validates(input_validator, &request, "commit search request");

    let deadline = tokio::time::Instant::now() + COMMIT_FILL_WAIT_MAX;
    let structured = loop {
        let result =
            call_tool_retrying_acceptance(&client, tools_call_request("search", &request)?).await?;
        text_complete::assert_text_states("search", &result);
        let structured = result
            .structured_content
            .ok_or("search must return structured content")?;
        if structured["results"]
            .as_array()
            .is_some_and(|hits| !hits.is_empty())
        {
            break structured;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                format!("the history store never answered the commit: {structured}").into(),
            );
        }
        tokio::time::sleep(COMMIT_FILL_POLL).await;
    };

    assert_validates(output_validator, &structured, "commit search result");
    assert_wire_hygiene("search", &request, &structured);
    let commit = &structured["results"][0]["hit"]["commit"];
    assert_eq!(structured["results"][0]["hit"]["target"], json!("commit"));
    assert_eq!(commit["message"], json!("introduce the change witness\n"));
    assert_eq!(commit["message_truncated"], json!(false));
    assert_eq!(
        commit["author"],
        json!({ "name": "Rift Fixture", "email": "fixture@rift.invalid" })
    );
    assert_eq!(commit["paths"], json!(["change_witness.rs"]));
    assert_eq!(commit["paths_truncated"], json!(false));
    assert_eq!(structured["results"].as_array().map(Vec::len), Some(1));

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A revision spelling past the advertised form - a reflog selector, or an ancestry suffix
/// followed by a path - refuses `invalid_request` naming the field that carried it.
#[tokio::test]
async fn a_revision_spelling_past_the_advertised_form_refuses_naming_the_field() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let refused = [
        (
            "get_symbol",
            json!({ "name": "beacon_one", "rev": "HEAD@{1}" }),
            "field rev",
        ),
        (
            "search",
            json!({ "change": { "base": "HEAD~1/lib.rs" } }),
            "field change.base",
        ),
    ];
    for (name, arguments, field) in refused {
        let failure = workspace_client::failed_call(
            client
                .call_tool(tools_call_request(name, &arguments)?)
                .await,
        )?;
        assert_eq!(
            failure.code,
            ErrorCode::InvalidRequest,
            "{arguments}: {}",
            failure.text
        );
        assert!(
            failure.message.contains(field),
            "{arguments}: {}",
            failure.message
        );
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A commit search takes `query` alone: beside another selector, a revision, a package
/// argument, a path selector, or a `scope` past `local`, the server refuses
/// `invalid_request` naming the field.
#[tokio::test]
async fn search_commit_refusals_name_the_field() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let packages = json!([{ "manager": "cargo", "name": "demo", "version": "1.0.0" }]);
    let refused = [
        (json!({ "pattern": "witness" }), "pattern"),
        (
            json!({ "traversal": { "seed": "rift://symbol/rust/lib.rs/beacon_one" } }),
            "traversal",
        ),
        (json!({ "change": { "base": "baseline" } }), "change"),
        (json!({ "rev": "main" }), "rev"),
        (json!({ "packages": packages, "scope": "all" }), "packages"),
        (json!({ "paths": { "include": ["lib.rs"] } }), "paths"),
        (json!({ "scope": "global" }), "scope"),
    ];
    for (extra, field) in refused {
        let mut arguments = json!({ "target": "commit", "query": "witness" });
        if let (Some(arguments), Some(extra)) = (arguments.as_object_mut(), extra.as_object()) {
            arguments.extend(extra.clone());
        }
        let failure = workspace_client::failed_call(
            client
                .call_tool(tools_call_request("search", &arguments)?)
                .await,
        )?;
        assert_eq!(
            failure.code,
            ErrorCode::InvalidRequest,
            "{arguments}: {}",
            failure.text
        );
        assert!(
            failure.message.contains(&format!("field {field}")),
            "the refusal names {field}: {arguments}: {}",
            failure.message
        );
    }

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

    let failure = workspace_client::failed_call(
        client
            .call_tool(
                CallToolRequestParams::new("nodes")
                    .with_arguments(arguments(&json!({ "path": "justfile", "position": 0 }))?),
            )
            .await,
    )?;
    assert_eq!(failure.code, ErrorCode::CapabilityUnavailable);
    let message = &failure.message;
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

/// Whether one property schema accepts `null` beside another value.
fn has_null_arm(property: &Value) -> bool {
    let null_type = property["type"]
        .as_array()
        .is_some_and(|kinds| kinds.len() > 1 && kinds.contains(&json!("null")));
    let null_branch = property["anyOf"]
        .as_array()
        .is_some_and(|branches| branches.len() > 1 && branches.contains(&json!({"type": "null"})));
    null_type || null_branch
}

/// Every property below `node` that its object does not require and whose schema
/// accepts `null` beside another value, named by the path to it.
fn optional_null_properties(node: &Value, path: &str, found: &mut Vec<String>) {
    match node {
        Value::Object(object) => {
            let required = object.get("required").and_then(Value::as_array);
            let properties = object.get("properties").and_then(Value::as_object);
            found.extend(
                properties
                    .into_iter()
                    .flatten()
                    .filter(|(name, property)| {
                        has_null_arm(property)
                            && !required.is_some_and(|names| names.contains(&json!(name)))
                    })
                    .map(|(name, _)| format!("{path}/{name}")),
            );
            for (key, child) in object {
                optional_null_properties(child, &format!("{path}/{key}"), found);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                optional_null_properties(item, &format!("{path}/{index}"), found);
            }
        }
        _ => {}
    }
}

/// A wire model omits an absent optional field, so no property a served schema does not
/// require accepts `null`: the arm would advertise a value no request needs and no answer
/// carries. A required property may keep it, the form reserved for a `null` the server
/// treats apart from absence.
#[tokio::test]
async fn no_advertised_optional_property_accepts_null() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;
    let mut found = Vec::new();
    for tool in &tools {
        let input = Value::Object(tool.input_schema.as_ref().clone());
        optional_null_properties(&input, &format!("{}/input_schema", tool.name), &mut found);
        let output = tool
            .output_schema
            .as_ref()
            .map(|schema| Value::Object(schema.as_ref().clone()))
            .ok_or_else(|| format!("tool {} must advertise an output schema", tool.name))?;
        optional_null_properties(&output, &format!("{}/output_schema", tool.name), &mut found);
    }
    assert!(
        found.is_empty(),
        "tools/list advertises optional properties that accept null: {found:#?}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}
