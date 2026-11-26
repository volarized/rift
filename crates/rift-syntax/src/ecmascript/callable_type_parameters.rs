//! Value parameter declarations inside written TypeScript callable types.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::{self, Visited};
use crate::{ByteRange, StaticFacts, SyntaxSymbol};

pub(super) fn declared_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
    let parameter = visited.syntax_element();
    let scope = callable_type_scope(parameter)?;
    let list = visited.parent()?.syntax_element();
    if scope.child_by_field_name("parameters") != Some(list) || parameter.has_error() {
        return None;
    }
    parameter
        .child_by_field_name("pattern")
        .filter(|name| name.kind() == "identifier" && !name.is_missing())
}

fn callable_type_scope(parameter: Node<'_>) -> Option<Node<'_>> {
    if !matches!(
        parameter.kind(),
        "required_parameter" | "optional_parameter"
    ) {
        return None;
    }
    let list = parameter
        .parent()
        .filter(|node| node.kind() == "formal_parameters")?;
    let scope = list.parent()?;
    matches!(
        scope.kind(),
        "function_type" | "call_signature" | "construct_signature" | "constructor_type"
    )
    .then_some(scope)
}

pub(super) fn append(
    root: Node<'_>,
    symbols: &mut Vec<SyntaxSymbol>,
    mut parameters: Vec<SyntaxSymbol>,
    facts: &StaticFacts,
) {
    if parameters.is_empty() {
        return;
    }
    let mut owners = BTreeMap::<&str, Vec<ByteRange>>::new();
    for symbol in symbols.iter() {
        if !symbol.is_ecmascript_package_binding() {
            owners
                .entry(symbol.qualified_name.as_str())
                .or_default()
                .push(symbol.item_range);
        }
    }
    let sites = facts
        .parameters()
        .iter()
        .map(|site| (site.name, site.function))
        .collect::<BTreeMap<_, _>>();
    let names = symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut next = BTreeMap::<String, usize>::new();
    let mut scopes = BTreeMap::<ByteRange, usize>::new();
    let mut retained = BTreeMap::new();
    parameters.sort_unstable_by_key(|parameter| parameter.item_range.start);
    for mut parameter in parameters {
        let Some(name) = parameter.name_range else {
            continue;
        };
        let Some(container) = parameter.container.as_deref() else {
            continue;
        };
        let Some(scope) = written_scope(root, name) else {
            continue;
        };
        if owners
            .get(container)
            .is_none_or(|items| items.iter().filter(|item| item.encloses(scope)).count() != 1)
            || sites.get(&name) != Some(&scope)
        {
            continue;
        }
        let ordinal = scopes.entry(scope).or_insert_with(|| {
            let ordinal = next.entry(container.to_owned()).or_default();
            let current = *ordinal;
            *ordinal += 1;
            current
        });
        let qualified = format!("{container}.$signature#{ordinal}.{}", parameter.name);
        if names.contains(qualified.as_str()) || !rift_core::is_portable_name(&qualified) {
            continue;
        }
        parameter.qualified_name.clone_from(&qualified);
        retained
            .entry(qualified)
            .and_modify(|entry| *entry = None)
            .or_insert(Some(parameter));
    }
    symbols.extend(retained.into_values().flatten());
}

fn written_scope(root: Node<'_>, name: ByteRange) -> Option<ByteRange> {
    let start = usize::try_from(name.start).ok()?;
    let end = usize::try_from(name.end).ok()?;
    let written = root.descendant_for_byte_range(start, end)?;
    if written.byte_range() != (start..end) {
        return None;
    }
    let scope = callable_type_scope(written.parent()?)?;
    extract::byte_range(scope).ok()
}
