//! Conformance gate for the served MCP surface.
//!
//! Every advertised tool has request coverage in the corpus below, every
//! corpus request satisfies the tool's advertised input schema, and every
//! structured result validates against the tool's advertised output schema.
//! Both `next_cursor` arms are proven against the schema a client enforces:
//! the `null` arm from live results, the present arm by revalidating a page
//! with a cursor spliced in. The walk also follows any cursor a result
//! returns, so live pagination joins the gate as soon as a read mints one.
//! Every `ChangeResult` arm is proven the same way: applied (with and
//! without parser findings), and refused for a failed precondition, an
//! ambiguous target, and an unsupported file-level change — plus a live
//! witnessed `replace_node` that lands after the walk.

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

/// Most cursor pages one corpus request may walk before the gate fails.
const FOLLOWED_PAGES_MAX: usize = 16;

/// One request per advertised tool behavior worth proving: a single exact
/// hit, a paginated prefix listing, a body-free lookup, and node listings
/// at a declaration's first byte and inside its body.
fn corpus() -> Vec<(&'static str, Value)> {
    vec![
        ("get_symbol", json!({ "name": "beacon_one" })),
        ("get_symbol", json!({ "name": "beacon", "limit": 1 })),
        (
            "get_symbol",
            json!({ "name": "beacon", "include_body": false }),
        ),
        ("search", json!({ "query": "beacon" })),
        ("search", json!({ "query": "beacon", "limit": 1 })),
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
            "replace_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/dual",
                "body": "pub fn dual() -> u8 { 3 }"
            }),
        ),
        (
            "replace_symbol",
            json!({
                "symbol": "rift://symbol/rust/lib.rs/beacon_three",
                "body": "pub fn beacon_three( {"
            }),
        ),
    ]
}

fn arguments(value: &Value) -> TestResult<serde_json::Map<String, Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
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

/// Builds the shared fixture workspace and serves it to one client.
async fn served_fixture() -> TestResult<(
    tempfile::TempDir,
    rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tokio::task::JoinHandle<()>,
)> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("lib.rs"),
        "pub fn beacon_one() {}\npub fn beacon_two() {}\npub fn beacon_three() {}\n\
         #[cfg(unix)]\npub fn dual() {}\n#[cfg(windows)]\npub fn dual() {}\n",
    )?;
    let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())?;
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

#[tokio::test]
async fn every_tool_result_validates_against_served_output_schema() -> TestResult {
    let (_directory, client, server_task) = served_fixture().await?;
    let tools = client.list_all_tools().await?;

    let advertised: BTreeSet<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let covered: BTreeSet<&str> = corpus().iter().map(|(name, _)| *name).collect();
    assert_eq!(
        advertised, covered,
        "every advertised tool needs a conformance corpus entry, and every \
         corpus entry an advertised tool: extend `corpus` alongside the surface"
    );

    let validators = tool_validators(&tools)?;

    let mut null_cursor_pages = 0_usize;
    let mut present_cursor_pages = 0_usize;
    let mut applied_changes = 0_usize;
    let mut applied_with_findings = 0_usize;
    let mut refusal_reasons: BTreeSet<String> = BTreeSet::new();
    for (name, request) in corpus() {
        let (input_validator, output_validator) = validators
            .get(name)
            .ok_or_else(|| format!("corpus names unadvertised tool {name}"))?;
        let mut request = request;
        let mut followed_pages = 0_usize;
        loop {
            assert!(
                followed_pages <= FOLLOWED_PAGES_MAX,
                "cursor walk for {name} exceeded {FOLLOWED_PAGES_MAX} pages: \
                 the fixture is too large or pagination never terminates"
            );
            assert_validates(input_validator, &request, &format!("{name} request"));
            let result = client
                .call_tool(CallToolRequestParams::new(name).with_arguments(arguments(&request)?))
                .await?;
            let structured = result
                .structured_content
                .ok_or_else(|| format!("{name} must return structured content"))?;
            assert_validates(output_validator, &structured, &format!("{name} result"));
            match structured["status"].as_str() {
                Some("applied") => {
                    applied_changes += 1;
                    if structured["summary"]["diagnostics"]
                        .as_array()
                        .is_some_and(|findings| !findings.is_empty())
                    {
                        applied_with_findings += 1;
                    }
                }
                Some("refused") => {
                    if let Some(reason) = structured["reason"].as_str() {
                        refusal_reasons.insert(reason.to_owned());
                    }
                }
                _ => {}
            }
            if structured.get("next_cursor").is_some() {
                let mut continued = structured.clone();
                continued["next_cursor"] = json!("b3BhcXVl");
                assert_validates(
                    output_validator,
                    &continued,
                    &format!("{name} result with a present cursor"),
                );
                present_cursor_pages += 1;
            }
            match &structured["next_cursor"] {
                Value::String(cursor) => {
                    present_cursor_pages += 1;
                    followed_pages += 1;
                    request["cursor"] = json!(cursor);
                }
                Value::Null => {
                    null_cursor_pages += 1;
                    break;
                }
                other => panic!("{name} next_cursor must be a string or null, got {other}"),
            }
        }
    }
    assert!(
        null_cursor_pages > 0 && present_cursor_pages > 0,
        "the corpus must prove both next_cursor arms against the schema: \
         null_cursor_pages={null_cursor_pages}, present_cursor_pages={present_cursor_pages}"
    );
    assert!(
        applied_changes >= 3 && applied_with_findings >= 1,
        "the corpus must prove the applied arm with and without parser findings: \
         applied={applied_changes}, with_findings={applied_with_findings}"
    );
    for reason in ["unmet_precondition", "ambiguous_target", "unsupported"] {
        assert!(
            refusal_reasons.contains(reason),
            "the corpus must prove the {reason} refusal arm; proven: {refusal_reasons:?}"
        );
    }

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
