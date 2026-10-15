//! Captured search and pattern selection failures.

use crate::{
    read::ReadWarning,
    symbol_read::{CapturedView, SymbolUnavailableReason},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The captured selection cannot serve this search or pattern request.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SearchUnavailable {
    /// Presence or absence cannot be established in the requested selection.
    Unavailable {
        /// The existing typed cause of selection unavailability.
        reason: SymbolUnavailableReason,
        /// Selected view, when one could be captured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view: Option<CapturedView>,
        /// Details and partial-read warnings.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        warnings: Vec<ReadWarning>,
    },
}

/// One page of search hits, or the typed cause of selection unavailability.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
#[schemars(extend("examples" = [{
    "outcome": "unavailable",
    "reason": "insufficient_coverage"
}]))]
pub enum SearchReadResult {
    /// Search hits and the captured serving context, including an empty page.
    Results(Box<crate::search::SearchResult>),
    /// The captured selection cannot serve this request.
    Unavailable(SearchUnavailable),
}

#[cfg(test)]
mod schema_tests {
    use super::{SearchReadResult, SearchUnavailable};
    use serde_json::json;

    #[test]
    fn captured_search_result_and_unavailable_are_distinct_in_wire_and_schema() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchReadResult))
            .expect("search result schema");
        let validator = jsonschema::validator_for(&schema).expect("search result validator");
        let view = json!({
            "id": "claims.selection",
            "expires_at": "2026-10-11T12:15:00Z"
        });
        let page = json!({
            "view": view,
            "results": [],
            "pagination": {"page_index": 2, "total_pages": 0}
        });
        let unavailable = json!({
            "outcome": "unavailable",
            "reason": "view_expired",
            "view": view
        });
        for value in [&page, &unavailable] {
            assert!(validator.is_valid(value));
            let decoded: SearchReadResult =
                serde_json::from_value(value.clone()).expect("one strict branch");
            assert_eq!(serde_json::to_value(decoded).expect("result wire"), *value);
        }
        assert!(matches!(
            serde_json::from_value::<SearchReadResult>(page).expect("empty captured page"),
            SearchReadResult::Results(page) if page.results.is_empty() && page.view.is_some()
        ));
        assert!(matches!(
            serde_json::from_value::<SearchReadResult>(unavailable).expect("expired view"),
            SearchReadResult::Unavailable(SearchUnavailable::Unavailable { .. })
        ));
    }

    #[test]
    fn captured_search_refuses_mixed_and_malformed_result_branches() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchReadResult))
            .expect("search result schema");
        let validator = jsonschema::validator_for(&schema).expect("search result validator");
        for value in [
            json!({
                "outcome": "unavailable", "reason": "view_context",
                "results": [], "pagination": {"page_index": 0, "total_pages": 0}
            }),
            json!({"results": [], "outcome": "unavailable"}),
            json!({"outcome": "unavailable", "reason": "view_context", "unknown": true}),
            json!({
                "results": [], "pagination": {"page_index": 0, "total_pages": 0},
                "view": {"id": "claims.selection"}
            }),
        ] {
            assert!(!validator.is_valid(&value), "{value}");
            assert!(
                serde_json::from_value::<SearchReadResult>(value.clone()).is_err(),
                "{value}"
            );
        }
    }
}

#[cfg(test)]
mod tests;
