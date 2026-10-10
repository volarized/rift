//! Search results admitted through the shared symbol model or the lexical package model.

use std::collections::HashMap;

use rift_protocol::{
    read::{SearchHit, SearchHitTarget},
    symbol_read::CapturedView,
};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::generated::{PackageDocumentationHit, PackageSearchHit, PublicationFormat, Warning};

/// One symbol or documentation result from the global index.
#[derive(Clone, Debug, PartialEq)]
pub enum PackageSearchItem {
    /// A lexical package declaration with its contributing fields and match class.
    Search(Box<PackageSearchHit>),
    /// A lexical documentation block with its contributing fields.
    Documentation(Box<PackageDocumentationHit>),
    /// A canonical symbol, including a runtime symbol without readable source.
    Symbol(Box<SearchHit>),
}

impl<'de> Deserialize<'de> for PackageSearchItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value
            .as_object()
            .is_some_and(|fields| fields.contains_key("hit"))
        {
            let hit: SearchHit = serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            if !matches!(hit.hit, SearchHitTarget::Symbol { .. }) {
                return Err(serde::de::Error::custom(
                    "global search hit must target a symbol",
                ));
            }
            return Ok(Self::Symbol(Box::new(hit)));
        }
        if let Ok(hit) = serde_json::from_value::<PackageSearchHit>(value.clone()) {
            return Ok(Self::Search(Box::new(hit)));
        }
        serde_json::from_value::<PackageDocumentationHit>(value)
            .map(|hit| Self::Documentation(Box::new(hit)))
            .map_err(serde::de::Error::custom)
    }
}

/// One global search page from an immutable captured view.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PackageSearchPage {
    /// Ordered symbol and documentation results.
    pub items: Vec<PackageSearchItem>,
    /// Opaque continuation bound to this view and the original request.
    pub next_cursor: Option<String>,
    /// Warnings attached to this page.
    pub warnings: Vec<Warning>,
    /// Publication format used by the package index.
    pub publication_format: PublicationFormat,
    /// Analyzer revision shared by this view.
    pub analyzer_revision: String,
    /// Corpus revision shared by this view.
    pub corpus_revision: String,
    /// Documentation revision shared by this view when documentation is selected.
    pub documentation_revision: Option<String>,
    /// Original captured view and expiry, including for an empty page.
    pub view: CapturedView,
    /// Additional properties retained from the lexical page contract.
    #[serde(flatten)]
    pub additional_properties: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::{PackageSearchItem, PackageSearchPage};
    use rift_protocol::read::SearchHitTarget;
    use serde_json::{Value, json};

    fn runtime_hit() -> Value {
        json!({"hit": {"target": "symbol", "symbol": {
            "id": "rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile",
            "language": "javascript", "name": "readFile", "kind": "function",
            "origin": {"location": "stdlib", "source_kind": "authored",
                "runtime": {"runtime": "node", "version": "26.11.1"}}
        }}})
    }

    #[test]
    fn source_less_runtime_retains_canonical_object_without_physical_fields() {
        let item: PackageSearchItem =
            serde_json::from_value(runtime_hit()).expect("runtime fixture");
        let PackageSearchItem::Symbol(hit) = item else {
            panic!("shared symbol branch");
        };
        assert!(hit.distance.is_none());
        assert!(hit.unit.is_none());
        assert!(hit.path.is_none());
        assert!(hit.range.is_none());
        assert!(hit.line.is_none());
        assert!(hit.source.is_none());
        let SearchHitTarget::Symbol { symbol } = hit.hit else {
            panic!("symbol target");
        };
        assert_eq!(symbol.name, "readFile");
        assert!(symbol.origin.package.is_none());
        assert_eq!(
            symbol.id.expect("canonical identity").0,
            "rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile"
        );
    }

    #[test]
    fn source_less_traversal_retains_shared_relationship_path() {
        let mut value = runtime_hit();
        value["distance"] = json!(1);
        value["traversal_path"] = json!([{
            "direction": "outgoing",
            "relationship": {
                "from": "rift://symbol/stdlib/node@26.11.1/javascript/fs/open",
                "to": "rift://symbol/stdlib/node@26.11.1/javascript/fs/readFile",
                "kind": "call", "facets": ["calls"], "derivation": "resolution"
            }
        }]);
        let expected: rift_protocol::read::SearchHit =
            serde_json::from_value(value.clone()).expect("shared traversal fixture");
        let item: PackageSearchItem = serde_json::from_value(value).expect("traversal fixture");
        let PackageSearchItem::Symbol(hit) = item else {
            panic!("shared symbol branch");
        };
        assert_eq!(*hit, expected);
        assert_eq!(hit.distance, Some(1));
        assert_eq!(hit.traversal_path.expect("relationship path").len(), 1);
        assert!(hit.unit.is_none());
        assert!(hit.range.is_none());
    }

    #[test]
    fn malformed_shared_hit_never_falls_back_to_permissive_legacy_branch() {
        for hit in [Value::Null, json!({}), json!({"target": "documentation"})] {
            let value = json!({"hit": hit, "target": "documentation"});
            assert!(serde_json::from_value::<PackageSearchItem>(value).is_err());
        }
        for field in [
            "package",
            "symbol",
            "contributing_fields",
            "match_class",
            "target",
        ] {
            let mut mixed = runtime_hit();
            mixed[field] = json!("legacy");
            assert!(
                serde_json::from_value::<PackageSearchItem>(mixed).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn legacy_lexical_branches_preserve_ranking_fields() {
        let value = json!({
            "package": {"manager": "cargo", "registry": "crates.io", "name": "demo", "version": "1.0.0"},
            "symbol": {"id": "rift://symbol/local/rust/demo/parse", "language": "rust",
                "name": "parse", "kind": "function"},
            "unit": "rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs",
            "range": {"start": 0, "end": 4}, "line": 1,
            "contributing_fields": ["qualified_name"], "match_class": "qualified_exact"
        });
        let legacy: crate::generated::PackageSearchHit =
            serde_json::from_value(value.clone()).expect("legacy fixture");
        let item: PackageSearchItem = serde_json::from_value(value).expect("legacy fixture");
        assert_eq!(item, PackageSearchItem::Search(Box::new(legacy)));
        let documentation = json!({"target": "documentation"});
        let legacy: crate::generated::PackageDocumentationHit =
            serde_json::from_value(documentation.clone()).expect("legacy defaults");
        assert_eq!(
            serde_json::from_value::<PackageSearchItem>(documentation).expect("legacy defaults"),
            PackageSearchItem::Documentation(Box::new(legacy))
        );
    }

    #[test]
    fn empty_page_requires_original_captured_view() {
        let mut value = json!({"items": [], "warnings": [],
            "publication_format": "rift-package-index-v2",
            "analyzer_revision": "analyzer", "corpus_revision": "corpus"});
        assert!(serde_json::from_value::<PackageSearchPage>(value.clone()).is_err());
        value["view"] = json!({"id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "expires_at": "2026-10-10T12:00:00Z"});
        let page: PackageSearchPage = serde_json::from_value(value).expect("captured page");
        assert!(page.items.is_empty());
        assert_eq!(page.view.expires_at, "2026-10-10T12:00:00Z");
    }
}
