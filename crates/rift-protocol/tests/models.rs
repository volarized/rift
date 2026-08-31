//! Behavior of the model logic this crate itself owns: documented wire
//! defaults, optional-field omission, and the cross-field schema
//! constraints. Serialization of the derived shapes is exercised by the MCP
//! server tests, not re-proven here.

use rift_protocol::change::ChangeResult;
use rift_protocol::error::ErrorData;
use rift_protocol::read::{
    Diagnostic, DiagnosticReliability, GetSymbolParams, GetSymbolResult, NodesResult, ResultOrder,
    SearchParams, SearchParamsTarget, Severity,
};
use rift_protocol::search::SearchResult;
use schemars::schema_for;
use serde_json::{Value, json};

type TestResult = Result<(), serde_json::Error>;

/// Recursively asserts `value` carries no `null` member and no empty array or object: the
/// D1 wire contract states absence, never an explicit empty collection or a null.
fn assert_no_null_or_empty(value: &Value, context: &str) {
    match value {
        Value::Null => panic!("{context} must carry no null member: {value:#}"),
        Value::Array(items) => {
            assert!(
                !items.is_empty(),
                "{context} must carry no empty array: {value:#}"
            );
            for (index, item) in items.iter().enumerate() {
                assert_no_null_or_empty(item, &format!("{context}[{index}]"));
            }
        }
        Value::Object(members) => {
            assert!(
                !members.is_empty(),
                "{context} must carry no empty object: {value:#}"
            );
            for (key, item) in members {
                assert_no_null_or_empty(item, &format!("{context}.{key}"));
            }
        }
        Value::String(_) | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Every authored example on `schema` walks clean under [`assert_no_null_or_empty`].
fn assert_examples_carry_no_null_or_empty(schema: &Value, type_name: &str) {
    let examples = schema["examples"]
        .as_array()
        .unwrap_or_else(|| panic!("{type_name} must carry authored examples"));
    assert!(
        !examples.is_empty(),
        "{type_name} must carry at least one authored example"
    );
    for (index, example) in examples.iter().enumerate() {
        assert_no_null_or_empty(example, &format!("{type_name} example {index}"));
    }
}

/// The D1 conformance gate: every response model's authored wire example carries no
/// `null` member and no empty array or object. A field that can legitimately be absent
/// is absent in its example, never present with an empty value.
#[test]
fn response_model_examples_carry_no_null_or_empty_collection() {
    let get_symbol_result =
        serde_json::to_value(schema_for!(GetSymbolResult)).expect("get_symbol_result schema");
    assert_examples_carry_no_null_or_empty(&get_symbol_result, "GetSymbolResult");

    let nodes_result = serde_json::to_value(schema_for!(NodesResult)).expect("nodes_result schema");
    assert_examples_carry_no_null_or_empty(&nodes_result, "NodesResult");

    let search_result =
        serde_json::to_value(schema_for!(SearchResult)).expect("search_result schema");
    assert_examples_carry_no_null_or_empty(&search_result, "SearchResult");

    let change_result =
        serde_json::to_value(schema_for!(ChangeResult)).expect("change_result schema");
    assert_examples_carry_no_null_or_empty(&change_result, "ChangeResult");
}

/// A `GetSymbolResult`, `NodesResult`, and `SearchResult` each deserialize from a minimal
/// JSON that omits `warnings` entirely, filling the field with its empty default -
/// proving the serde side of the schema's `default: []` for a reader who never sends it.
#[test]
fn paginated_read_results_deserialize_with_warnings_absent() -> TestResult {
    let get_symbol: GetSymbolResult = serde_json::from_value(json!({
        "hits": [],
        "pagination": { "page_index": 0, "total_pages": 0 }
    }))?;
    assert!(get_symbol.warnings.is_empty());

    let nodes: NodesResult = serde_json::from_value(json!({
        "nodes": [],
        "source": []
    }))?;
    assert!(nodes.warnings.is_empty());

    let search: SearchResult = serde_json::from_value(json!({
        "results": [],
        "pagination": { "page_index": 0, "total_pages": 0 }
    }))?;
    assert!(search.warnings.is_empty());
    Ok(())
}

/// A `refused` `ChangeResult` deserializes with `diagnostics` absent, filling it with the
/// empty default; a re-serialized empty-diagnostics refusal omits the member.
#[test]
fn change_result_refused_deserializes_with_diagnostics_absent() -> TestResult {
    let refused: ChangeResult = serde_json::from_value(json!({
        "status": "refused",
        "reason": "unsupported",
        "preconditions": []
    }))?;
    let ChangeResult::Refused { diagnostics, .. } = &refused else {
        panic!("expected a refused ChangeResult, got {refused:?}");
    };
    assert!(diagnostics.is_empty());
    assert_eq!(
        serde_json::to_value(&refused)?,
        json!({
            "status": "refused",
            "reason": "unsupported",
            "preconditions": []
        }),
        "empty diagnostics must stay off the wire"
    );
    Ok(())
}

/// `ErrorData` deserializes with `diagnostics` and `causes` both absent, filling them with
/// their empty defaults.
#[test]
fn error_data_deserializes_with_diagnostics_and_causes_absent() -> TestResult {
    let data: ErrorData = serde_json::from_value(json!({
        "code": "internal_error",
        "message": "invariant violated",
        "retry": "never",
        "phase": "read"
    }))?;
    assert!(data.diagnostics.is_empty());
    assert!(data.causes.is_empty());
    Ok(())
}

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
        "reliability": "reliable",
        "continuation": "unknown"
    });
    let diagnostic: Diagnostic = serde_json::from_value(minimal.clone())?;
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.code, None);
    assert_eq!(diagnostic.span, None);
    assert_eq!(diagnostic.language, None);
    assert_eq!(diagnostic.reliability, DiagnosticReliability::Reliable);
    assert!(diagnostic.related.is_empty());
    assert!(diagnostic.tags.is_empty());
    assert!(diagnostic.extensions.is_empty());
    assert_eq!(
        serde_json::to_value(&diagnostic)?,
        minimal,
        "absent optional members and empty collections must stay off the wire"
    );
    Ok(())
}

#[test]
fn schema_constraints_land_on_their_models() {
    let get_symbol_hit =
        serde_json::to_value(schema_for!(rift_protocol::read::GetSymbolHit)).expect("hit schema");
    assert_eq!(
        get_symbol_hit["allOf"][0]["oneOf"],
        json!([{ "required": ["path"] }, { "required": ["unit"] }])
    );

    let hit =
        serde_json::to_value(schema_for!(rift_protocol::read::SearchHit)).expect("hit schema");
    assert_eq!(
        hit["allOf"][0]["oneOf"][0]["required"],
        json!(["range", "line"])
    );
    assert_eq!(
        hit["allOf"][1]["then"]["required"],
        json!(["range", "line"])
    );
}
