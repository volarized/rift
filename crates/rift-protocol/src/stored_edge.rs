//! Stored producer rows, before read projection.

use crate::read::{
    EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, Extensions, SourceUnitSpan, SymbolId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One producer row retained in a package publication.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_edge_empty_defaults)]
pub struct StoredEdge {
    /// The symbol the edge starts at.
    pub from: SymbolId,
    /// What the edge is in this provider's vocabulary, such as `import`, `use`, or
    /// `implements`.
    pub kind: ExactKind,
    /// Portable classification, so a query for `imports` finds local kinds such as `import`
    /// and `use` alike.
    #[schemars(length(min = 1))]
    pub kinds: Vec<EdgeKind>,
    /// The symbol the edge points at. One Rift cannot read carries the `external` origin;
    /// the edge is the same either way.
    pub to: SymbolId,
    /// Exact physical occurrence when this edge was read from source, independent of
    /// whether the provider supplied a source syntax element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<SourceUnitSpan>,
    /// Which analysis produced this edge.
    pub provider: EdgeProvider,
    /// How this edge was established.
    pub derivation: EdgeDerivation,
    /// How likely a `heuristic` edge is to hold, from 0 to 1. Absent for any other
    /// derivation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0, max = 1))]
    pub confidence: Option<f64>,
    /// The external producer that emitted the edge, named by a package publication that
    /// carries its facts. Absent for an edge Rift's own analysis produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<crate::producer::FactProducer>,
    /// Edge facts the model has no field for, namespaced by the provider that emitted them.
    /// Absent when empty.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl From<StoredEdge> for crate::read::Edge {
    fn from(edge: StoredEdge) -> Self {
        Self {
            from: edge.from,
            kind: edge.kind,
            kinds: edge.kinds,
            to: edge.to,
            occurrence: edge.occurrence,
            providers: std::collections::BTreeSet::from([edge.provider]),
            derivation: edge.derivation,
            confidence: edge.confidence,
            producer: edge.producer,
            extensions: edge.extensions,
        }
    }
}

impl StoredEdge {
    /// Projects bounded producer rows into merged read edges before paging.
    /// `include.providers` attaches each edge's sorted producer set.
    #[must_use]
    pub fn project(
        rows: impl IntoIterator<Item = Self>,
        include: crate::edge_read::EdgeInclude,
    ) -> Vec<crate::read::Edge> {
        let mut edges = crate::read::Edge::merged(rows.into_iter().map(Into::into));
        if !include.providers {
            for edge in &mut edges {
                edge.providers.clear();
            }
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::StoredEdge;
    use crate::edge_read::EdgeInclude;
    use crate::read::EdgeProvider;
    use serde_json::json;

    fn row(provider: EdgeProvider) -> StoredEdge {
        serde_json::from_value(json!({
            "from": "rift://symbol/local/rust/app/caller",
            "kind": "calls", "kinds": ["calls"],
            "to": "rift://symbol/local/rust/app/callee",
            "occurrence": {"unit": "rift://source/project/src/lib.rs", "range": {"start": 10, "end": 16}},
            "provider": provider, "derivation": "resolution"
        })).expect("stored producer row")
    }

    #[test]
    fn three_analyses_merge_and_providers_are_requested_only() {
        let rows = [
            EdgeProvider::Static,
            EdgeProvider::Lsp,
            EdgeProvider::Engine,
            EdgeProvider::Static,
        ]
        .map(row);
        let default = StoredEdge::project(rows.clone(), EdgeInclude::default());
        assert_eq!(default.len(), 1);
        let value = serde_json::to_value(&default[0]).expect("read edge");
        assert!(value.get("provider").is_none());
        assert!(value.get("providers").is_none());
        let included = StoredEdge::project(
            rows.clone(),
            EdgeInclude {
                providers: true,
                ..EdgeInclude::default()
            },
        );
        assert_eq!(included.len(), 1);
        let value = serde_json::to_value(&included[0]).expect("read edge");
        assert_eq!(value["providers"], json!(["engine", "lsp", "static"]));
        assert!(value.get("provider").is_none());
        for stored in rows {
            let value = serde_json::to_value(&stored).expect("stored edge");
            assert!(value.get("provider").is_some());
            assert!(value.get("providers").is_none());
            assert_eq!(
                serde_json::from_value::<StoredEdge>(value).expect("unchanged row format"),
                stored
            );
        }
    }

    #[test]
    fn merge_keeps_each_identity_member_and_empty_input() {
        assert!(StoredEdge::project([], EdgeInclude::default()).is_empty());
        let original = row(EdgeProvider::Static);
        let mut rows = vec![original.clone(); 5];
        rows[1].from = original.to.clone();
        rows[2].to = original.from.clone();
        rows[3].kind.0 = "references".to_owned();
        rows[4]
            .occurrence
            .as_mut()
            .expect("physical occurrence")
            .range
            .start += 1;
        let projected = StoredEdge::project(rows, EdgeInclude::default());
        assert_eq!(projected.len(), 5);
        let pagination = crate::read::Pagination::new(2, 1, projected.len() as u64);
        assert_eq!(pagination.total, 5);
        assert_eq!(pagination.pages, 5);
    }
}
