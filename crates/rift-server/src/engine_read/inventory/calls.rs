//! Combines declaration call hierarchy with authored call-site definitions.

use std::collections::BTreeMap;

use rift_protocol::read::{SourceUnitSpan, SymbolId};

use super::GraphCalls;

/// Keeps authored spans for calls both operations resolved, and adds native-only calls.
pub(super) fn merge_calls(
    hierarchy: Vec<(SymbolId, GraphCalls)>,
    definitions: Vec<(SymbolId, GraphCalls)>,
) -> Vec<(SymbolId, GraphCalls)> {
    let sites: BTreeMap<_, _> = definitions
        .iter()
        .map(|(seed, calls)| (seed.clone(), DefinitionSites::new(calls)))
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

struct DefinitionSites {
    units: BTreeMap<String, CallIntervals>,
}

impl DefinitionSites {
    fn new(calls: &GraphCalls) -> Self {
        let mut units = BTreeMap::<_, Vec<_>>::new();
        for (_, ranges) in &calls.local {
            for source in ranges {
                units
                    .entry(source.unit.as_str().to_owned())
                    .or_default()
                    .push((source.range.start, source.range.end));
            }
        }
        Self {
            units: units
                .into_iter()
                .map(|(unit, ranges)| (unit, CallIntervals::new(ranges)))
                .collect(),
        }
    }

    fn remove_repeated(&self, calls: &mut GraphCalls) {
        let retain = |ranges: &mut Vec<SourceUnitSpan>| {
            let located = !ranges.is_empty();
            ranges.retain(|source| {
                !self
                    .units
                    .get(source.unit.as_str())
                    .is_some_and(|spans| spans.contains(source))
            });
            !located || !ranges.is_empty()
        };
        calls.local.retain_mut(|(_, ranges)| retain(ranges));
        calls.packages.retain_mut(|(_, ranges)| retain(ranges));
    }
}

struct CallIntervals {
    spans: Vec<(u64, u64)>,
}

impl CallIntervals {
    fn new(mut spans: Vec<(u64, u64)>) -> Self {
        spans.sort_unstable();
        let mut end = 0;
        for span in &mut spans {
            end = end.max(span.1);
            span.1 = end;
        }
        Self { spans }
    }

    fn contains(&self, source: &SourceUnitSpan) -> bool {
        self.spans
            .partition_point(|(start, _)| *start <= source.range.start)
            .checked_sub(1)
            .is_some_and(|position| self.spans[position].1 >= source.range.end)
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
            vec![(caller, definition)],
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
        let merged = merge_calls(vec![(caller.clone(), native)], vec![(caller, defined)]);
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
