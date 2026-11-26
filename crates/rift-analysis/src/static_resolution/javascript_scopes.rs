//! Indexes ECMAScript function values from captured syntax ranges.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticFacts, SyntaxSymbol};

use super::javascript::Scopes;
use super::{File, Work};

impl<'source> Scopes<'source> {
    pub(super) fn new(file: &File<'source>, work: &mut Work) -> Option<Self> {
        let captured = FunctionScopes::capture(file.facts, work)?;
        let value_ranges = crate::EnclosingDefinitions::new(
            captured
                .values
                .iter()
                .filter(|(value, range)| value == range)
                .map(|(_, range)| (range.start, range.end, *range))
                .collect::<Vec<_>>(),
        );
        let function_values = crate::EnclosingDefinitions::new(
            captured
                .values
                .values()
                .map(|range| (range.start, range.end, *range))
                .collect::<BTreeSet<_>>(),
        );
        let deferred = deferred_scopes(file, work)?;
        let mut classes = Vec::new();
        let mut definitions = Vec::new();
        let mut callables = Vec::new();
        let mut fields = BTreeSet::new();
        let mut values: BTreeMap<ByteRange, Option<&SyntaxSymbol>> = BTreeMap::new();
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            let entry = (symbol.range.start, symbol.range.end, symbol);
            let class_body = if symbol.kind == "class" {
                symbol.body_range
            } else {
                file.javascript_class_value_body(symbol)
            };
            if let Some(body) = class_body {
                classes.push((body.start, body.end, symbol));
            }
            if let Some(range) = captured.callable(symbol, &value_ranges) {
                callables.push((range.start, range.end, symbol));
                if captured.values.contains_key(&range) {
                    values
                        .entry(range)
                        .and_modify(|owner| *owner = None)
                        .or_insert(Some(symbol));
                }
                if symbol.kind == "field" {
                    fields.insert(range);
                }
            }
            definitions.push(entry);
        }
        let functions = captured
            .functions
            .into_iter()
            .filter(|range| !fields.contains(range))
            .map(|range| (range.start, range.end, range))
            .collect::<Vec<_>>();
        Some(Self {
            values,
            classes: crate::EnclosingDefinitions::new(classes),
            deferred,
            definitions: crate::EnclosingDefinitions::new(definitions),
            function_values,
            callables: crate::EnclosingDefinitions::new(callables),
            functions: crate::EnclosingDefinitions::new(functions),
            blocks: crate::EnclosingDefinitions::new(
                captured
                    .blocks
                    .into_iter()
                    .map(|range| (range.start, range.end, range))
                    .collect::<Vec<_>>(),
            ),
        })
    }
}

impl File<'_> {
    /// A held class body belongs to the exact physical variable that initializes it.
    pub(super) fn javascript_class_value_body(&self, symbol: &SyntaxSymbol) -> Option<ByteRange> {
        if symbol.syntax_element_kind != Some("variable_declarator") {
            return None;
        }
        let body = *self.javascript_class_bodies.get(&symbol.name_range?)?;
        symbol.body_range?.encloses(body).then_some(body)
    }
}

fn deferred_scopes(
    file: &File<'_>,
    work: &mut Work,
) -> Option<crate::EnclosingDefinitions<ByteRange>> {
    let mut callees = Vec::new();
    for call in file.facts.calls() {
        if !work.charge() {
            return None;
        }
        let range = call.callee_range;
        callees.push((range.start, range.end, range));
    }
    let immediate = crate::EnclosingDefinitions::new(callees);
    let mut functions = BTreeSet::new();
    for &(range, _) in file.facts.function_values() {
        if !work.charge() {
            return None;
        }
        functions.insert(range);
    }
    for symbol in file.document.facts.symbols() {
        if !work.charge() {
            return None;
        }
        if matches!(symbol.kind, "function" | "method")
            && let Some(body) = symbol.body_range
        {
            // Decorators and computed names execute before the function's parameters and body.
            let start = symbol
                .name_range
                .filter(|name| name.end <= body.start)
                .map_or(body.start, |name| name.end);
            functions.insert(ByteRange {
                start,
                end: body.end,
            });
        }
    }
    let spans = functions
        .into_iter()
        .filter(|range| immediate.resolve(range.start, range.end).is_none())
        .map(|range| (range.start, range.end, range))
        .collect::<Vec<_>>();
    Some(crate::EnclosingDefinitions::new(spans))
}

#[derive(Default)]
struct FunctionScopes {
    values: BTreeMap<ByteRange, ByteRange>,
    functions: Vec<ByteRange>,
    blocks: Vec<ByteRange>,
}

impl FunctionScopes {
    fn capture(facts: &StaticFacts, work: &mut Work) -> Option<Self> {
        let mut captured = Self::default();
        for &(range, value) in facts.function_values() {
            if !work.charge() {
                return None;
            }
            captured.values.insert(value, range);
        }
        for &range in facts.function_scopes() {
            if !work.charge() {
                return None;
            }
            captured.functions.push(range);
        }
        for &range in facts.static_blocks() {
            if !work.charge() {
                return None;
            }
            captured.blocks.push(range);
        }
        Some(captured)
    }

    fn callable(
        &self,
        symbol: &SyntaxSymbol,
        values: &crate::EnclosingDefinitions<ByteRange>,
    ) -> Option<ByteRange> {
        if matches!(symbol.kind, "function" | "method") {
            if symbol.syntax_element_kind == Some("assignment_expression") {
                let body = symbol.body_range?;
                return values
                    .resolve(body.start, body.end)
                    .copied()
                    .filter(|range| symbol.item_range.encloses(*range));
            }
            return Some(symbol.range);
        }
        if !symbol.kinds.contains(&SymbolKind::Callable) {
            return None;
        }
        self.values.get(&symbol.body_range?).copied()
    }
}
