//! Parameter declarations inside written TypeScript type signatures.

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::Node;

use crate::extract::{self, Visited};
use crate::{ByteRange, StaticBindingKind, StaticFacts, SyntaxSymbol};

pub(super) fn callable_parameter_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
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

pub(super) fn callable_rest_parameter_name<'tree>(
    visited: Visited<'_, 'tree>,
) -> Option<Node<'tree>> {
    let name = visited.syntax_element();
    if name.kind() != "identifier" || name.is_missing() {
        return None;
    }
    let rest = visited.parent()?;
    if rest.syntax_element().kind() != "rest_pattern" {
        return None;
    }
    let parameter = rest.parent()?;
    let list = parameter.parent()?;
    let scope = list.parent()?;
    checked_rest_scope(
        name,
        rest.syntax_element(),
        parameter.syntax_element(),
        list.syntax_element(),
        scope.syntax_element(),
    )?;
    Some(name)
}

fn callable_rest_scope(name: Node<'_>) -> Option<Node<'_>> {
    if name.kind() != "identifier" || name.is_missing() {
        return None;
    }
    let rest = name.parent().filter(|node| node.kind() == "rest_pattern")?;
    let parameter = rest.parent()?;
    let list = parameter.parent()?;
    checked_rest_scope(name, rest, parameter, list, list.parent()?)
}

fn checked_rest_scope<'tree>(
    name: Node<'tree>,
    rest: Node<'tree>,
    parameter: Node<'tree>,
    list: Node<'tree>,
    scope: Node<'tree>,
) -> Option<Node<'tree>> {
    if rest.named_child(0) != Some(name)
        || parameter.kind() != "required_parameter"
        || parameter.child_by_field_name("pattern") != Some(rest)
        || parameter.child_by_field_name("value").is_some()
        || parameter.has_error()
        || !checked_callable_type_scope(list, scope)
    {
        return None;
    }
    // Only the remaining siblings of a checked rest formal can disqualify it.
    let mut following = parameter.next_named_sibling();
    while let Some(next) = following {
        if !next.is_extra() {
            return None;
        }
        following = next.next_named_sibling();
    }
    Some(scope)
}

pub(super) fn callable_pattern_parameter_name<'tree>(
    visited: Visited<'_, 'tree>,
) -> Option<Node<'tree>> {
    let name = visited.syntax_element();
    if name.is_missing() {
        return None;
    }
    let pattern = match name.kind() {
        "shorthand_property_identifier_pattern" => visited.parent()?,
        "identifier" => {
            let pair = visited.parent()?;
            if !checked_renamed_pattern_binding(name, pair.syntax_element()) {
                return None;
            }
            pair.parent()?
        }
        _ => return None,
    };
    if pattern.syntax_element().kind() != "object_pattern" {
        return None;
    }
    let parameter = pattern.parent()?;
    let list = parameter.parent()?;
    let scope = list.parent()?;
    checked_pattern_scope(
        pattern.syntax_element(),
        parameter.syntax_element(),
        list.syntax_element(),
        scope.syntax_element(),
    )?;
    Some(name)
}

/// A simple written binding of the object pattern of one exact callable type.
fn callable_pattern_scope(name: Node<'_>) -> Option<Node<'_>> {
    if name.is_missing() {
        return None;
    }
    let pattern = match name.kind() {
        "shorthand_property_identifier_pattern" => name.parent()?,
        "identifier" => name
            .parent()
            .filter(|pair| checked_renamed_pattern_binding(name, *pair))?
            .parent()?,
        _ => return None,
    };
    if pattern.kind() != "object_pattern" {
        return None;
    }
    let parameter = pattern.parent()?;
    let list = parameter.parent()?;
    checked_pattern_scope(pattern, parameter, list, list.parent()?)
}

fn checked_renamed_pattern_binding(name: Node<'_>, pair: Node<'_>) -> bool {
    pair.kind() == "pair_pattern"
        && pair.child_by_field_name("value") == Some(name)
        && pair
            .child_by_field_name("key")
            .is_some_and(|key| key.kind() == "property_identifier" && !key.is_missing())
}

fn checked_pattern_scope<'tree>(
    pattern: Node<'tree>,
    parameter: Node<'tree>,
    list: Node<'tree>,
    scope: Node<'tree>,
) -> Option<Node<'tree>> {
    (matches!(
        parameter.kind(),
        "required_parameter" | "optional_parameter"
    ) && parameter.child_by_field_name("pattern") == Some(pattern)
        && parameter.child_by_field_name("value").is_none()
        && checked_callable_type_scope(list, scope)
        && !parameter.has_error())
    .then_some(scope)
}

fn callable_type_scope(parameter: Node<'_>) -> Option<Node<'_>> {
    if !matches!(
        parameter.kind(),
        "required_parameter" | "optional_parameter"
    ) {
        return None;
    }
    let list = parameter.parent()?;
    let scope = list.parent()?;
    (list.kind() == "formal_parameters" && callable_signature(scope.kind())).then_some(scope)
}

fn checked_callable_type_scope(list: Node<'_>, scope: Node<'_>) -> bool {
    list.kind() == "formal_parameters"
        && callable_signature(scope.kind())
        && scope.child_by_field_name("parameters") == Some(list)
}

pub(super) fn callable_signature(kind: &str) -> bool {
    matches!(
        kind,
        "function_type" | "call_signature" | "construct_signature" | "constructor_type"
    )
}

pub(super) fn append_callable_types(
    root: Node<'_>,
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
    facts: &StaticFacts,
) {
    if parameters.is_empty() {
        return;
    }
    let sites = facts
        .parameters()
        .iter()
        .map(|site| (site.name, site.function))
        .collect::<BTreeMap<_, _>>();
    let pattern_bindings = parameters
        .iter()
        .any(|parameter| {
            parameter.is_typescript_callable_type_rest_parameter()
                || parameter.is_typescript_callable_type_pattern_parameter()
        })
        .then(|| {
            facts
                .bindings()
                .iter()
                .filter(|site| site.kind == StaticBindingKind::Shadow)
                .map(|binding| (binding.name, binding.scope))
                .collect::<BTreeSet<_>>()
        });
    append_scoped(symbols, parameters, "$signature", |name| {
        let written = written_name(root, name)?;
        if let Some(scope) =
            callable_rest_scope(written).or_else(|| callable_pattern_scope(written))
        {
            let scope = extract::byte_range(scope).ok()?;
            return pattern_bindings
                .as_ref()?
                .contains(&(name, scope))
                .then_some(scope);
        }
        let scope = extract::byte_range(callable_type_scope(written.parent()?)?).ok()?;
        (sites.get(&name) == Some(&scope)).then_some(scope)
    });
}

pub(super) fn append_generic_parameters(
    root: Node<'_>,
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
    facts: &StaticFacts,
    label: &str,
    accepts_owner: impl Fn(&str) -> bool,
) {
    if parameters.is_empty() {
        return;
    }
    let captured = facts
        .bindings()
        .iter()
        .filter(|site| site.kind == StaticBindingKind::Shadow)
        .map(|site| (site.name, site.scope))
        .collect::<BTreeSet<_>>();
    append_scoped(symbols, parameters, label, |name| {
        let written = written_name(root, name)?;
        let parameter = written.parent()?;
        let list = parameter.parent()?;
        let owner = list.parent()?;
        if parameter.child_by_field_name("name") != Some(written)
            || owner.child_by_field_name("type_parameters") != Some(list)
            || !accepts_owner(owner.kind())
        {
            return None;
        }
        let scope = extract::byte_range(owner).ok()?;
        captured.contains(&(name, scope)).then_some(scope)
    });
}

pub(super) fn append_inferred_parameters(
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: &[SyntaxSymbol],
    facts: &StaticFacts,
) {
    if parameters.is_empty() {
        return;
    }
    let bindings = facts
        .bindings()
        .iter()
        .filter(|binding| binding.kind == StaticBindingKind::TypeScriptInference)
        .map(|binding| (binding.name, (binding.available_from, binding.scope)))
        .collect::<BTreeMap<_, _>>();
    let mut counts = BTreeMap::new();
    let mut placed = BTreeMap::<&str, Vec<ByteRange>>::new();
    for symbol in symbols
        .iter()
        .filter(|symbol| !symbol.is_ecmascript_package_binding())
    {
        placed
            .entry(&symbol.qualified_name)
            .or_default()
            .push(symbol.item_range);
    }
    for parameter in parameters {
        if let Some((branch, _)) = parameter.name_range.and_then(|name| bindings.get(&name)) {
            *counts
                .entry((*branch, parameter.name.as_str()))
                .or_insert(0_usize) += 1;
        }
    }
    let parameters = parameters
        .iter()
        .filter_map(|parameter| {
            let (branch, scope) = bindings.get(&parameter.name_range?)?;
            if counts.get(&(*branch, parameter.name.as_str())) != Some(&1) {
                return None;
            }
            let mut owner = parameter.container.as_deref()?;
            loop {
                if placed.get(owner).is_some_and(|ranges| {
                    ranges
                        .iter()
                        .filter(|region| region.encloses(*scope))
                        .count()
                        == 1
                }) {
                    let mut placed = parameter.clone();
                    placed.container = Some(owner.to_owned());
                    return Some(placed);
                }
                owner = owner.rsplit_once('.')?.0;
            }
        })
        .collect();
    append_scoped(symbols, parameters, "$conditional", |name| {
        bindings.get(&name).map(|(_, scope)| *scope)
    });
}

/// Name written in an index signature. A mapped type names its parameter in a separate rule.
pub(super) fn index_parameter_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
    index_name(visited.syntax_element())
}

fn index_name(scope: Node<'_>) -> Option<Node<'_>> {
    if scope.kind() != "index_signature" || scope.has_error() {
        return None;
    }
    scope
        .child_by_field_name("name")
        .filter(|name| name.kind() == "identifier" && !name.is_missing())
}

pub(super) fn append_indexes(
    root: Node<'_>,
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
) {
    append_scoped(symbols, parameters, "$index", |name| {
        let written = written_name(root, name)?;
        let scope = written.parent()?;
        if index_name(scope) != Some(written) {
            return None;
        }
        extract::byte_range(scope).ok()
    });
}

/// The written binder of a single mapped object body.
pub(super) fn mapped_parameter_name(clause: Node<'_>) -> Option<Node<'_>> {
    if clause.kind() != "mapped_type_clause" || clause.has_error() {
        return None;
    }
    let index = clause
        .parent()
        .filter(|node| node.kind() == "index_signature")?;
    let object = index.parent().filter(|node| node.kind() == "object_type")?;
    if object.has_error() || index.child_by_field_name("type").is_none() {
        return None;
    }
    let mut cursor = object.walk();
    if !object
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra())
        .eq(std::iter::once(index))
    {
        return None;
    }
    clause
        .child_by_field_name("name")
        .filter(|name| name.kind() == "type_identifier" && !name.is_missing())
}

/// Places mapped parameters in the written type alias that contains each body.
pub(super) fn append_mapped_types(
    root: Node<'_>,
    symbols: &mut Vec<SyntaxSymbol>,
    parameters: Vec<SyntaxSymbol>,
) {
    if parameters.is_empty() {
        return;
    }
    let mut names = BTreeMap::<_, usize>::new();
    for symbol in symbols
        .iter()
        .filter(|symbol| !symbol.is_ecmascript_package_binding())
    {
        *names.entry(symbol.qualified_name.as_str()).or_default() += 1;
    }
    let owners = symbols
        .iter()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("type_alias_declaration")
                && names.get(symbol.qualified_name.as_str()) == Some(&1)
        })
        .map(|symbol| (symbol.item_range, symbol.qualified_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut scopes = BTreeMap::new();
    let admitted = parameters
        .into_iter()
        .filter_map(|mut parameter| {
            let range = parameter.name_range?;
            let written = written_name(root, range)?;
            let clause = written.parent()?;
            if mapped_parameter_name(clause) != Some(written) {
                return None;
            }
            let body = clause.parent()?.parent()?;
            let alias = enclosing_alias(body)?;
            let owner = owners.get(&extract::byte_range(alias).ok()?)?;
            scopes.insert(range, extract::byte_range(body).ok()?);
            parameter.container = Some((*owner).to_owned());
            Some(parameter)
        })
        .collect();
    append_scoped(symbols, admitted, "$mapped", |name| {
        scopes.get(&name).copied()
    });
}

fn enclosing_alias(mut node: Node<'_>) -> Option<Node<'_>> {
    // The syntax walk admitted the complete body and its ancestor depths.
    while let Some(parent) = node.parent() {
        match parent.kind() {
            "type_alias_declaration" => return Some(parent),
            "class_declaration"
            | "interface_declaration"
            | "function_signature"
            | "function_declaration" => return None,
            _ => node = parent,
        }
    }
    None
}

fn append_scoped(
    symbols: &mut Vec<SyntaxSymbol>,
    mut parameters: Vec<SyntaxSymbol>,
    label: &str,
    scope_of: impl Fn(ByteRange) -> Option<ByteRange>,
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
    let names = symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut next = BTreeMap::<String, usize>::new();
    let mut scopes = BTreeMap::<ByteRange, usize>::new();
    let mut retained = BTreeMap::<String, Option<SyntaxSymbol>>::new();
    parameters.sort_unstable_by_key(|parameter| {
        (
            parameter.is_typescript_callable_type_pattern_parameter(),
            parameter.is_typescript_callable_type_rest_parameter(),
            parameter.item_range.start,
        )
    });
    for mut parameter in parameters {
        let Some(name) = parameter.name_range else {
            continue;
        };
        let Some(container) = parameter.container.as_deref() else {
            continue;
        };
        let Some(scope) = scope_of(name) else {
            continue;
        };
        if owners
            .get(container)
            .is_none_or(|items| items.iter().filter(|item| item.encloses(scope)).count() != 1)
        {
            continue;
        }
        let ordinal = scopes.entry(scope).or_insert_with(|| {
            let ordinal = next.entry(container.to_owned()).or_default();
            let current = *ordinal;
            *ordinal += 1;
            current
        });
        let qualified = format!("{container}.{label}#{ordinal}.{}", parameter.name);
        if names.contains(qualified.as_str()) || !rift_core::is_portable_name(&qualified) {
            continue;
        }
        parameter.qualified_name.clone_from(&qualified);
        let pattern = parameter.is_typescript_callable_type_pattern_parameter();
        retained
            .entry(qualified)
            .and_modify(|entry| {
                if !pattern
                    || entry
                        .as_ref()
                        .is_some_and(SyntaxSymbol::is_typescript_callable_type_pattern_parameter)
                {
                    *entry = None;
                }
            })
            .or_insert(Some(parameter));
    }
    symbols.extend(retained.into_values().flatten());
}

fn written_name(root: Node<'_>, name: ByteRange) -> Option<Node<'_>> {
    let start = usize::try_from(name.start).ok()?;
    let end = usize::try_from(name.end).ok()?;
    let written = root.descendant_for_byte_range(start, end)?;
    if written.byte_range() != (start..end) {
        return None;
    }
    Some(written)
}
