//! Parameter declarations with a named owner and a captured lexical scope.

use std::collections::{BTreeMap, BTreeSet};

use crate::{StaticBindingKind, StaticFacts, SyntaxSymbol};

pub(super) fn append_under_named_owner(
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
    facts: &StaticFacts,
    accepts_owner: impl Fn(&str) -> bool,
) {
    if parameters.is_empty() {
        return;
    }
    let mut owners = BTreeMap::new();
    for symbol in symbols.iter() {
        owners
            .entry(symbol.qualified_name.as_str())
            .and_modify(|entry| *entry = None)
            .or_insert(
                symbol
                    .syntax_element_kind
                    .filter(|kind| accepts_owner(kind))
                    .map(|_| symbol.item_range),
            );
    }
    let mut counts = BTreeMap::new();
    for parameter in &parameters {
        *counts
            .entry(parameter.qualified_name.as_str())
            .or_insert(0_usize) += 1;
    }
    let checked = facts
        .bindings()
        .iter()
        .filter(|binding| binding.kind == StaticBindingKind::Shadow)
        .map(|binding| (binding.name, binding.scope))
        .collect::<BTreeSet<_>>();
    let retained = parameters
        .iter()
        .filter_map(|parameter| {
            let owner = *owners.get(parameter.container.as_deref()?)?.as_ref()?;
            let name = parameter.name_range?;
            (!owners.contains_key(parameter.qualified_name.as_str())
                && counts.get(parameter.qualified_name.as_str()) == Some(&1)
                && checked.contains(&(name, owner))
                && rift_core::is_portable_name(&parameter.qualified_name)
                && rift_core::is_portable_name(&parameter.name))
            .then_some(name)
        })
        .collect::<BTreeSet<_>>();
    symbols.extend(parameters.into_iter().filter(|parameter| {
        parameter
            .name_range
            .is_some_and(|name| retained.contains(&name))
    }));
}
