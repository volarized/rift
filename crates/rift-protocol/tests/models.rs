//! Behavior of the model logic this crate itself owns: documented wire
//! defaults, optional-field omission, and the cross-field schema
//! constraints. Serialization of the derived shapes is exercised by the MCP
//! server tests, not re-proven here.

use rift_protocol::read::{
    Diagnostic, DiagnosticReliability, GetSymbolParams, ResultOrder, SearchParams,
    SearchParamsTarget, SearchScope, SearchTraversal, Severity,
};
use schemars::schema_for;
use serde_json::json;

type TestResult = Result<(), serde_json::Error>;

#[test]
fn get_symbol_params_fill_documented_defaults() -> TestResult {
    let params: GetSymbolParams = serde_json::from_value(json!({ "name": "ReadService" }))?;
    assert!(params.include_body);
    assert!(!params.include_history);
    assert_eq!(params.limit, 5);
    assert_eq!(params.scope, SearchScope::All);
    Ok(())
}

#[test]
fn search_params_fill_documented_defaults() -> TestResult {
    let params: SearchParams = serde_json::from_value(json!({ "query": "ReadService" }))?;
    assert_eq!(params.target, SearchParamsTarget::All);
    assert_eq!(params.order, ResultOrder::Relevance);
    assert_eq!(params.scope, SearchScope::Project);
    Ok(())
}

#[test]
fn search_traversal_fills_documented_hop_and_node_bounds() -> TestResult {
    let traversal: SearchTraversal = serde_json::from_value(json!({
        "seed": "rift://symbol/rust/rift-server/ReadService",
        "intent": "find_tests"
    }))?;
    assert_eq!(traversal.max_hops, 1);
    assert_eq!(traversal.max_nodes, 25);
    assert_eq!(traversal.direction, None);
    assert_eq!(traversal.facets, None);
    Ok(())
}

#[test]
fn diagnostic_omits_absent_code_span_and_language() -> TestResult {
    let minimal = json!({
        "severity": "error",
        "message": "mismatched types",
        "related": [],
        "tags": [],
        "reliability": "reliable",
        "continuation": "unknown",
        "extensions": {}
    });
    let diagnostic: Diagnostic = serde_json::from_value(minimal.clone())?;
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.code, None);
    assert_eq!(diagnostic.span, None);
    assert_eq!(diagnostic.language, None);
    assert_eq!(diagnostic.reliability, DiagnosticReliability::Reliable);
    assert_eq!(
        serde_json::to_value(&diagnostic)?,
        minimal,
        "absent optional members must stay off the wire"
    );
    Ok(())
}

#[test]
fn schema_constraints_land_on_their_models() {
    let file = serde_json::to_value(schema_for!(rift_protocol::read::File)).expect("file schema");
    assert_eq!(
        file["allOf"][0]["then"]["properties"]["semantic"]["const"],
        json!(false)
    );

    let filter = serde_json::to_value(schema_for!(rift_protocol::read::RelationFilter))
        .expect("relation filter schema");
    assert_eq!(filter["anyOf"][0]["required"], json!(["kind"]));
    assert_eq!(filter["anyOf"][1]["required"], json!(["facet"]));

    let hit =
        serde_json::to_value(schema_for!(rift_protocol::read::SearchHit)).expect("hit schema");
    assert_eq!(
        hit["allOf"][0]["oneOf"][0]["required"],
        json!(["span", "line"])
    );
    assert_eq!(hit["allOf"][1]["then"]["required"], json!(["span", "line"]));

    let search = serde_json::to_value(schema_for!(SearchParams)).expect("search schema");
    assert_eq!(
        search["allOf"][1]["then"]["properties"]["scope"]["const"],
        json!("project")
    );
    let satisfiers: Vec<_> = search["anyOf"]
        .as_array()
        .expect("search anyOf")
        .iter()
        .map(|clause| clause["required"][0].as_str().expect("required field"))
        .collect();
    assert_eq!(satisfiers, ["query", "filter", "traversal"]);
}
