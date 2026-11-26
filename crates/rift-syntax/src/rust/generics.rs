//! Direct uses of generic parameters in authored type annotations.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;

use crate::{ByteRange, StaticFacts, StaticRustGenericReference, SyntaxElement, SyntaxSymbol};

pub(super) fn references(
    source: &str,
    elements: &[SyntaxElement],
    symbols: &[SyntaxSymbol],
    facts: &StaticFacts,
) -> Vec<StaticRustGenericReference> {
    let declarations = symbols
        .iter()
        .filter(|symbol| symbol.is_rust_generic_parameter())
        .filter_map(|symbol| symbol.name_range.map(|range| (range, symbol)))
        .collect::<BTreeMap<_, _>>();
    if declarations.is_empty() {
        return Vec::new();
    }
    let mut bindings = BTreeMap::new();
    for element in elements {
        let Some(symbol) = declarations.get(&element.range) else {
            continue;
        };
        let Some(parameter) = element.parent.and_then(|parent| elements.get(parent)) else {
            continue;
        };
        if parameter.kind != symbol.kind {
            continue;
        }
        let Some(owner) = parameter
            .parent
            .and_then(|parent| elements.get(parent))
            .and_then(|parameters| parameters.parent)
        else {
            continue;
        };
        bindings.insert((owner, symbol.kind, symbol.name.as_str()), element.range);
    }

    let types = facts
        .references()
        .iter()
        .filter(|reference| reference.role == ReferenceRole::Type)
        .map(|reference| reference.target)
        .collect::<BTreeSet<_>>();
    elements
        .iter()
        .filter(|element| !declarations.contains_key(&element.range))
        .filter_map(|element| {
            let parent = elements.get(element.parent?)?;
            let (kind, role) = reference_kind(element, parent.kind, &types)?;
            let name = read(source, element.range)?;
            let binding = binding(elements, element.parent, &bindings, kind, name)?;
            Some(StaticRustGenericReference {
                range: element.range,
                binding,
                role,
            })
        })
        .collect()
}

fn reference_kind(
    element: &SyntaxElement,
    parent: &str,
    types: &BTreeSet<ByteRange>,
) -> Option<(&'static str, ReferenceRole)> {
    match element.kind {
        "type_identifier" if types.contains(&element.range) => {
            Some(("type_parameter", ReferenceRole::Type))
        }
        "identifier" if parent == "array_type" => Some(("const_parameter", ReferenceRole::Read)),
        "lifetime"
            if matches!(
                parent,
                "reference_type"
                    | "type_arguments"
                    | "trait_bounds"
                    | "where_predicate"
                    | "bounded_type"
            ) =>
        {
            Some(("lifetime_parameter", ReferenceRole::Type))
        }
        _ => None,
    }
}

fn binding(
    elements: &[SyntaxElement],
    mut parent: Option<usize>,
    bindings: &BTreeMap<(usize, &str, &str), ByteRange>,
    kind: &str,
    name: &str,
) -> Option<ByteRange> {
    while let Some(index) = parent {
        // Each step moves to a recorded parent within the syntax depth bound.
        let item = elements.get(index)?;
        if let Some(range) = bindings.get(&(index, kind, name)) {
            return Some(*range);
        }
        match item.kind {
            "function_item"
            | "function_signature_item"
            | "associated_type"
            | "type_item"
            | "const_item" => {
                let list = elements.get(item.parent?)?;
                if list.kind != "declaration_list"
                    || !matches!(elements.get(list.parent?)?.kind, "impl_item" | "trait_item")
                {
                    return None;
                }
            }
            "impl_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "trait_item"
            | "mod_item"
            | "static_item"
            | "for_lifetimes"
            | "higher_ranked_trait_bound"
            | "block" => return None,
            _ => {}
        }
        parent = item.parent;
    }
    None
}

fn read(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
