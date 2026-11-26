//! Physical value parameters of authored TypeScript overload declarations.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::{ByteRange, StaticFacts, SyntaxSymbol};

use super::OVERLOAD_PARAMETER_KIND;

pub(super) fn capture(
    symbols: &[SyntaxSymbol],
    parameters: &[SyntaxSymbol],
    facts: &StaticFacts,
    root: Node<'_>,
) -> Vec<SyntaxSymbol> {
    let mut declarations = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
    for symbol in symbols {
        declarations
            .entry(symbol.qualified_name.as_str())
            .or_default()
            .push(symbol);
    }
    let mut scopes = BTreeMap::new();
    for group in declarations.values().filter(|group| group.len() > 1) {
        let mut ordered = group.clone();
        ordered.sort_unstable_by_key(|declaration| declaration.item_range.start);
        let mut lexical = BTreeMap::<_, Vec<_>>::new();
        for (ordinal, declaration) in ordered.into_iter().enumerate() {
            if let Some(container) = callable_region(root, declaration) {
                lexical
                    .entry(container)
                    .or_default()
                    .push((declaration, ordinal));
            }
        }
        for overloads in lexical.values().filter(|group| is_overload_group(group)) {
            for &(declaration, ordinal) in overloads {
                if let Some(name) = declaration.name_range {
                    scopes.insert((name, declaration.item_range), (declaration, ordinal));
                }
            }
        }
    }
    let owners = facts
        .parameters()
        .iter()
        .filter_map(|site| {
            let (owner, ordinal) = scopes.get(&(site.owner?, site.function))?;
            Some((site.name, (*owner, *ordinal)))
        })
        .collect::<BTreeMap<ByteRange, _>>();
    let mut retained = BTreeMap::new();
    let occupied = declarations.keys().copied().collect::<BTreeSet<_>>();
    for parameter in parameters {
        let Some((owner, ordinal)) = parameter.name_range.and_then(|name| owners.get(&name)) else {
            continue;
        };
        let container = owner.qualified_name.as_str();
        if parameter.container.as_deref() != Some(container) {
            continue;
        }
        let qualified = format!("{container}.$overload#{ordinal}.{}", parameter.name);
        if occupied.contains(qualified.as_str()) || !rift_core::is_portable_name(&qualified) {
            continue;
        }
        retained
            .entry(qualified.clone())
            .and_modify(|row| *row = None)
            .or_insert_with(|| {
                let mut located = parameter.clone();
                located.kind = OVERLOAD_PARAMETER_KIND;
                located.qualified_name = qualified;
                Some(located)
            });
    }
    retained.into_values().flatten().collect()
}

fn is_overload_group(group: &[(&SyntaxSymbol, usize)]) -> bool {
    let [(first, _), _, ..] = group else {
        return false;
    };
    group
        .iter()
        .all(|(declaration, _)| declaration.kind == first.kind)
        && group
            .iter()
            .filter(|(declaration, _)| declaration.body_range.is_some())
            .count()
            <= 1
}

fn callable_region(root: Node<'_>, declaration: &SyntaxSymbol) -> Option<ByteRange> {
    let kind = declaration.syntax_element_kind?;
    if !matches!(
        kind,
        "function_declaration"
            | "function_signature"
            | "method_definition"
            | "method_signature"
            | "abstract_method_signature"
    ) {
        return None;
    }
    let start = usize::try_from(declaration.item_range.start).ok()?;
    let end = usize::try_from(declaration.item_range.end).ok()?;
    let node = root.named_descendant_for_byte_range(start, end)?;
    if node.kind() != kind || node.byte_range() != (start..end) {
        return None;
    }
    let mut container = node.parent()?;
    // These wrappers state visibility or declaration-only syntax, not a lexical scope.
    while matches!(container.kind(), "export_statement" | "ambient_declaration") {
        container = container.parent()?;
    }
    super::extract::byte_range(container).ok()
}
