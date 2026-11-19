//! Indexes ECMAScript function values from captured syntax ranges.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticFacts, SyntaxSymbol};

use super::javascript::Scopes;
use super::{File, Work};

impl<'source> Scopes<'source> {
    pub(super) fn new(file: &File<'source>, work: &mut Work) -> Option<Self> {
        let captured = FunctionScopes::capture(file.facts, work)?;
        let mut classes = Vec::new();
        let mut definitions = Vec::new();
        let mut callables = Vec::new();
        let mut fields = BTreeSet::new();
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            let entry = (symbol.range.start, symbol.range.end, symbol);
            if symbol.kind == "class"
                && let Some(body) = symbol.body_range
            {
                classes.push((body.start, body.end, symbol));
            }
            if let Some(range) = captured.callable(symbol) {
                callables.push((range.start, range.end, symbol));
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
            classes: crate::EnclosingDefinitions::new(classes),
            definitions: crate::EnclosingDefinitions::new(definitions),
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

    fn callable(&self, symbol: &SyntaxSymbol) -> Option<ByteRange> {
        if matches!(symbol.kind, "function" | "method") {
            return Some(symbol.range);
        }
        if !symbol.kinds.contains(&SymbolKind::Callable) {
            return None;
        }
        self.values.get(&symbol.body_range?).copied()
    }
}
