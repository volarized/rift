//! Combines declaration call hierarchy with authored call-site definitions.

use std::collections::BTreeMap;

use rift_protocol::read::{SourceUnitSpan, SymbolId};

use super::GraphCalls;

/// Keeps authored spans for calls both operations resolved, and adds native-only calls.
pub(super) fn merge_calls(
    hierarchy: Vec<(SymbolId, GraphCalls)>,
    definitions: Vec<(SymbolId, GraphCalls)>,
    locations: Vec<(SymbolId, SourceUnitSpan, u64)>,
) -> Vec<(SymbolId, GraphCalls)> {
    let mut authored = BTreeMap::<_, BTreeMap<_, _>>::new();
    for (caller, source, at) in locations {
        authored
            .entry(caller)
            .or_default()
            .insert((source.unit.0, source.range.start, source.range.end), at);
    }
    let sites: BTreeMap<_, _> = definitions
        .iter()
        .map(|(seed, calls)| {
            (
                seed.clone(),
                DefinitionSites::new(calls, authored.get(seed)),
            )
        })
        .collect();
    let mut answers: BTreeMap<_, _> = definitions.into_iter().collect();
    for (seed, mut native) in hierarchy {
        if let Some(sites) = sites.get(&seed) {
            sites.remove_repeated(&mut native);
        }
        match answers.entry(seed) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(native);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().merge(native);
            }
        }
    }
    answers.into_iter().collect()
}

type AuthoredSites = BTreeMap<(String, u64, u64), u64>;

struct DefinitionSites {
    units: BTreeMap<String, BTreeMap<u64, Vec<(u64, u64)>>>,
}

impl DefinitionSites {
    fn new(calls: &GraphCalls, authored: Option<&AuthoredSites>) -> Self {
        let mut units = BTreeMap::<_, BTreeMap<_, Vec<_>>>::new();
        for (_, ranges) in &calls.local {
            for source in ranges {
                let key = (source.unit.0.clone(), source.range.start, source.range.end);
                let Some(at) = authored.and_then(|authored| authored.get(&key)) else {
                    continue;
                };
                units
                    .entry(source.unit.0.clone())
                    .or_default()
                    .entry(*at)
                    .or_default()
                    .push((source.range.start, source.range.end));
            }
        }
        Self { units }
    }

    fn remove_repeated(&self, calls: &mut GraphCalls) {
        let retain = |ranges: &mut Vec<SourceUnitSpan>| {
            let located = !ranges.is_empty();
            ranges.retain(|source| {
                !self.units.get(source.unit.as_str()).is_some_and(|points| {
                    points
                        .range(source.range.start..source.range.end)
                        .any(|(_, spans)| {
                            spans.iter().any(|(start, end)| {
                                *start <= source.range.start && source.range.end <= *end
                            })
                        })
                })
            });
            !located || !ranges.is_empty()
        };
        calls.local.retain_mut(|(_, ranges)| retain(ranges));
        calls.packages.retain_mut(|(_, ranges)| retain(ranges));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::read::{SourceUnitId, TextRange};

    fn span(start: u64, end: u64) -> SourceUnitSpan {
        SourceUnitSpan {
            unit: SourceUnitId("rift://source/project/lib.rs".to_owned()),
            range: TextRange { start, end },
        }
    }

    #[test]
    fn native_calls_inside_arguments_survive_a_resolved_outer_call() {
        let caller = SymbolId("rift://symbol/local/rust/test/caller".to_owned());
        let outer = SymbolId("rift://symbol/local/rust/test/outer".to_owned());
        let expanded = SymbolId("rift://symbol/local/rust/test/expanded".to_owned());
        let definition = GraphCalls {
            local: vec![(outer.clone(), vec![span(10, 30)])],
            complete: true,
            ..GraphCalls::default()
        };
        let hierarchy = GraphCalls {
            local: vec![
                (outer.clone(), vec![span(10, 13)]),
                (expanded.clone(), vec![span(20, 25)]),
            ],
            complete: true,
            ..GraphCalls::default()
        };
        let merged = merge_calls(
            vec![(caller.clone(), hierarchy)],
            vec![(caller.clone(), definition)],
            vec![(caller, span(10, 30), 12)],
        );
        assert!(merged[0].1.local.contains(&(outer, vec![span(10, 30)])));
        assert!(merged[0].1.local.contains(&(expanded, vec![span(20, 25)])));
    }

    #[test]
    fn definition_target_replaces_a_different_hierarchy_target_at_the_same_site() {
        let caller = SymbolId("rift://symbol/local/typescript/test/caller".to_owned());
        let local = SymbolId("rift://symbol/local/typescript/test/local".to_owned());
        let helper = SymbolId("rift://symbol/local/typescript/test/helper".to_owned());
        let definition = GraphCalls {
            local: vec![(local.clone(), vec![span(10, 30)])],
            complete: true,
            ..GraphCalls::default()
        };
        let hierarchy = GraphCalls {
            local: vec![(helper, vec![span(10, 15)])],
            complete: true,
            ..GraphCalls::default()
        };
        let merged = merge_calls(
            vec![(caller.clone(), hierarchy)],
            vec![(caller.clone(), definition)],
            vec![(caller, span(10, 30), 12)],
        );
        assert_eq!(merged[0].1.local, vec![(local, vec![span(10, 30)])]);
    }

    #[test]
    fn authored_calls_keep_nested_sites_and_add_native_macro_sites() {
        let caller = SymbolId("rift://symbol/local/rust/test/caller".to_owned());
        let target = SymbolId("rift://symbol/local/rust/test/target".to_owned());
        let defined = GraphCalls {
            local: vec![(target.clone(), vec![span(10, 30), span(15, 20)])],
            complete: true,
            ..GraphCalls::default()
        };
        let native = GraphCalls {
            local: vec![(target, vec![span(10, 13), span(15, 18), span(40, 45)])],
            complete: false,
            ..GraphCalls::default()
        };
        let merged = merge_calls(
            vec![(caller.clone(), native)],
            vec![(caller.clone(), defined)],
            vec![
                (caller.clone(), span(10, 30), 12),
                (caller, span(15, 20), 17),
            ],
        );
        let [(_, calls)] = merged.as_slice() else {
            panic!("one caller")
        };
        let mut ranges: Vec<_> = calls
            .local
            .iter()
            .flat_map(|(_, ranges)| {
                ranges
                    .iter()
                    .map(|source| (source.range.start, source.range.end))
            })
            .collect();
        ranges.sort_unstable();
        assert_eq!(ranges, [(10, 30), (15, 20), (40, 45)]);
        assert!(!calls.complete);
    }
}
