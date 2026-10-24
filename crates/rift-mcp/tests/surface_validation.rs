//! Served read requests and responses validate against their advertised schemas.

#[cfg(unix)]
mod fake_engine;
#[allow(
    dead_code,
    reason = "shared fixture global API exposes helpers this suite does not use"
)]
mod global_api;
mod hermetic_search;
#[path = "python_runtime/environment.rs"]
mod python_runtime_environment;
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
use surface_corpus::corpus;

fn arguments(value: &Value) -> TestResult<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// The result rows a paginated answer pages: search hits or symbol hits. None for a tool
/// whose result carries no pagination.
fn paged_rows<'result>(name: &str, structured: &'result Value) -> Option<&'result [Value]> {
    if name != "search" {
        return None;
    }
    match structured["answer"]["result"].as_str() {
        Some("bindings") => structured["answer"]["bindings"]
            .as_array()
            .map(Vec::as_slice),
        Some("graph") => structured["answer"]["triples"]
            .as_array()
            .map(Vec::as_slice),
        _ => None,
    }
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

/// Proves one tool result carries source-unit resolvers the request reaches,
/// that every `search` hit names exactly one location, and that a read result
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
/// A `get_symbol` request with a released owner, or a `search` request whose `scope`
/// reaches packages, carries the package
/// warnings and no other: the fixture's path dependency `helper` answers
/// `package_unavailable`, the standard library entry the fixture global API cannot
/// resolve answers `package_requirement_absent`, and the global API's own page warnings
/// arrive as `global_page_warning`.
fn assert_wire_hygiene(name: &str, request: &Value, structured: &Value) {
    let context = format!("{name} result");
    let reaches_dependencies = request_reaches_dependencies(name, request)
        .expect("a validated exact request carries a canonical symbol identity");
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
        for hit in structured["declarations"].as_array().into_iter().flatten() {
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
                    structured.get("history").is_none(),
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
        && let Some(rows) = structured["answer"]["bindings"].as_array()
    {
        for row in rows {
            if row.get("hit").is_some() {
                assert!(
                    !(row.get("path").is_some() && row.get("unit").is_some()),
                    "one retrieved location per binding: {row}"
                );
                if row.get("path").is_some() {
                    assert!(
                        workspace_client::query::literal(row, "path").is_ok(),
                        "literal source path: {row}"
                    );
                }
                if row.get("unit").is_some() {
                    assert!(
                        workspace_client::query::iri(row, "unit").is_ok(),
                        "source unit IRI: {row}"
                    );
                }
                for field in ["source", "score"] {
                    if row.get(field).is_some() {
                        assert!(
                            workspace_client::query::literal(row, field).is_ok(),
                            "typed retrieved field: {row}"
                        );
                    }
                }
            }
        }
    }
}

/// Exact retrieval reaches the released owner its canonical identity names.
fn request_reaches_dependencies(name: &str, request: &Value) -> Option<bool> {
    use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
    if name == "get_symbol" {
        let identity = SymbolIdentity::parse(request["id"].as_str()?).ok()?;
        return Some(matches!(
            identity.owner(),
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
        ));
    }
    if name != "search" {
        return Some(false);
    }
    let configuration = rift_protocol::query::QueryConfiguration::default();
    let limits = rift_search::sparql::QueryLimits {
        query_bytes_max: usize::try_from(configuration.statement_size.bytes()).ok()?,
        algebra_nodes_max: usize::try_from(configuration.algebra_nodes).ok()?,
        quads_max: usize::try_from(configuration.statements).ok()?,
        rdf_bytes_max: usize::try_from(configuration.fact_size.bytes()).ok()?,
        result_items_max: usize::try_from(configuration.results).ok()?,
        result_bytes_max: usize::try_from(configuration.result_size.bytes()).ok()?,
    };
    let query =
        rift_search::sparql::PreparedQuery::parse(request["query"].as_str()?, limits).ok()?;
    Some(query.retrievals().iter().any(|producer| {
        serde_json::to_value(&producer.scope)
            .is_ok_and(|scope| matches!(scope.as_str(), Some("global" | "all")))
    }))
}

#[test]
fn exact_request_owner_controls_source_resolvers_and_refuses_malformed_identity() -> TestResult {
    use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
    let package = global_api::collected_identity(global_api::SymbolFixture::Valid, "beacon");
    let runtime = global_api::named_identity(&global_api::NAMED_POSITIONS[0]);
    let local = SymbolIdentity::new(
        SymbolOwner::Local,
        rift_protocol::read::Language::from_identity_segment("rust")?,
        vec!["beacon".to_owned()],
    )?
    .wire_identity();
    for (identity, reaches_dependencies) in [(package, true), (runtime, true), (local, false)] {
        assert_eq!(
            request_reaches_dependencies("get_symbol", &json!({"id": identity})),
            Some(reaches_dependencies)
        );
    }
    let package_source = json!({"unit": global_api::COLLECTED_UNIT});
    assert_source_unit_ids_use_served_resolvers(&package_source, "released source", true);
    assert!(
        std::panic::catch_unwind(|| {
            assert_source_unit_ids_use_served_resolvers(&package_source, "project source", false);
        })
        .is_err()
    );
    for request in [json!({"id": "beacon"}), json!({})] {
        assert_eq!(request_reaches_dependencies("get_symbol", &request), None);
        assert!(
            std::panic::catch_unwind(|| assert_wire_hygiene("get_symbol", &request, &json!({})))
                .is_err()
        );
    }
    assert_eq!(
        request_reaches_dependencies(
            "search",
            &json!({"query": "SELECT ?hit { SERVICE :search { ?hit :query \"beacon\"; :scope \"global\"; :index ?index } }"})
        ),
        Some(true)
    );
    assert_eq!(
        request_reaches_dependencies("nodes", &json!({})),
        Some(false)
    );
    Ok(())
}

/// A search hit in a file is addressed by exactly one of `path` and `unit`, and a file hit
/// carries `unit` only for a package file, which a package scope alone reaches. A commit
/// hit carries neither.

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
/// reach: the project resolver always, and the Cargo resolver when the exact owner or
/// search scope reaches the packages the global API serves.
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
    served_fixture_with_source(None).await
}

async fn served_fixture_with_source(
    source: Option<&str>,
) -> TestResult<(
    surface_corpus::SurfaceFixture,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    served_fixture_with_configuration(source, None).await
}

async fn served_fixture_with_configuration(
    source: Option<&str>,
    search: Option<&rift_protocol::configuration::SearchConfiguration>,
) -> TestResult<(
    surface_corpus::SurfaceFixture,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    served_fixture_with_setup(source, search, false, false).await
}

async fn served_fixture_with_setup(
    source: Option<&str>,
    search: Option<&rift_protocol::configuration::SearchConfiguration>,
    canonical_change: bool,
    complete_selection: bool,
) -> TestResult<(
    surface_corpus::SurfaceFixture,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let fixture = if complete_selection {
        let fixture = surface_corpus::SurfaceFixture::start().await?;
        let path = fixture.root().join("lib.rs");
        let original = std::fs::read_to_string(&path)?;
        assert!(fixture.root().join("change_witness.rs").is_file());
        std::fs::write(path, format!("{original}pub mod change_witness;\n"))?;
        rift_history::fixture::commit_all(fixture.root(), "declare the original change module");
        fixture
    } else {
        surface_corpus::SurfaceFixture::start().await?
    };
    if let Some(search) = search {
        let path = fixture.root().join("rift.toml");
        let written = std::fs::read_to_string(&path)?;
        let table = format!(
            "[search]\nsymbol_alternatives_work = {}\nsymbol_alternatives_lowercase_work = {}\n\n[search.vector]",
            search.symbol_alternatives_work, search.symbol_alternatives_lowercase_work
        );
        assert!(written.contains("[search.vector]"));
        assert!(!written.contains("[search]\n"));
        std::fs::write(path, written.replacen("[search.vector]", &table, 1))?;
    }
    if let Some(source) = source {
        std::fs::write(fixture.root().join("alternatives.rs"), source)?;
        if complete_selection {
            let path = fixture.root().join("lib.rs");
            let original = std::fs::read_to_string(&path)?;
            std::fs::write(path, format!("{original}pub mod alternatives;\n"))?;
        }
    }
    if canonical_change {
        let path = fixture.root().join("lib.rs");
        let original = std::fs::read_to_string(&path)?;
        std::fs::write(
            path,
            format!("{original}#[path=\"change_witness.rs\"] pub mod changed;\n"),
        )?;
        rift_history::fixture::commit_all(fixture.root(), "admit canonical change witness");
    }
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

async fn served_canonical_miss_fixture(
    source: Option<&str>,
    search: Option<&rift_protocol::configuration::SearchConfiguration>,
) -> TestResult<(
    surface_corpus::SurfaceFixture,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    served_fixture_with_setup(source, search, false, true).await
}

/// Builds an absent exact request from an observed owner, language and logical path.
async fn absent_request(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    language: &str,
    revision: Option<&str>,
) -> TestResult<Value> {
    use rift_protocol::identity::SymbolIdentity;
    let anchor = workspace_client::captured_seed(client, "beacon_one", "lib.rs").await?;
    let anchor = SymbolIdentity::parse(&anchor)?;
    let mut path = anchor.qualified_path().to_vec();
    name.clone_into(path.last_mut().ok_or("observed terminal name")?);
    let missing = SymbolIdentity::new(
        anchor.owner().clone(),
        rift_protocol::read::Language::from_identity_segment(language)?,
        path,
    )?;
    let mut request = json!({"id":missing.wire_identity(),"include":[]});
    if let Some(revision) = revision {
        request["rev"] = json!(revision);
    }
    Ok(request)
}

/// A miss keeps the empty answer and proposes declarations the caller can retrieve (#590).
#[tokio::test]
async fn get_symbol_miss_proposes_three_project_alternatives() -> TestResult {
    let (_fixture, client, server_task) = served_canonical_miss_fixture(None, None).await?;
    let tools = client.list_all_tools().await?;
    let tool = tools
        .iter()
        .find(|tool| tool.name == "get_symbol")
        .ok_or("get_symbol absent")?;
    let schema = Value::Object(
        tool.output_schema
            .as_deref()
            .ok_or("output schema absent")?
            .clone(),
    );
    let validator = jsonschema::validator_for(&schema)?;
    assert_symbol_alternative_bounds(tool, &schema);
    let request = absent_request(&client, "beacon_ane", "rust", None).await?;
    let result = client
        .call_tool(tools_call_request("get_symbol", &request)?)
        .await?;
    assert!(
        result.is_error.unwrap_or(false),
        "a miss must report a tool error with its typed answer"
    );
    let answer = result
        .structured_content
        .ok_or("structured answer absent")?;
    eprintln!(
        "exact miss context: {}",
        json!({"outcome":answer["outcome"], "reason":answer["reason"], "view":answer["view"], "warnings":answer["warnings"]})
    );
    assert_validates(&validator, &answer, "get_symbol miss");
    assert_eq!(answer["outcome"], "missing");
    let warning = &answer["symbol_not_found"];
    assert_eq!(warning["id"], request["id"]);
    assert!(warning.get("detail").is_none());
    let mut expected = Vec::new();
    for name in ["beacon_one", "beacon_two", "beacon_three"] {
        expected.push(workspace_client::captured_seed(&client, name, "lib.rs").await?);
    }
    let actual: Vec<_> = warning["alternatives"]
        .as_array()
        .ok_or("alternatives")?
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(actual, expected);
    for alternative in warning["alternatives"].as_array().ok_or("alternatives")? {
        assert_eq!(
            alternative["match_context"],
            json!(["owner", "language", "name"])
        );
    }
    let text = result
        .content
        .iter()
        .filter_map(|content| content.as_text().map(|content| content.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains(request["id"].as_str().ok_or("requested id")?),
        "the missing name must reach the caller: {text}"
    );
    assert!(
        text.contains("beacon_one"),
        "the closest declaration must reach the caller: {text}"
    );
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

fn assert_symbol_alternative_bounds(tool: &rmcp::model::Tool, schema: &Value) {
    let miss = &schema["$defs"]["SymbolNotFound"];
    assert_eq!(
        miss["properties"]["alternatives"]["maxItems"],
        json!(rift_protocol::read::SYMBOL_ALTERNATIVES_MAX)
    );
    assert_eq!(
        schema["$defs"]["SymbolAlternative"]["properties"]["name"]["maxLength"],
        json!(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX)
    );
    assert!(tool.input_schema["properties"].get("id").is_some());
    assert_eq!(miss["properties"]["detail"]["maxLength"], json!(4096));
}

#[tokio::test]
async fn get_symbol_configured_work_preserves_named_miss_and_exact_hits() -> TestResult {
    let search = rift_protocol::configuration::SearchConfiguration {
        symbol_alternatives_work: 1,
        symbol_alternatives_lowercase_work: 24,
        ..Default::default()
    };
    let (_fixture, client, server_task) =
        served_canonical_miss_fixture(None, Some(&search)).await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (_, output) = validators
        .get("get_symbol")
        .ok_or("get_symbol advertised")?;
    let expected = rift_error::errors::server::read_symbol_alternatives_unavailable()
        .work(1_u64)
        .lowercase_work(24_u64)
        .remaining(1_u64)
        .required(48_u128)
        .error()
        .to_string();
    for request in [
        absent_request(&client, "İ", "rust", None).await?,
        absent_request(&client, "İ", "python", None).await?,
        absent_request(&client, "İ", "rust", Some("baseline")).await?,
    ] {
        let result = client
            .call_tool(tools_call_request("get_symbol", &request)?)
            .await?;
        assert!(result.is_error.unwrap_or(false));
        let answer = result
            .structured_content
            .ok_or("structured answer absent")?;
        eprintln!(
            "exact miss context: {}",
            json!({"outcome":answer["outcome"], "reason":answer["reason"], "view":answer["view"], "warnings":answer["warnings"]})
        );
        assert_validates(output, &answer, "configured work result");
        let warning = &answer["symbol_not_found"];
        assert_eq!(
            warning,
            &json!({"id":request["id"],"alternatives":[],"detail":expected})
        );
        assert_eq!(answer["outcome"], "missing");
        let text = result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|content| content.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&expected), "{text}");
    }
    let exact_id = workspace_client::captured_seed(&client, "beacon_one", "lib.rs").await?;
    let result = client
        .call_tool(tools_call_request(
            "get_symbol",
            &json!({"id":exact_id,"include":[]}),
        )?)
        .await?;
    assert!(!result.is_error.unwrap_or(false));
    let answer = result.structured_content.ok_or("exact answer absent")?;
    assert_eq!(answer["outcome"], "found");
    assert_eq!(answer["symbol"]["id"], exact_id);
    assert!(!answer["warnings"].as_array().is_some_and(|warnings| {
        warnings
            .iter()
            .any(|warning| warning["code"] == "symbol_not_found")
    }));
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn get_symbol_miss_reports_unavailable_alternatives_at_the_work_bound() -> TestResult {
    use std::fmt::Write as _;

    let source = (0..160).fold(String::new(), |mut source, index| {
        writeln!(source, "pub fn beacon_load_configuration_{index:07}() {{}}")
            .expect("a string write succeeds");
        source
    });
    let (_fixture, client, server_task) =
        served_canonical_miss_fixture(Some(&source), None).await?;
    let tools = client.list_all_tools().await?;
    let schema = tools
        .iter()
        .find(|tool| tool.name == "get_symbol")
        .and_then(|tool| tool.output_schema.as_deref())
        .ok_or("get_symbol output schema absent")?;
    let validator = jsonschema::validator_for(&Value::Object(schema.clone()))?;
    let name = "x".repeat(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX);
    for request in [
        absent_request(&client, &name, "rust", None).await?,
        absent_request(&client, &name, "python", None).await?,
        absent_request(&client, &name, "rust", Some("baseline")).await?,
    ] {
        let result = client
            .call_tool(tools_call_request("get_symbol", &request)?)
            .await?;
        assert!(result.is_error.unwrap_or(false));
        let answer = result
            .structured_content
            .ok_or("structured answer absent")?;
        eprintln!(
            "exact miss context: {}",
            json!({"outcome":answer["outcome"], "reason":answer["reason"], "view":answer["view"], "warnings":answer["warnings"]})
        );
        assert_validates(&validator, &answer, "get_symbol work bound");
        let warning = &answer["symbol_not_found"];
        assert_eq!(warning["id"], request["id"]);
        let requested = rift_protocol::identity::SymbolIdentity::parse(
            request["id"].as_str().ok_or("requested id")?,
        )?;
        let exhausted = request.get("rev").is_none() && requested.language().name == "rust";
        assert_eq!(warning.get("detail").is_some(), exhausted, "{answer:#}");
        if exhausted {
            assert_eq!(warning["alternatives"], json!([]));
            let text = result
                .content
                .iter()
                .filter_map(|content| content.as_text().map(|content| content.text.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains(request["id"].as_str().ok_or("requested id")?));
            assert!(text.contains("work bound"), "{text}");
            assert!(text.contains("closest"), "{text}");
        } else {
            assert!(
                !warning["alternatives"]
                    .as_array()
                    .ok_or("alternatives absent")?
                    .is_empty()
            );
        }
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

#[tokio::test]
async fn get_symbol_miss_uses_requested_revision_and_remote_matches_clear_the_warning() -> TestResult
{
    let (_fixture, client, server_task) = served_canonical_miss_fixture(None, None).await?;
    for (revision, expected) in [(None, true), (Some("baseline"), false)] {
        let request = absent_request(&client, "change_witnesz", "rust", revision).await?;
        let result = client
            .call_tool(tools_call_request("get_symbol", &request)?)
            .await?;
        let answer = result
            .structured_content
            .ok_or("structured answer absent")?;
        eprintln!(
            "exact miss context: {}",
            json!({"outcome":answer["outcome"], "reason":answer["reason"], "view":answer["view"], "warnings":answer["warnings"]})
        );
        let alternatives = answer["symbol_not_found"]["alternatives"]
            .as_array()
            .ok_or("alternatives absent")?;
        let carries_current = alternatives.iter().any(|identity| {
            identity["id"]
                .as_str()
                .is_some_and(|identity| identity.ends_with("/change_witness"))
        });
        assert_eq!(
            carries_current, expected,
            "revision controls which declarations can be proposed: {answer:#}"
        );
    }
    let remote = surface_corpus::prepared_corpus(&client)
        .await?
        .into_iter()
        .find(|(tool, request)| {
            *tool == "get_symbol"
                && request["id"]
                    .as_str()
                    .is_some_and(|id| id.contains("/cargo/"))
        })
        .ok_or("observed remote exact request")?
        .1;
    let answer = client
        .call_tool(tools_call_request("get_symbol", &remote)?)
        .await?
        .structured_content
        .ok_or("remote answer")?;
    assert_eq!(answer["outcome"], "found");
    assert_eq!(answer["symbol"]["id"], remote["id"]);
    assert!(answer.get("symbol_not_found").is_none());
    for request in [
        json!({"query":"helper_beacon","scope":"global","include":[]}),
        json!({"query":"helper_beacon","scope":"all","include":[]}),
        json!({"query":"helper_beacon","scope":"all","include":[],"limit":1,"page_index":100}),
    ] {
        let request = serde_json::to_value(workspace_client::query::retrieval(
            &serde_json::from_value(request.clone())?,
        )?)?;
        let answer = client
            .call_tool(tools_call_request("search", &request)?)
            .await?
            .structured_content
            .ok_or("remote discovery")?;
        assert!(
            !answer["warnings"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|warning| warning["code"] == "symbol_not_found"),
            "matched remote discovery clears the miss, including an empty later page: {answer:#}"
        );
    }
    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// Public result branches and retained serving facts observed through the live SDK.
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
    query_forms: BTreeSet<String>,
    query_terms: BTreeSet<String>,
}

impl CorpusArms {
    fn observe(&mut self, structured: &Value) {
        self.dependency_units += dependency_unit_count(&structured["declarations"]);
        self.search_dependency_units += dependency_unit_count(&structured["answer"]["bindings"]);
        for warning in structured["warnings"].as_array().into_iter().flatten() {
            if warning["code"] == "package_unavailable" {
                self.unavailable_entries += 1;
            }
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
        if let Some(form) = structured["answer"]["result"].as_str() {
            self.query_forms.insert(form.to_owned());
        }
        let mut pending = vec![&structured["answer"]];
        while let Some(value) = pending.pop() {
            match value {
                Value::Object(object) => {
                    if let Some(kind) = object.get("type").and_then(Value::as_str) {
                        self.query_terms.insert(kind.to_owned());
                    }
                    pending.extend(object.values());
                }
                Value::Array(values) => pending.extend(values),
                _ => {}
            }
        }
    }

    fn assert_proven(&self) {
        assert!(
            self.multi_page_results > 0 && self.past_end_pages > 0,
            "retained multi-page and past-end results: {}/{}",
            self.multi_page_results,
            self.past_end_pages
        );
        assert!(
            self.dependency_units > 0,
            "exact package declarations retain source units"
        );
        assert!(
            self.search_dependency_units > 0,
            "query bindings retain package source units"
        );
        assert!(
            self.unavailable_entries > 0,
            "path dependency remains explicitly unavailable"
        );
        assert!(
            self.replaced_entries > 0 && self.added_entries > 0,
            "package replacement and addition remain visible"
        );
        assert!(
            self.literal_at_identities > 0 && self.escaped_identities > 0,
            "literal @ and percent-encoded node identities remain valid"
        );
        for form in ["bindings", "boolean", "graph"] {
            assert!(
                self.query_forms.contains(form),
                "missing live result form {form}: {:?}",
                self.query_forms
            );
        }
        for kind in ["iri", "blank_node", "literal", "language_literal"] {
            assert!(
                self.query_terms.contains(kind),
                "missing live RDF term {kind}: {:?}",
                self.query_terms
            );
        }
    }
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
    let warmup =
        workspace_client::query::retrieval(&serde_json::from_value(json!({"query":"beacon"}))?)?;
    workspace_client::search_after_population(&client, &warmup).await?;
    let tools = client.list_all_tools().await?;
    let advertised: BTreeSet<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let covered: BTreeSet<&str> = corpus().iter().map(|(name, _)| *name).collect();
    assert_eq!(
        advertised, covered,
        "every advertised tool has a corpus request"
    );
    let validators = tool_validators(&tools)?;
    let mut arms = CorpusArms::default();
    for (name, request) in surface_corpus::prepared_corpus(&client).await? {
        let (input, output) = validators.get(name).ok_or("advertised corpus tool")?;
        let mut request = request;
        let mut followed = 0usize;
        let mut expected_page = 0u64;
        let mut past_end = false;
        let mut original = None;
        loop {
            assert!(
                followed <= FOLLOWED_PAGES_MAX,
                "bounded retained page walk: {name}"
            );
            assert_validates(input, &request, &format!("{name} request"));
            let result =
                call_tool_retrying_acceptance(&client, tools_call_request(name, &request)?).await?;
            text_complete::assert_text_states(name, &result);
            let structured = result
                .structured_content
                .ok_or("structured corpus result")?;
            assert_validates(output, &structured, &format!("{name} result"));
            assert_wire_hygiene(name, &request, &structured);
            arms.observe(&structured);
            let Some(page) = structured.get("pagination") else {
                break;
            };
            let page_index = page["page_index"].as_u64().ok_or("page index")?;
            let total_pages = page["total_pages"].as_u64().ok_or("page count")?;
            assert_eq!(page_index, expected_page, "original retained page index");
            if total_pages > 1 {
                arms.multi_page_results += 1;
                if original.is_none() {
                    let answer: rift_protocol::query::QueryAnswer =
                        serde_json::from_value(structured["answer"].clone())?;
                    let view: rift_protocol::symbol_read::CapturedViewId =
                        serde_json::from_value(structured["view"]["id"].clone())?;
                    original = Some((answer, view, total_pages));
                }
            }
            if page_index >= total_pages {
                assert!(
                    paged_rows(name, &structured)
                        .ok_or("query rows")?
                        .is_empty(),
                    "past-end retained page is empty: {structured}"
                );
                arms.past_end_pages += 1;
                break;
            }
            if let Some(query) = page["next_query"].as_str() {
                followed += 1;
                expected_page = page_index + 1;
                request = json!({"query":query, "language":"sparql"});
                continue;
            }
            if !past_end && let Some((answer, view, pages)) = &original {
                followed += 1;
                expected_page = *pages;
                past_end = true;
                request = json!({"query":rift_search::sparql::continuation_query(answer, view, *pages)?, "language":"sparql"});
                continue;
            }
            break;
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

/// Removed selection arguments refuse in the schema; malformed SPARQL reaches the parser.
fn traversal_refusal_corpus(target: &str, seed: &str) -> Vec<(Value, InputSchema, ErrorCode)> {
    vec![
        (
            json!({"query":"ASK {}", "rev":"main", "traversal":{"seed":target}}),
            InputSchema::Refuses,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"query":"ASK {}", "traversal":{"seed":seed,"direction":"outgoing"}}),
            InputSchema::Refuses,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"query":"ASK {}", "scope":"global"}),
            InputSchema::Refuses,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"query":"ASK {}", "change":{"base":"baseline","head":"HEAD"}}),
            InputSchema::Refuses,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"query":"SELECT {"}),
            InputSchema::Admits,
            ErrorCode::InvalidRequest,
        ),
        (
            json!({"query":"ASK {}", "language":"cypher"}),
            InputSchema::Refuses,
            ErrorCode::InvalidRequest,
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
    let target = workspace_client::captured_seed(&client, "callee", "traversal_callee.py").await?;
    let seed =
        workspace_client::captured_seed(&client, "calls_callee", "traversal_caller.py").await?;
    for (request, verdict, code) in traversal_refusal_corpus(&target, &seed) {
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
        &[
            (
                "Cargo.toml",
                "[package]\nname=\"surface_fixture\"\nversion=\"1.0.0\"\n[lib]\npath=\"lib.rs\"\n",
            ),
            ("lib.rs", "pub struct Beacon;\n"),
        ],
        Some(configuration),
    )
    .await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, output_validator) = validators
        .get("search")
        .ok_or("search must be advertised")?;
    workspace_client::call_retrying_acceptance(
        &client,
        workspace_client::tool_request(
            "search",
            &json!({ "query": "ASK { ?symbol a :Symbol; :name 'Beacon' }" }),
        ),
    )
    .await?;
    let beacon = workspace_client::captured_seed(&client, "Beacon", "lib.rs").await?;
    let unprepared = serde_json::to_value(workspace_client::query::path(&beacon, ":calls")?)?;
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
    let caller = workspace_client::captured_seed(&client, "caller", "lib.rs").await?;
    let unconfirmed = serde_json::to_value(workspace_client::query::path(&caller, ":calls")?)?;
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
    let (_directory, client, _server_task) = workspace_client::served_prepared_workspace(
        &[("pyproject.toml", "[build-system]\nrequires=[\"setuptools==80.9.0\"]\nbuild-backend=\"setuptools.build_meta\"\n[tool.setuptools]\npy-modules=[\"caller\"]\n"), (
            "caller.py",
            "import json\n\n\ndef caller() -> int:\n    return len([1])\n",
        )],
        Some(configuration),
        python_runtime_environment::prepare,
    )
    .await?;
    let validators = tool_validators(&client.list_all_tools().await?)?;
    let (input_validator, output_validator) = validators
        .get("search")
        .ok_or("search must be advertised")?;
    let caller = workspace_client::captured_seed(&client, "caller", "caller.py").await?;
    let walk = json!({"query":format!("SELECT ?symbol ?name ?unit {{ <{caller}> :calls ?symbol . ?symbol :name ?name; :declarations/:source/:unit ?unit }} ORDER BY ?name")});
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
    let units = workspace_client::query::rows(&structured)?
        .iter()
        .map(|row| workspace_client::query::iri(row, "unit"))
        .collect::<TestResult<Vec<_>>>()?;
    assert_eq!(
        units,
        [global_api::named_unit(&global_api::NAMED_POSITIONS[0])],
        "{structured:#}"
    );
    assert_eq!(
        warning_count(&structured, "callees_dropped"),
        0,
        "complete package callee: {structured:#}"
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
    let id = workspace_client::captured_seed(&client, "beacon_one", "lib.rs").await?;
    let requests = [
        ("get_symbol", json!({ "id": id, "packages": packages })),
        (
            "get_symbol",
            json!({ "id": id, "scope": "all", "rev": "main", "packages": packages }),
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
    for (name, producer) in requests {
        let request = if name == "search" {
            serde_json::to_value(workspace_client::query::retrieval(
                &serde_json::from_value(producer.clone())?,
            )?)?
        } else {
            producer.clone()
        };
        let (input_validator, _) = validators
            .get(name)
            .ok_or_else(|| format!("{name} is advertised"))?;
        if name == "get_symbol" {
            assert!(
                !input_validator.is_valid(&request),
                "removed exact selection fields refuse in schema"
            );
        } else {
            assert_validates(input_validator, &request, &format!("{name} request"));
        }
        let failure = workspace_client::failed_call(
            client.call_tool(tools_call_request(name, &request)?).await,
        )?;
        assert_eq!(
            failure.code,
            ErrorCode::InvalidRequest,
            "{request}: {}",
            failure.text
        );
        if name == "get_symbol" {
            assert!(
                failure.message.contains("tool get_symbol")
                    && failure.message.contains("accepted declaration_cursor")
                    && !failure.message.contains("accepted packages"),
                "the schema refusal lists exact retrieval fields: {request}: {}",
                failure.message
            );
        } else {
            let field = if producer.get("rev").is_some() {
                "field scope"
            } else {
                "field packages"
            };
            assert!(
                failure.message.contains(field),
                "the refusal names the field: {request}: {}",
                failure.message
            );
        }
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
    for (producer, code) in refused {
        let arguments = serde_json::to_value(workspace_client::query::retrieval(
            &serde_json::from_value(producer)?,
        )?)?;
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
    let request = json!({"query": "SELECT ?message ?message_truncated ?author_name ?email ?path ?paths_truncated { SERVICE :search { ?hit :query 'witness'; :target 'commit'; :hit ?entry . ?entry :commit ?commit . ?commit :message ?message; :message_truncated ?message_truncated; :author ?author; :paths ?paths; :paths_truncated ?paths_truncated . ?author :name ?author_name; :email ?email . ?paths :index 0; :value ?path } }"});
    assert_validates(input_validator, &request, "commit search request");

    let deadline = tokio::time::Instant::now() + COMMIT_FILL_WAIT_MAX;
    let structured = loop {
        let result =
            call_tool_retrying_acceptance(&client, tools_call_request("search", &request)?).await?;
        text_complete::assert_text_states("search", &result);
        let structured = result
            .structured_content
            .ok_or("search must return structured content")?;
        if structured["answer"]["bindings"]
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
    let rows = workspace_client::query::rows(&structured)?;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    for (field, expected) in [
        ("message", "introduce the change witness\n"),
        ("message_truncated", "false"),
        ("author_name", "Rift Fixture"),
        ("email", "fixture@rift.invalid"),
        ("path", "change_witness.rs"),
        ("paths_truncated", "false"),
    ] {
        assert_eq!(workspace_client::query::literal(row, field)?, expected);
    }

    client.cancel().await?;
    server_task.await?;
    Ok(())
}

/// A revision spelling past the advertised form - a reflog selector, or an ancestry suffix
/// followed by a path - refuses `invalid_request` naming the field that carried it.
#[tokio::test]
async fn a_revision_spelling_past_the_advertised_form_refuses_naming_the_field() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let id = workspace_client::captured_seed(&client, "beacon_one", "lib.rs").await?;
    let refused = [
        (
            "get_symbol",
            json!({ "id": id, "rev": "HEAD@{1}" }),
            "field rev",
        ),
        (
            "search",
            json!({ "change": { "base": "HEAD~1/lib.rs" } }),
            "field change.base",
        ),
    ];
    for (name, producer, field) in refused {
        let arguments = if name == "search" {
            serde_json::to_value(workspace_client::query::retrieval(
                &serde_json::from_value(producer)?,
            )?)?
        } else {
            producer
        };
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
        let arguments = serde_json::to_value(workspace_client::query::retrieval(
            &serde_json::from_value(arguments)?,
        )?)?;
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
