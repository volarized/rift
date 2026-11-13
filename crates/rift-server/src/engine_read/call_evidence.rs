//! Compares independent call observations over the captured source.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId as CoreSymbolId;
use rift_protocol::read::{
    RelationshipFacet, SourceUnitId, SourceUnitSpan, SymbolId, TextRange, TraversalDirection,
};

use crate::ReadService;
use crate::traversal::TRAVERSAL_NODES_MAX;

use super::{CallCorroboration, CallDestination, CallProvider, GraphCalls};

/// The calls one captured static pass established for a selected declaration.
pub(super) struct CapturedCalls<'read> {
    reads: &'read ReadService,
    direction: TraversalDirection,
    edges: Vec<&'read rift_index::RelationshipEdge>,
    truncated: bool,
}

impl<'read> CapturedCalls<'read> {
    pub(super) fn read(
        reads: &'read ReadService,
        seed: &CoreSymbolId,
        direction: TraversalDirection,
    ) -> Self {
        let store = reads.relationships();
        let adjacent = match direction {
            TraversalDirection::Incoming => store.incoming(seed),
            TraversalDirection::Outgoing => store.outgoing(seed),
        };
        let mut calls = adjacent.iter().filter(|edge| {
            edge.facet() == RelationshipFacet::Calls && edge.static_callee().is_some()
        });
        let edges = calls.by_ref().take(TRAVERSAL_NODES_MAX).collect();
        Self {
            reads,
            direction,
            edges,
            truncated: calls.next().is_some(),
        }
    }

    pub(super) fn answer(self) -> Option<GraphCalls> {
        (!self.edges.is_empty()).then(|| self.combine(GraphCalls::default()))
    }

    pub(super) fn combine(self, mut engine: GraphCalls) -> GraphCalls {
        if self.edges.is_empty() {
            return engine;
        }
        let comparison = CallEvidence::new(&engine);
        let mut removed = BTreeSet::new();
        let mut local = BTreeMap::<SymbolId, Vec<SourceUnitSpan>>::new();
        for (neighbor, ranges) in std::mem::take(&mut engine.local) {
            local.entry(neighbor).or_default().extend(ranges);
        }
        engine.complete &= !self.truncated;
        for edge in self.edges {
            let neighbor = match self.direction {
                TraversalDirection::Incoming => edge.from(),
                TraversalDirection::Outgoing => edge.to(),
            };
            let neighbor = SymbolId(neighbor.as_str().to_owned());
            let binding = edge.occurrence();
            let unit = SourceUnitId(binding.unit().to_string());
            let span = SourceUnitSpan {
                unit,
                range: TextRange {
                    start: binding.range().start(),
                    end: binding.range().end(),
                },
            };
            let Some(callee) = edge.static_callee() else {
                continue;
            };
            let conflict = comparison.corroborate(
                &span,
                callee,
                &neighbor,
                (&mut removed, &mut engine.corroboration),
                |destination| {
                    matches!(destination, CallDestination::Local(id) if id == &neighbor
                        || self.direction == TraversalDirection::Incoming
                            && caller_contains(self.reads, id, &neighbor, &span))
                },
            );
            if conflict {
                engine.complete = false;
                continue;
            }
            engine.local.push((neighbor, vec![span]));
        }
        for (neighbor, mut ranges) in local {
            ranges.retain(|range| !removed.contains(&call_key(range)));
            if !ranges.is_empty() || comparison.unlocated.contains(&neighbor) {
                engine.local.push((neighbor, ranges));
            }
        }
        engine.packages.retain_mut(|(_, ranges)| {
            let unlocated = ranges.is_empty();
            ranges.retain(|range| !removed.contains(&call_key(range)));
            unlocated || !ranges.is_empty()
        });
        merge_call_neighbors(&mut engine.local);
        engine
    }
}

fn caller_contains(
    reads: &ReadService,
    engine: &SymbolId,
    authored: &SymbolId,
    occurrence: &SourceUnitSpan,
) -> bool {
    let declaration = |id: &SymbolId| {
        let id = CoreSymbolId::new(id.as_str().to_owned()).ok()?;
        let (declarations, count) = reads.index().symbols_by_identity_page(&id, 0, 1).ok()?;
        let [declaration] = declarations.as_slice() else {
            return None;
        };
        (count == 1 && crate::read::source_unit_id(declaration.file.path()) == occurrence.unit)
            .then_some(declaration.symbol.item_range)
    };
    let (Some(outer), Some(inner)) = (declaration(engine), declaration(authored)) else {
        return false;
    };
    // Incoming observations already name the same callee. An engine can group a
    // nested initializer with its enclosing callable. Keep the innermost caller.
    outer != inner
        && outer.encloses(inner)
        && inner.start <= occurrence.range.start
        && occurrence.range.end <= inner.end
}

type CallKey = (String, u64, u64);

fn call_key(span: &SourceUnitSpan) -> CallKey {
    (span.unit.0.clone(), span.range.start, span.range.end)
}

struct CallEvidence {
    locations: BTreeMap<CallKey, Vec<(CallProvider, CallDestination)>>,
    unlocated: BTreeSet<SymbolId>,
}

impl CallEvidence {
    fn new(engine: &GraphCalls) -> Self {
        let mut locations = BTreeMap::<_, Vec<_>>::new();
        let mut unlocated = BTreeSet::new();
        for (target, ranges) in &engine.local {
            if ranges.is_empty() {
                unlocated.insert(target.clone());
            }
            for range in ranges {
                locations
                    .entry(call_key(range))
                    .or_default()
                    .push((CallProvider::Engine, CallDestination::Local(target.clone())));
            }
        }
        for (target, ranges) in &engine.packages {
            for range in ranges {
                locations.entry(call_key(range)).or_default().push((
                    CallProvider::Engine,
                    CallDestination::Dependency(Box::new(target.clone())),
                ));
            }
        }
        Self {
            locations,
            unlocated,
        }
    }

    fn at(
        &self,
        span: &SourceUnitSpan,
        callee: rift_syntax::ByteRange,
    ) -> impl Iterator<Item = (&CallKey, &Vec<(CallProvider, CallDestination)>)> {
        let key = call_key(span);
        let name = self
            .locations
            .range(
                (span.unit.0.clone(), callee.start, 0)
                    ..=(span.unit.0.clone(), callee.end, u64::MAX),
            )
            .filter(move |((_, start, end), _)| start < end && *end <= callee.end);
        let full = self
            .locations
            .get_key_value(&key)
            .filter(move |((_, start, end), _)| *start < callee.start || *end > callee.end);
        name.chain(full)
    }

    fn corroborate(
        &self,
        span: &SourceUnitSpan,
        callee: rift_syntax::ByteRange,
        target: &SymbolId,
        (removed, records): (&mut BTreeSet<CallKey>, &mut Vec<CallCorroboration>),
        agrees_with: impl Fn(&CallDestination) -> bool,
    ) -> bool {
        let mut conflict = false;
        for (key, destinations) in self.at(span, callee) {
            removed.insert(key.clone());
            for (provider, destination) in destinations {
                let agrees = agrees_with(destination);
                conflict |= !agrees;
                if records.len() == TRAVERSAL_NODES_MAX {
                    return true;
                }
                records.push(CallCorroboration {
                    authored: span.clone(),
                    observed: SourceUnitSpan {
                        unit: span.unit.clone(),
                        range: TextRange {
                            start: key.1,
                            end: key.2,
                        },
                    },
                    static_target: target.clone(),
                    engine_target: destination.clone(),
                    provider: *provider,
                    agrees,
                });
            }
        }
        conflict
    }
}

fn merge_call_neighbors(neighbors: &mut Vec<(SymbolId, Vec<SourceUnitSpan>)>) {
    let mut unique = BTreeMap::<_, Vec<_>>::new();
    for (neighbor, ranges) in std::mem::take(neighbors) {
        unique.entry(neighbor).or_default().extend(ranges);
    }
    for (neighbor, mut ranges) in unique {
        ranges.sort_by_key(call_key);
        ranges.dedup();
        neighbors.push((neighbor, ranges));
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::read::SymbolId;

    #[test]
    fn incoming_engine_group_preserves_the_authored_initializer_caller() {
        use rift_protocol::read::{TextRange, TraversalDirection};

        let directory = tempfile::tempdir().expect("workspace");
        let source = "function callee() {}\nexport function outer() { const token = callee(); return token; }\nfunction other() {}\n";
        std::fs::write(directory.path().join("entry.mjs"), source).expect("source");
        let reads = super::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("captured workspace");
        let identity = |name| {
            super::CoreSymbolId::new(format!(
                "rift://symbol/local/javascript/.%2Fentry.mjs/{name}"
            ))
            .expect("canonical identity")
        };
        let callee = identity("callee");
        let outer = SymbolId(identity("outer").as_str().to_owned());
        let other = SymbolId(identity("other").as_str().to_owned());
        let start = u64::try_from(source.find("callee();").expect("call")).expect("byte position");
        let captured = super::CapturedCalls::read(&reads, &callee, TraversalDirection::Incoming);
        let [edge] = captured.edges.as_slice() else {
            panic!("expected one static call");
        };
        let initializer = SymbolId(edge.from().as_str().to_owned());
        assert_ne!(initializer, outer);
        assert!(initializer.as_str().ends_with("/token"));
        let observed = super::SourceUnitSpan {
            unit: super::SourceUnitId(edge.occurrence().unit().to_string()),
            range: TextRange {
                start,
                end: start + 6,
            },
        };
        for (engine_caller, agrees) in [(outer, true), (other, false)] {
            let calls = super::CapturedCalls::read(&reads, &callee, TraversalDirection::Incoming)
                .combine(super::GraphCalls {
                    local: vec![(engine_caller.clone(), vec![observed.clone()])],
                    complete: true,
                    ..super::GraphCalls::default()
                });
            assert_eq!(calls.complete, agrees);
            let [record] = calls.corroboration.as_slice() else {
                panic!("expected one corroboration record");
            };
            assert_eq!(
                record.engine_target,
                super::CallDestination::Local(engine_caller)
            );
            assert_eq!(record.observed, observed);
            assert_eq!(record.agrees, agrees);
            if agrees {
                let [(actual, ranges)] = calls.local.as_slice() else {
                    panic!("expected one incoming caller");
                };
                assert_eq!(actual, &initializer);
                assert_eq!(ranges, std::slice::from_ref(&record.authored));
            } else {
                assert!(calls.local.is_empty());
            }
        }
    }

    #[test]
    fn authored_call_evidence_matches_full_and_identifier_ranges_only() {
        use rift_protocol::read::{SourceUnitId, SourceUnitSpan, TextRange};

        let span = |start, end| SourceUnitSpan {
            unit: SourceUnitId("rift://source/project/src/lib.rs".to_owned()),
            range: TextRange { start, end },
        };
        let call = span(10, 32);
        let identifier = span(14, 17);
        let nested = span(18, 25);
        let other = span(3, 15);
        let engine = super::GraphCalls {
            local: vec![
                (
                    SymbolId("target".to_owned()),
                    vec![call.clone(), identifier.clone()],
                ),
                (SymbolId("nested".to_owned()), vec![nested, other]),
            ],
            complete: true,
            ..super::GraphCalls::default()
        };
        let evidence = super::CallEvidence::new(&engine);
        let matching = evidence
            .at(&call, rift_syntax::ByteRange { start: 10, end: 17 })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            matching,
            [super::call_key(&identifier), super::call_key(&call)]
        );
    }

    #[test]
    fn independent_call_providers_keep_original_agreement_and_conflict_evidence() {
        use rift_protocol::read::{SourceUnitId, SourceUnitSpan, TextRange};
        use std::collections::BTreeSet;

        let span = |start, end| SourceUnitSpan {
            unit: SourceUnitId("rift://source/project/src/lib.rs".to_owned()),
            range: TextRange { start, end },
        };
        let static_target = SymbolId("target".to_owned());
        let other_target = SymbolId("other".to_owned());
        let authored = span(20, 31);
        let name = span(20, 24);
        let unrelated = span(41, 45);
        let engine = super::GraphCalls {
            local: vec![(static_target.clone(), vec![name.clone()])],
            complete: true,
            ..super::GraphCalls::default()
        };
        let mut evidence = super::CallEvidence::new(&engine);
        evidence.locations.insert(
            super::call_key(&authored),
            vec![(
                super::CallProvider::Extractor,
                super::CallDestination::Local(other_target.clone()),
            )],
        );
        evidence.locations.insert(
            super::call_key(&unrelated),
            vec![(
                super::CallProvider::Extractor,
                super::CallDestination::Local(static_target.clone()),
            )],
        );
        let mut removed = BTreeSet::new();
        let mut records = Vec::new();
        let conflict = evidence.corroborate(
            &authored,
            rift_syntax::ByteRange { start: 20, end: 24 },
            &static_target,
            (&mut removed, &mut records),
            |destination| matches!(destination, super::CallDestination::Local(id) if id == &static_target),
        );
        assert!(conflict);
        assert_eq!(
            removed,
            BTreeSet::from([super::call_key(&name), super::call_key(&authored)])
        );
        let [agreed, disagreed] = records.as_slice() else {
            panic!("expected independent agreement and disagreement, got {records:?}");
        };
        assert_eq!(agreed.provider, super::CallProvider::Engine);
        assert_eq!(agreed.observed, name);
        assert!(agreed.agrees);
        assert_eq!(disagreed.provider, super::CallProvider::Extractor);
        assert_eq!(disagreed.observed, authored);
        assert_eq!(
            disagreed.engine_target,
            super::CallDestination::Local(other_target)
        );
        assert!(!disagreed.agrees);
    }
}
