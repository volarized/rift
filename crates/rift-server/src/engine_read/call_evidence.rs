//! Compares independent call observations over the captured source.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId as CoreSymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{
    EdgeKind, ReadWarning, SourceUnitId, SourceUnitSpan, SymbolId, TextRange, TraversalDirection,
};

use crate::ReadService;
use crate::traversal::TRAVERSAL_SYMBOLS_MAX;

use super::{CallCorroboration, CallDestination, CallProvider, GraphCalls};

/// The engine references one confirmation read for each callee, kept for the other callers
/// of the same read.
///
/// A callee's references answer the same for every caller, so callers that share a callee
/// read its references once instead of once each: the read is linear in callers and callees,
/// not in their product. A failed read is not kept, so the next caller asks again.
#[derive(Default)]
pub(super) struct TargetReferences {
    reports: BTreeMap<SymbolId, TargetReport>,
}

/// The parts of one callee's engine references that a confirmation reads.
struct TargetReport {
    positions: BTreeMap<CallKey, SourceUnitSpan>,
    edge_warnings: Vec<ReadWarning>,
}

impl TargetReferences {
    /// The callee's references, read from the engine only the first time a caller asks.
    async fn read(
        &mut self,
        reads: &ReadService,
        engines: &crate::EnginePool,
        target: &SymbolId,
        deadline: tokio::time::Instant,
    ) -> Option<&TargetReport> {
        if !self.reports.contains_key(target) {
            // This read contains only engine observations, never captured references.
            let report = Box::pin(super::engine_graph_references(
                reads, engines, target, false, deadline,
            ))
            .await
            .ok()?;
            let positions = report
                .occurrences
                .into_iter()
                .filter(|occurrence| occurrence.target == *target)
                .map(|occurrence| (call_key(&occurrence.source), occurrence.source))
                .collect();
            self.reports.insert(
                target.clone(),
                TargetReport {
                    positions,
                    edge_warnings: report.edge_warnings,
                },
            );
        }
        self.reports.get(target)
    }
}

/// Retains a concrete call when a separate engine operation confirms its callee token.
/// Original disagreements and incomplete provider coverage remain in the answer.
pub(super) async fn confirm_reference_sites(
    reads: &ReadService,
    engines: &crate::EnginePool,
    seed: &SymbolId,
    mut calls: GraphCalls,
    targets: &mut TargetReferences,
    deadline: tokio::time::Instant,
) -> GraphCalls {
    let mut pending = BTreeMap::<SymbolId, Vec<usize>>::new();
    for (position, record) in calls.corroboration.iter().enumerate() {
        if !record.agrees && record.provider == CallProvider::Engine {
            pending
                .entry(record.static_target.clone())
                .or_default()
                .push(position);
        }
    }
    if pending.is_empty() {
        return calls;
    }
    'targets: for (target, records) in pending {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let Ok(identity) = SymbolIdentity::parse(target.as_str()) else {
            continue;
        };
        let Some(slot) = engines.engine_for(identity.language()) else {
            continue;
        };
        let provider = slot.edge_provider();
        let Some(report) = targets.read(reads, engines, &target, deadline).await else {
            continue;
        };
        for warning in &report.edge_warnings {
            if !calls.edge_warnings.contains(warning) {
                calls.edge_warnings.push(warning.clone());
            }
        }
        let positions = &report.positions;
        for position in records {
            if calls.corroboration.len() == TRAVERSAL_SYMBOLS_MAX {
                break 'targets;
            }
            let record = &calls.corroboration[position];
            let Some(observed) = reference_witness(positions, record) else {
                continue;
            };
            let confirmed = CallCorroboration {
                authored: record.authored.clone(),
                callee: TextRange {
                    start: record.callee.start,
                    end: record.callee.end,
                },
                observed: observed.clone(),
                static_target: target.clone(),
                engine_target: CallDestination::Local(target.clone()),
                provider: CallProvider::EngineReferences,
                agrees: true,
            };
            calls.push_local(
                seed,
                target.clone(),
                vec![confirmed.authored.clone()],
                TraversalDirection::Outgoing,
                provider,
            );
            calls.corroboration.push(confirmed);
        }
    }
    merge_call_neighbors(&mut calls.local);
    calls
}

fn reference_witness<'a>(
    positions: &'a BTreeMap<CallKey, SourceUnitSpan>,
    record: &CallCorroboration,
) -> Option<&'a SourceUnitSpan> {
    let start = (record.authored.unit.0.clone(), record.callee.start, 0);
    let end = (record.authored.unit.0.clone(), record.callee.end, u64::MAX);
    positions
        .range(start..=end)
        .map(|(_, source)| source)
        .find(|source| {
            source.range.start < source.range.end && source.range.end <= record.callee.end
        })
}

/// The calls one captured static pass established for a selected declaration.
pub(super) struct CapturedCalls<'read> {
    reads: &'read ReadService,
    direction: TraversalDirection,
    edges: Vec<&'read rift_index::Edge>,
    truncated: bool,
    edge_complete: bool,
}

impl<'read> CapturedCalls<'read> {
    pub(super) fn read(
        reads: &'read ReadService,
        seed: &CoreSymbolId,
        direction: TraversalDirection,
    ) -> Self {
        let store = reads.edges();
        let adjacent = match direction {
            TraversalDirection::Incoming => store.incoming(seed),
            TraversalDirection::Outgoing => store.outgoing(seed),
        };
        let mut calls = adjacent
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Calls && edge.static_callee().is_some());
        let edges = calls.by_ref().take(TRAVERSAL_SYMBOLS_MAX).collect();
        let edge_complete = super::edge_declarations(reads, seed).is_ok_and(|declarations| {
            !declarations.is_empty()
                && declarations.iter().all(|declaration| {
                    let range = (direction == TraversalDirection::Outgoing).then_some(
                        rift_syntax::ByteRange {
                            start: declaration.symbol.item_range.start,
                            end: declaration.symbol.item_range.end,
                        },
                    );
                    reads
                        .index()
                        .static_edge_calls_complete(declaration.file.path(), range)
                })
        });
        Self {
            reads,
            direction,
            edges,
            truncated: calls.next().is_some(),
            edge_complete,
        }
    }

    pub(super) const fn is_complete(&self) -> bool {
        self.edge_complete && !self.truncated
    }

    pub(super) fn answer(self) -> Option<GraphCalls> {
        (self.edge_complete || !self.edges.is_empty()).then(|| {
            let complete = self.is_complete();
            self.combine(GraphCalls {
                complete,
                ..GraphCalls::default()
            })
        })
    }

    /// Keeps authored external paths beside definition-site answers. Local captured
    /// calls still require the caller's independent hierarchy or reference evidence.
    pub(super) fn combine_external(mut self, engine: GraphCalls) -> GraphCalls {
        self.edges.retain(|edge| {
            let neighbor = match self.direction {
                TraversalDirection::Incoming => edge.from(),
                TraversalDirection::Outgoing => edge.to(),
            };
            rift_protocol::identity::SymbolIdentity::parse(neighbor.as_str())
                .is_ok_and(|identity| !identity.owner().is_local())
        });
        self.combine(engine)
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
            // A static edge to a dependency or standard-library symbol names the spelled
            // public path. An engine that names a dependency declaration at the same call
            // supplies the defining identity. A Rust trait call can name the interface
            // at the site while the engine names the method in a local implementation.
            let external = rift_protocol::identity::SymbolIdentity::parse(neighbor.as_str())
                .is_ok_and(|identity| !identity.owner().is_local());
            if external
                && (comparison.at(&span, callee).any(|(_, destinations)| {
                    destinations.iter().any(|(_, destination)| {
                        matches!(destination, CallDestination::Dependency(_))
                    })
                }) || self.direction == TraversalDirection::Outgoing
                    && comparison.local_at(&span, callee).is_some_and(|method| {
                        rust_trait_method(self.reads, &neighbor, method) == Some(true)
                    }))
            {
                engine.edges.push(edge.to_protocol());
                engine.local.push((neighbor, vec![span]));
                continue;
            }
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
            engine.edges.push(edge.to_protocol());
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
        engine.edges.retain(|edge| {
            edge.provider == rift_protocol::read::EdgeProvider::Static
                || edge
                    .occurrence
                    .as_ref()
                    .is_none_or(|span| !removed.contains(&call_key(span)))
        });
        merge_call_neighbors(&mut engine.local);
        engine
    }
}

/// Whether a local Rust method implements the trait member named at a call site.
fn rust_trait_method(reads: &ReadService, target: &SymbolId, method: &SymbolId) -> Option<bool> {
    let identity = SymbolIdentity::parse(method.as_str()).ok()?;
    if !identity.owner().is_local() || identity.language().name != "rust" {
        return None;
    }
    let (trait_id, trait_method) = target.as_str().rsplit_once('/')?;
    let (owner, method_name) = method.as_str().rsplit_once('/')?;
    if trait_method != method_name {
        return None;
    }
    let owner = CoreSymbolId::new(owner).ok()?;
    let method = CoreSymbolId::new(method.as_str()).ok()?;
    let (declarations, total) = reads.index().symbols_by_identity_page(&method, 0, 1).ok()?;
    let [declaration] = declarations.as_slice() else {
        return None;
    };
    if total != 1 {
        return None;
    }
    let implementation = declaration
        .file
        .syntax()
        .static_facts()?
        .inherent_impls()
        .iter()
        .filter(|item| item.range.encloses(declaration.symbol.item_range))
        .min_by_key(|item| item.range.end - item.range.start)?;
    let unit = rift_core::SourceUnitId::parse(
        crate::read::source_unit_id(declaration.file.path()).as_str(),
    )
    .ok()?;
    Some(
        reads
            .edges()
            .outgoing(&owner)
            .iter()
            .take(TRAVERSAL_SYMBOLS_MAX)
            .any(|edge| {
                let location = edge.occurrence();
                edge.kind() == EdgeKind::Implements
                    && edge.to().as_str() == trait_id
                    && location.unit() == &unit
                    && implementation.range.start <= location.range().start()
                    && location.range().end() <= implementation.type_path.start
            }),
    )
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

    fn local_at(&self, span: &SourceUnitSpan, callee: rift_syntax::ByteRange) -> Option<&SymbolId> {
        let mut observed = self.at(span, callee).flat_map(|(_, destinations)| {
            destinations
                .iter()
                .filter_map(|(_, destination)| match destination {
                    CallDestination::Local(id) => Some(id),
                    CallDestination::Dependency(_) => None,
                })
        });
        let first = observed.next()?;
        observed.all(|id| id == first).then_some(first)
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
                if records.len() == TRAVERSAL_SYMBOLS_MAX {
                    return true;
                }
                records.push(CallCorroboration {
                    authored: span.clone(),
                    callee: TextRange {
                        start: callee.start,
                        end: callee.end,
                    },
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
    fn reference_witness_excludes_arguments_and_other_source_units() {
        let text = "leaf(argument)";
        let unit = super::SourceUnitId("rift://source/project/lib.rs".to_owned());
        let span = |unit: super::SourceUnitId, start, end| super::SourceUnitSpan {
            unit,
            range: super::TextRange { start, end },
        };
        let target = SymbolId("rift://symbol/local/rust/app/leaf".to_owned());
        let record = super::CallCorroboration {
            authored: span(unit.clone(), 0, text.len() as u64),
            callee: super::TextRange { start: 0, end: 4 },
            observed: span(unit.clone(), 0, 4),
            static_target: target.clone(),
            engine_target: super::CallDestination::Local(target),
            provider: super::CallProvider::Engine,
            agrees: false,
        };
        for (source, expected) in [
            (span(unit.clone(), 0, 4), true),
            (span(unit.clone(), 5, text.len() as u64 - 1), false),
            (span(unit.clone(), 0, 5), false),
            (span(unit, 4, 4), false),
            (
                span(
                    super::SourceUnitId("rift://source/project/other.rs".to_owned()),
                    0,
                    4,
                ),
                false,
            ),
        ] {
            let positions = std::collections::BTreeMap::from([(super::call_key(&source), source)]);
            assert_eq!(
                super::reference_witness(&positions, &record).is_some(),
                expected
            );
        }
    }

    #[test]
    fn test_edge_static_calls_answer_complete_and_empty_adjacency() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(
            directory.path().join("entry.mjs"),
            "export function root() { leaf(); }\nfunction leaf() {}\n",
        )
        .expect("source");
        let reads = super::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("captured workspace");
        for (name, direction, count) in [
            ("root", super::TraversalDirection::Outgoing, 1),
            ("leaf", super::TraversalDirection::Incoming, 1),
            ("leaf", super::TraversalDirection::Outgoing, 0),
        ] {
            let seed = super::CoreSymbolId::new(format!(
                "rift://symbol/local/javascript/.%2Fentry.mjs/{name}"
            ))
            .expect("identity");
            let answer = super::CapturedCalls::read(&reads, &seed, direction)
                .answer()
                .expect("static answer");
            assert!(answer.complete);
            assert_eq!(answer.local.len(), count);
        }
    }

    #[test]
    fn test_edge_static_completeness_tracks_unresolved_calls_by_direction() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(directory.path().join("entry.mjs"), "export function root() { leaf(); }\nfunction leaf() {}\nfunction unknown(callback) { callback(); }\n").expect("source");
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
            .expect("identity")
        };
        let root = super::CapturedCalls::read(
            &reads,
            &identity("root"),
            super::TraversalDirection::Outgoing,
        )
        .answer()
        .expect("captured call");
        assert!(root.complete);
        let incoming = super::CapturedCalls::read(
            &reads,
            &identity("leaf"),
            super::TraversalDirection::Incoming,
        )
        .answer()
        .expect("captured caller");
        assert!(!incoming.complete);
        assert!(
            super::CapturedCalls::read(
                &reads,
                &identity("unknown"),
                super::TraversalDirection::Outgoing
            )
            .answer()
            .is_none()
        );
    }

    #[test]
    fn test_edge_incoming_completeness_covers_javascript_and_typescript_sources() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(
            directory.path().join("entry.mjs"),
            "export function root() { leaf(); }\nfunction leaf() {}\n",
        )
        .expect("source");
        std::fs::write(
            directory.path().join("unknown.tsx"),
            "export function unknown(callback: () => void) { callback(); }\n",
        )
        .expect("source");
        let reads = super::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("captured workspace");
        let leaf = super::CoreSymbolId::new("rift://symbol/local/javascript/.%2Fentry.mjs/leaf")
            .expect("identity");
        let answer = super::CapturedCalls::read(&reads, &leaf, super::TraversalDirection::Incoming)
            .answer()
            .expect("captured edge");
        assert!(!answer.complete);
    }

    #[test]
    fn incoming_engine_group_preserves_the_authored_initializer_caller() {
        // A value initializer's call belongs to the callable that holds it; an arrow function
        // initializer is a callable of its own.
        for (source, static_owner) in [
            (
                "function callee() {}\nexport function outer() { const token = callee(); return token; }\nfunction other() {}\n",
                "/outer",
            ),
            (
                "function callee() {}\nexport function outer() { const token = () => callee(); return token; }\nfunction other() {}\n",
                "/token",
            ),
        ] {
            assert_initializer_caller(source, static_owner);
        }
    }

    /// The static caller of `callee()` ends with `static_owner`, and an engine caller agrees
    /// only when it names `outer`, the declaration that holds the initializer.
    fn assert_initializer_caller(source: &str, static_owner: &str) {
        use rift_protocol::read::{TextRange, TraversalDirection};

        let directory = tempfile::tempdir().expect("workspace");
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
        assert_eq!(initializer == outer, static_owner == "/outer");
        assert!(
            initializer.as_str().ends_with(static_owner),
            "{initializer:?}"
        );
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
    fn rust_trait_call_keeps_its_external_path_and_the_confirmed_implementation() {
        use rift_protocol::read::{EdgeProvider, TraversalDirection};

        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::create_dir(directory.path().join("src")).expect("sources");
        std::fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Shape;\nimpl Default for Shape { fn default() -> Self { Shape } }\n\
             pub struct Other;\nimpl Other { pub fn default() -> Self { Other } }\n\
             pub fn make() -> Shape { Default::default() }\n",
        )
        .expect("source");
        let reads = super::ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("captured workspace");
        let caller = SymbolId("rift://symbol/local/rust/fixture/make".to_owned());
        let caller_id = super::CoreSymbolId::new(caller.as_str()).expect("caller identity");
        let capture =
            || super::CapturedCalls::read(&reads, &caller_id, TraversalDirection::Outgoing);
        let captured = capture();
        let [authored] = captured.edges.as_slice() else {
            panic!("expected one authored trait call");
        };
        let authored = authored.to_protocol();
        let occurrence = authored.occurrence.clone().expect("call site");
        let shape = SymbolId("rift://symbol/local/rust/fixture/Shape/default".to_owned());
        let other = SymbolId("rift://symbol/local/rust/fixture/Other/default".to_owned());
        for (targets, complete) in [
            (vec![shape.clone()], true),
            (vec![other.clone()], false),
            (vec![shape.clone(), other], false),
        ] {
            let mut definitions = super::GraphCalls {
                complete: true,
                ..super::GraphCalls::default()
            };
            for target in targets {
                definitions.push_local(
                    &caller,
                    target,
                    vec![occurrence.clone()],
                    TraversalDirection::Outgoing,
                    EdgeProvider::Lsp,
                );
            }
            let calls = capture().combine_external(definitions);
            assert_eq!(calls.complete, complete);
            if complete {
                assert_eq!(calls.edges.len(), 2);
                assert!(calls.edges.contains(&authored));
                assert!(calls.edges.iter().any(|edge| {
                    edge.to == shape
                        && edge.provider == EdgeProvider::Lsp
                        && edge.occurrence.as_ref() == Some(&occurrence)
                }));
            } else {
                assert!(calls.edges.is_empty());
            }
        }
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
