//! Physical return ownership for separately placed private functions.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::{identity::SymbolIdentity, read::SymbolKind};
use rift_syntax::{ByteRange, ScopedIdentityAnchors, StaticFacts, SyntaxSymbol};

use super::{SelectedFile, binding_name};

struct ReturnedOwner<'source> {
    declaration: &'source SyntaxSymbol,
    identity: SymbolIdentity,
    nested: bool,
}

/// Gives each returned key the identity of its already placed private function.
pub(super) fn place(
    file: &SelectedFile<'_>,
    anchors: &BTreeMap<String, SymbolId>,
    scoped: &mut ScopedIdentityAnchors,
) {
    if file.syntax.language().name != "javascript"
        || file.syntax.has_errors()
        || file.syntax.left_out_declaration_count() != 0
    {
        return;
    }
    let Some(facts) = file.syntax.static_facts() else {
        return;
    };
    let mut owners = checked_returns(file, facts, scoped);
    if owners.is_empty() {
        return;
    }
    let physical = facts
        .body_members()
        .iter()
        .map(|member| (member.member, member))
        .collect::<BTreeMap<_, _>>();
    let mut occupied = anchors
        .values()
        .chain(scoped.values().flat_map(BTreeMap::values))
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut written = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for property in file.syntax.symbols() {
        let Some(member) = physical.get(&property.item_range) else {
            continue;
        };
        if !authored_property(file.source, property) {
            continue;
        }
        *counts
            .entry((member.body, property.name.as_str()))
            .or_insert(0_usize) += 1;
        written.insert(property.item_range, (property, *member));
    }
    // A parent's written pair starts before every member of its literal.
    for (property, member) in written.into_values() {
        let Some(owner) = owners.get(&member.body) else {
            continue;
        };
        if property.container.as_deref() != Some(owner.declaration.qualified_name.as_str())
            || (owner.nested && member.holder != owner.declaration.item_range)
            || counts.get(&(member.body, property.name.as_str())) != Some(&1)
            || scoped
                .get(&property.qualified_name)
                .is_some_and(|group| group.contains_key(&property.declaration_ordinal))
        {
            continue;
        }
        let mut path = owner.identity.qualified_path().to_vec();
        path.push(property.name.clone());
        let Ok(identity) = SymbolIdentity::new(
            owner.identity.owner().clone(),
            owner.identity.language().clone(),
            path,
        ) else {
            continue;
        };
        let Ok(id) = SymbolId::new(identity.wire_identity()) else {
            continue;
        };
        if !occupied.insert(id.clone()) {
            continue;
        }
        scoped
            .entry(property.qualified_name.clone())
            .or_default()
            .insert(property.declaration_ordinal, id);
        if let Some(body) = property.body_range
            && property.syntax_element_kind == Some("pair")
            && property.item_range.encloses(body)
            && text(file.source, body)
                .is_some_and(|value| value.starts_with('{') && value.ends_with('}'))
        {
            owners.insert(
                body,
                ReturnedOwner {
                    declaration: property,
                    identity,
                    nested: true,
                },
            );
        }
    }
}

fn checked_returns<'source>(
    file: &SelectedFile<'source>,
    facts: &StaticFacts,
    scoped: &ScopedIdentityAnchors,
) -> BTreeMap<ByteRange, ReturnedOwner<'source>> {
    let physical = file
        .syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            if symbol.syntax_element_kind != Some("function_declaration")
                || !symbol.kinds.contains(&SymbolKind::Callable)
                || symbol.kinds.contains(&SymbolKind::Public)
            {
                return None;
            }
            let scope = scoped
                .get(&symbol.qualified_name)?
                .get(&symbol.declaration_ordinal)?;
            Some((symbol.name_range?, (symbol, scope)))
        })
        .collect::<BTreeMap<_, _>>();
    checked_returned_bodies(facts)
        .into_iter()
        .filter_map(|(name, body)| {
            let (symbol, id) = *physical.get(&name)?;
            if !symbol.item_range.encloses(body) {
                return None;
            }
            let identity = SymbolIdentity::parse(id.as_str()).ok()?;
            Some((
                body,
                ReturnedOwner {
                    declaration: symbol,
                    identity,
                    nested: false,
                },
            ))
        })
        .collect()
}

/// Matches each authored function name with its single complete returned literal.
pub(super) fn checked_returned_bodies(facts: &StaticFacts) -> BTreeMap<ByteRange, ByteRange> {
    let mut bodies = BTreeMap::new();
    for &(name, returned) in facts.returned_objects() {
        bodies
            .entry(name)
            .and_modify(|prior| *prior = None)
            .or_insert(returned);
    }
    bodies
        .into_iter()
        .filter_map(|(name, body)| Some((name, body?)))
        .collect()
}

fn authored_property(source: &str, property: &SyntaxSymbol) -> bool {
    property.kind == "property"
        && matches!(
            property.syntax_element_kind,
            Some("pair" | "shorthand_property_identifier")
        )
        && !property.name.starts_with('[')
        && property.name_range.is_some_and(|name| {
            text(source, name) == Some(property.name.as_str())
                || binding_name(source, name).is_some_and(|key| key == property.name)
        })
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
