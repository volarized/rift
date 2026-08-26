//! Behavior of the model logic this crate itself owns: documented wire
//! defaults, optional-field omission, and the cross-field schema
//! constraints. Serialization of the derived shapes is exercised by the MCP
//! server tests, not re-proven here.

use rift_protocol::read::{
    Diagnostic, DiagnosticReliability, GetSymbolParams, ResultOrder, SearchParams,
    SearchParamsTarget, Severity,
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
    Ok(())
}

#[test]
fn search_params_fill_documented_defaults() -> TestResult {
    let params: SearchParams = serde_json::from_value(json!({ "query": "ReadService" }))?;
    assert_eq!(params.target, SearchParamsTarget::All);
    assert_eq!(params.order, ResultOrder::Relevance);
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

    let hit =
        serde_json::to_value(schema_for!(rift_protocol::read::SearchHit)).expect("hit schema");
    assert_eq!(
        hit["allOf"][0]["oneOf"][0]["required"],
        json!(["span", "line"])
    );
    assert_eq!(hit["allOf"][1]["then"]["required"], json!(["span", "line"]));
}
