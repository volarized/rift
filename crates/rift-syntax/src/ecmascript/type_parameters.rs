//! Authored parameters of named TypeScript types.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::Visited;

use crate::{StaticBindingKind, StaticFacts, SyntaxSymbol};

use super::{
    ABSTRACT_CLASS_DECLARATION_KIND, CLASS_DECLARATION_KIND, INTERFACE_DECLARATION_KIND,
    TYPE_ALIAS_DECLARATION_KIND,
};

pub(super) fn declared_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
    let parameter = visited.syntax_element();
    if parameter.kind() != "type_parameter" || parameter.has_error() {
        return None;
    }
    let list = visited.parent()?.syntax_element();
    let owner = list.parent()?;
    if owner.child_by_field_name("type_parameters") != Some(list) || !structured(owner.kind()) {
        return None;
    }
    parameter
        .child_by_field_name("name")
        .filter(|name| name.kind() == "type_identifier" && !name.is_missing())
}

fn structured(kind: &str) -> bool {
    matches!(
        kind,
        ABSTRACT_CLASS_DECLARATION_KIND
            | CLASS_DECLARATION_KIND
            | INTERFACE_DECLARATION_KIND
            | TYPE_ALIAS_DECLARATION_KIND
    )
}

pub(super) fn append(
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
    facts: &StaticFacts,
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
                    .filter(|kind| structured(kind))
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
