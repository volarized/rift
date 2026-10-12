use super::*;
use crate::symbol_read::CapturedViewId;
use serde_json::json;

#[test]
fn search_selection_failures_preserve_existing_typed_causes() {
    for reason in [
        SymbolUnavailableReason::ExactRelease,
        SymbolUnavailableReason::IndexPreparing,
        SymbolUnavailableReason::ReadFailed,
        SymbolUnavailableReason::CorruptPublication,
        SymbolUnavailableReason::InsufficientCoverage,
        SymbolUnavailableReason::ViewExpired,
        SymbolUnavailableReason::ViewContext,
    ] {
        let result = SearchUnavailable::Unavailable {
            reason,
            view: Some(CapturedView {
                id: CapturedViewId::parse("claims.signature").expect("opaque view"),
                expires_at: "2026-10-10T10:05:00Z".to_owned(),
            }),
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&result).expect("failure JSON");
        assert_eq!(value["outcome"], "unavailable");
        assert!(value.get("id").is_none());
        assert!(value.get("warnings").is_none());
        assert_eq!(
            serde_json::from_value::<SearchUnavailable>(value).expect("failure decode"),
            result
        );
    }
}

#[test]
fn search_unavailability_does_not_accept_service_errors_or_exact_selectors() {
    for value in [
        json!({"type":"about:blank","title":"Unavailable","status":503,"detail":"read failed"}),
        json!({"outcome":"missing","reason":"view_expired"}),
        json!({"outcome":"unavailable","reason":"unknown"}),
        json!({"outcome":"unavailable","reason":"view_context","id":"rift://symbol/local/rust/app/run"}),
    ] {
        assert!(serde_json::from_value::<SearchUnavailable>(value).is_err());
    }
    let result = serde_json::from_value::<SearchUnavailable>(
        json!({"outcome":"unavailable","reason":"index_preparing"}),
    )
    .expect("selection without an admitted view");
    assert!(matches!(
        result,
        SearchUnavailable::Unavailable {
            reason: SymbolUnavailableReason::IndexPreparing,
            view: None,
            warnings
        } if warnings.is_empty()
    ));
}

#[test]
fn search_failure_schema_and_runtime_reuse_captured_view_bounds() {
    let schema = serde_json::to_value(schemars::schema_for!(SearchUnavailable)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    for key in ["a".repeat(64), "claims.signature".to_owned()] {
        let value = json!({
            "outcome":"unavailable", "reason":"view_expired",
            "view":{"id":key,"expires_at":"2026-10-10T10:05:00Z"}
        });
        assert!(validator.is_valid(&value));
        assert!(serde_json::from_value::<SearchUnavailable>(value).is_ok());
    }
    for key in ["claims.".to_owned(), "c".repeat(4097)] {
        let value = json!({
            "outcome":"unavailable", "reason":"view_context",
            "view":{"id":key,"expires_at":"2026-10-10T10:05:00Z"}
        });
        assert!(!validator.is_valid(&value));
        assert!(serde_json::from_value::<SearchUnavailable>(value).is_err());
    }
}
