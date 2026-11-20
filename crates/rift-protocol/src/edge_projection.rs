//! Read projection of producer rows into logical edges.

use std::collections::BTreeMap;

use crate::read::{Edge, ExactKind, SourceUnitId, SymbolId};

type EdgeIdentity = (
    SymbolId,
    ExactKind,
    SymbolId,
    Option<(SourceUnitId, u64, u64)>,
);

impl Edge {
    /// The identity shared by analyses of the same physical edge.
    #[must_use]
    pub fn identity(&self) -> impl Ord + use<'_> {
        (
            &self.from,
            &self.kind,
            &self.to,
            self.occurrence
                .as_ref()
                .map(|span| (&span.unit, span.range.start, span.range.end)),
        )
    }

    /// Merges producer rows by endpoints, exact kind and occurrence, in first-seen order.
    /// The first row supplies evidence; `providers` is the sorted set from every row.
    /// Work is O(n log n) for the caller's bounded input of n rows.
    #[must_use]
    pub fn merged(edges: impl IntoIterator<Item = Self>) -> Vec<Self> {
        let mut positions = BTreeMap::<EdgeIdentity, usize>::new();
        let mut merged: Vec<Self> = Vec::new();
        for edge in edges {
            let key = (
                edge.from.clone(),
                edge.kind.clone(),
                edge.to.clone(),
                edge.occurrence
                    .as_ref()
                    .map(|span| (span.unit.clone(), span.range.start, span.range.end)),
            );
            if let Some(position) = positions.get(&key) {
                let existing = &mut merged[*position];
                existing.providers.extend(edge.providers);
            } else {
                positions.insert(key, merged.len());
                merged.push(edge);
            }
        }
        merged
    }
}
