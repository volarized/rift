//! Behavior of the model logic this crate itself owns: documented wire
//! defaults, required-but-nullable fields, and the cross-field schema
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
fn diagnostic_accepts_null_code_and_span_but_not_their_absence() -> TestResult {
    let with_nulls = json!({
        "severity": "error",
        "code": null,
        "message": "mismatched types",
        "span": null,
        "related": [],
        "tags": [],
        "reliability": "reliable",
        "continuation": "unknown",
        "extensions": {},
        "language": null
    });
    let diagnostic: Diagnostic = serde_json::from_value(with_nulls.clone())?;
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.code, None);
    assert_eq!(diagnostic.reliability, DiagnosticReliability::Reliable);

    let mut without_code = with_nulls;
    without_code.as_object_mut().expect("object").remove("code");
    let refused = serde_json::from_value::<Diagnostic>(without_code)
        .expect_err("a diagnostic without a code field must be refused");
    assert!(refused.to_string().contains("code"));
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
