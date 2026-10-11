//! Bounded relationship traversal over captured adjacency.

use std::collections::{BTreeSet, VecDeque};

use rift_core::{LoopBudget, SymbolId};
use rift_protocol::read::{GraphHop, SearchTraversal, TraversalDirection};

/// One traversal walk's outcome: each discovered symbol with its shortest path, and whether
/// the node bound stopped the walk before the reachable graph was exhausted.
#[derive(Debug)]
pub struct TraversalWalk {
    /// Each discovered symbol and its shortest path, in adjacency order.
    pub discovered: Vec<(SymbolId, Vec<GraphHop>)>,
    /// Whether the node bound prevented discovery of a reachable symbol.
    pub truncated: bool,
}

/// Walks captured adjacency breadth-first from `seed`, honoring the depth and node bounds.
///
/// The caller supplies bounded adjacency in its established order, after applying the
/// request's facet filter. Each pair carries the established next identity and its exact
/// relationship evidence. The walk never reports `seed` itself and never requeues a
/// discovered symbol. One `nodes_max` budget covers the whole walk.
#[must_use]
pub fn walk_traversal_capped<Edges>(
    seed: &SymbolId,
    traversal: &SearchTraversal,
    nodes_max: usize,
    mut edges: impl FnMut(&SymbolId, TraversalDirection) -> Edges,
) -> TraversalWalk
where
    Edges: IntoIterator<Item = (SymbolId, GraphHop)>,
{
    let mut enqueued: BTreeSet<SymbolId> = BTreeSet::from([seed.clone()]);
    let mut recorded: BTreeSet<SymbolId> = BTreeSet::from([seed.clone()]);
    let mut budget = LoopBudget::new(nodes_max);
    let mut truncated = false;
    let mut queue: VecDeque<(SymbolId, Vec<GraphHop>)> =
        VecDeque::from([(seed.clone(), Vec::new())]);
    let mut discovered = Vec::new();
    while let Some((current, path)) = queue.pop_front() {
        if path.len() as u64 >= traversal.depth {
            continue;
        }
        for (next, hop) in edges(&current, traversal.direction) {
            if recorded.contains(&next) {
                continue;
            }
            if budget.consume().is_err() {
                truncated = true;
                continue;
            }
            recorded.insert(next.clone());
            let mut next_path = path.clone();
            next_path.push(hop);
            discovered.push((next.clone(), next_path.clone()));
            if enqueued.insert(next.clone()) {
                queue.push_back((next, next_path));
            }
        }
    }
    TraversalWalk {
        discovered,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
    use rift_protocol::read::{
        ExactKind, Extensions, HopDirection, Language, Relationship, RelationshipDerivation,
        RelationshipFacet, SymbolId as WireSymbolId,
    };

    use super::*;

    fn identity(name: &str) -> SymbolId {
        let identity = SymbolIdentity::new(
            SymbolOwner::Local,
            Language::from_identity_segment("rust").expect("fixture language"),
            vec!["beacon".to_owned(), name.to_owned()],
        )
        .expect("fixture identity");
        SymbolId::new(identity.wire_identity()).expect("checked identity")
    }

    fn request(seed: &SymbolId, depth: u64) -> SearchTraversal {
        SearchTraversal {
            seed: Some(WireSymbolId(seed.as_str().to_owned())),
            direction: TraversalDirection::Outgoing,
            facets: vec![RelationshipFacet::Calls],
            depth,
            to: None,
        }
    }

    fn hop(from: &SymbolId, to: &SymbolId) -> GraphHop {
        GraphHop {
            relationship: Relationship {
                from: WireSymbolId(from.as_str().to_owned()),
                to: WireSymbolId(to.as_str().to_owned()),
                kind: ExactKind("calls".to_owned()),
                facets: vec![RelationshipFacet::Calls],
                evidence: Vec::new(),
                occurrence: None,
                derivation: RelationshipDerivation::Syntax,
                confidence: None,
                extensions: Extensions::default(),
            },
            direction: HopDirection::Outgoing,
        }
    }

    #[test]
    fn callback_walk_keeps_order_shortest_paths_cycles_and_source_less_evidence() {
        let seed = identity("seed");
        let first = identity("first");
        let second = identity("second");
        let last = identity("last");
        let request = request(&seed, 2);
        let walk = walk_traversal_capped(&seed, &request, 3, |current, direction| {
            assert_eq!(direction, TraversalDirection::Outgoing);
            let neighbors = if current == &seed {
                vec![first.clone(), second.clone()]
            } else {
                vec![seed.clone(), last.clone()]
            };
            neighbors
                .into_iter()
                .map(|next| (next.clone(), hop(current, &next)))
                .collect::<Vec<_>>()
        });
        assert!(!walk.truncated, "cycles do not spend the node budget");
        assert_eq!(
            walk.discovered.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            [&first, &second, &last]
        );
        let path = &walk.discovered[2].1;
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].relationship.to.0, first.as_str());
        assert_eq!(path[1].relationship.to.0, last.as_str());
        assert!(path.iter().all(|hop| hop.relationship.occurrence.is_none()));
        assert!(
            path.iter()
                .all(|hop| { hop.relationship.derivation == RelationshipDerivation::Syntax })
        );
    }

    #[test]
    fn callback_walk_reports_zero_exact_and_one_over_node_bounds() {
        let seed = identity("seed");
        let first = identity("first");
        let second = identity("second");
        let request = request(&seed, 1);
        for (maximum, count, truncated) in [(0, 0, true), (1, 1, true), (2, 2, false)] {
            let walk = walk_traversal_capped(&seed, &request, maximum, |current, _| {
                [&first, &second]
                    .into_iter()
                    .map(|next| (next.clone(), hop(current, next)))
                    .collect::<Vec<_>>()
            });
            assert_eq!(walk.discovered.len(), count, "maximum={maximum}");
            assert_eq!(walk.truncated, truncated, "maximum={maximum}");
        }
    }

    #[test]
    fn callback_walk_stops_at_depth_and_keeps_empty_adjacency_complete() {
        let seed = identity("seed");
        let next = identity("next");
        let calls = Cell::new(0);
        let request = request(&seed, 1);
        let walk = walk_traversal_capped(&seed, &request, 1, |current, _| {
            calls.set(calls.get() + 1);
            vec![(next.clone(), hop(current, &next))]
        });
        assert_eq!(calls.get(), 1, "depth-bound nodes do not request adjacency");
        assert_eq!(walk.discovered.len(), 1);
        assert!(!walk.truncated);
        let empty = walk_traversal_capped(&seed, &request, 0, |_, _| Vec::new());
        assert!(empty.discovered.is_empty());
        assert!(
            !empty.truncated,
            "an empty graph did not spend the node budget"
        );
    }
}
