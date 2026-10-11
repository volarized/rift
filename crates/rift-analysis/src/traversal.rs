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
    let mut state = TraversalState::new(seed, nodes_max);
    while let Some((current, path)) = state.next(traversal.depth) {
        state.record(&path, edges(&current, traversal.direction));
    }
    state.finish()
}

/// Walks bounded adjacency read asynchronously, using the same transitions as
/// [`walk_traversal_capped`]. The callback receives an owned identity and applies
/// the request's facet filter before returning adjacency in its established order.
/// Dropping this future drops the current read and the unpublished walk.
///
/// # Errors
/// Returns the callback's registered error unchanged. No partial walk is returned.
pub async fn walk_traversal_capped_async<Edges, Read>(
    seed: &SymbolId,
    traversal: &SearchTraversal,
    nodes_max: usize,
    mut edges: impl FnMut(SymbolId, TraversalDirection) -> Read,
) -> Result<TraversalWalk, rift_error::RiftError>
where
    Edges: IntoIterator<Item = (SymbolId, GraphHop)>,
    Read: std::future::Future<Output = Result<Edges, rift_error::RiftError>>,
{
    let mut state = TraversalState::new(seed, nodes_max);
    while let Some((current, path)) = state.next(traversal.depth) {
        let adjacency = edges(current, traversal.direction).await?;
        state.record(&path, adjacency);
    }
    Ok(state.finish())
}

struct TraversalState {
    enqueued: BTreeSet<SymbolId>,
    recorded: BTreeSet<SymbolId>,
    budget: LoopBudget,
    queue: VecDeque<(SymbolId, Vec<GraphHop>)>,
    walk: TraversalWalk,
}

impl TraversalState {
    fn new(seed: &SymbolId, nodes_max: usize) -> Self {
        Self {
            enqueued: BTreeSet::from([seed.clone()]),
            recorded: BTreeSet::from([seed.clone()]),
            budget: LoopBudget::new(nodes_max),
            queue: VecDeque::from([(seed.clone(), Vec::new())]),
            walk: TraversalWalk {
                discovered: Vec::new(),
                truncated: false,
            },
        }
    }

    fn next(&mut self, depth: u64) -> Option<(SymbolId, Vec<GraphHop>)> {
        while let Some((current, path)) = self.queue.pop_front() {
            if (path.len() as u64) < depth {
                return Some((current, path));
            }
        }
        None
    }

    fn record(&mut self, path: &[GraphHop], edges: impl IntoIterator<Item = (SymbolId, GraphHop)>) {
        for (next, hop) in edges {
            if self.recorded.contains(&next) {
                continue;
            }
            if self.budget.consume().is_err() {
                self.walk.truncated = true;
                continue;
            }
            self.recorded.insert(next.clone());
            let mut next_path = path.to_vec();
            next_path.push(hop);
            self.walk.discovered.push((next.clone(), next_path.clone()));
            if self.enqueued.insert(next.clone()) {
                self.queue.push_back((next, next_path));
            }
        }
    }

    fn finish(self) -> TraversalWalk {
        self.walk
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

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

    #[test]
    fn asynchronous_walk_matches_sync_order_paths_cycles_depth_and_caps() {
        fn require_send(_: &impl Send) {}

        let seed = identity("seed");
        let first = identity("first");
        let second = identity("second");
        let last = identity("last");
        let adjacency = |current: &SymbolId, direction| {
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
        };
        let mut context = Context::from_waker(Waker::noop());
        for depth in [0, 1, 2, 3] {
            for maximum in [0, 1, 2, 3, 4] {
                let request = request(&seed, depth);
                let expected = walk_traversal_capped(&seed, &request, maximum, adjacency);
                let mut actual = Box::pin(walk_traversal_capped_async(
                    &seed,
                    &request,
                    maximum,
                    |current, direction| std::future::ready(Ok(adjacency(&current, direction))),
                ));
                require_send(&actual);
                let Poll::Ready(Ok(actual)) = actual.as_mut().poll(&mut context) else {
                    panic!("ready adjacency must finish without suspension");
                };
                assert_eq!(actual.discovered, expected.discovered);
                assert_eq!(actual.truncated, expected.truncated);
            }
        }
    }

    #[test]
    fn asynchronous_walk_keeps_budget_across_pending_reads_and_drops_current_read() {
        struct ReadDrop<'a>(&'a Cell<usize>);
        impl Drop for ReadDrop<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let seed = identity("seed");
        let first = identity("first");
        let second = identity("second");
        let request = request(&seed, 2);
        let calls = Cell::new(0);
        let mut walk = Box::pin(walk_traversal_capped_async(
            &seed,
            &request,
            1,
            |current, _| {
                calls.set(calls.get() + 1);
                let mut pending = true;
                let next = if current == seed {
                    vec![first.clone(), second.clone()]
                } else {
                    vec![seed.clone()]
                };
                std::future::poll_fn(move |context| {
                    if pending {
                        pending = false;
                        context.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(next
                        .iter()
                        .map(|next| (next.clone(), hop(&current, next)))
                        .collect::<Vec<_>>()))
                })
            },
        ));
        let mut context = Context::from_waker(Waker::noop());
        assert!(walk.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.get(), 1);
        assert!(walk.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.get(), 2);
        let Poll::Ready(Ok(result)) = walk.as_mut().poll(&mut context) else {
            panic!("both adjacency reads were made ready");
        };
        assert_eq!(result.discovered.len(), 1);
        assert_eq!(result.discovered[0].0, first);
        assert!(result.truncated);

        let dropped = Cell::new(0);
        let mut cancelled = Box::pin(walk_traversal_capped_async(&seed, &request, 1, |_, _| {
            let guard = ReadDrop(&dropped);
            async move {
                let _guard = guard;
                std::future::pending::<Result<Vec<(SymbolId, GraphHop)>, rift_error::RiftError>>()
                    .await
            }
        }));
        assert!(cancelled.as_mut().poll(&mut context).is_pending());
        assert_eq!(dropped.get(), 0);
        drop(cancelled);
        assert_eq!(dropped.get(), 1, "dropping the walk drops its current read");
    }

    #[test]
    fn asynchronous_walk_returns_registered_failure_without_later_reads() {
        let seed = identity("seed");
        let request = request(&seed, 3);
        let calls = Cell::new(0);
        let mut walk = Box::pin(walk_traversal_capped_async(&seed, &request, 5, |_, _| {
            calls.set(calls.get() + 1);
            std::future::ready(Err::<Vec<(SymbolId, GraphHop)>, _>(
                rift_error::errors::server::read_cancelled().error(),
            ))
        }));
        let mut context = Context::from_waker(Waker::noop());
        let Poll::Ready(Err(error)) = walk.as_mut().poll(&mut context) else {
            panic!("the captured read returned a registered failure");
        };
        assert_eq!(
            error.slug(),
            rift_error::errors::server::read_cancelled::SLUG
        );
        assert_eq!(calls.get(), 1, "failure stops later adjacency reads");
    }
}
