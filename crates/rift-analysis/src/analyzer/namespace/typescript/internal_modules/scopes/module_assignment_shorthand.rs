//! Placement of shorthand written to an earlier module variable.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticAssignedObject, StaticBinding, StaticBindingKind,
    StaticBodyMember, StaticFacts, SyntaxSymbol,
};

use super::{
    FileScopes, LiteralOwners, Scope, SelectedFile, literal_property_identity,
    local_group_identity, ordinary_private_property, private_variable_declaration,
};

impl<'source> FileScopes<'source> {
    /// Separates a literal initializer and the shorthand that reads it.
    pub(in crate::analyzer::namespace::typescript::internal_modules) fn place_module_assignment_shorthand(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        placed: &mut ScopedIdentityAnchors,
    ) {
        let Some(facts) = file.syntax.static_facts().filter(|facts| {
            !facts.assigned_objects().is_empty()
                && !withheld.is_empty()
                && file.syntax.language().name == "javascript"
                && !file.syntax.has_errors()
                && file.syntax.left_out_declaration_count() == 0
        }) else {
            return;
        };
        let assigned = facts
            .assigned_objects()
            .iter()
            .map(|write| (write.object, write))
            .collect::<BTreeMap<_, _>>();
        let mut repeated = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if withheld.contains(name) && anchors.contains_key(name) && !placed.contains_key(name) {
                repeated.entry(name).or_default().push(symbol);
            }
        }
        let candidates = repeated
            .into_iter()
            .filter_map(|(name, rows)| {
                let [local, property] = rows.as_slice() else {
                    return None;
                };
                let member = self.body_members.get(&property.item_range)?;
                (is_plain_variable(local)
                    && local.body_range.is_some()
                    && property.syntax_element_kind == Some("shorthand_property_identifier")
                    && ordinary_private_property(property, file.source)
                    && local.item_range.end <= member.body.start)
                    .then_some((name, *local, *property, *assigned.get(&member.body)?))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return;
        }
        let bindings = capture_bindings(facts, file.source);
        let declarations = super::unique_declarations(file.syntax.symbols());
        let members = initializer_members(file.syntax.symbols(), &self.body_members);
        let owners = LiteralOwners::new(file, facts, anchors);
        let mut occupied = anchors
            .values()
            .chain(placed.values().flat_map(|rows| rows.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (name, local, property, write) in candidates {
            let Some((_, Some(callable))) = owners
                .enclosing
                .resolve(write.target.start, write.object.end)
            else {
                continue;
            };
            if callable.syntax_element_kind != Some("function_declaration")
                || !callable
                    .body_range
                    .is_some_and(|body| body.encloses(write.block))
                || self.of(property, file) != Scope::Body(write.object, false)
                || local.container.as_deref() != Some(callable.qualified_name.as_str())
                || property.container != local.container
                || !checked_read(file, local, property, write, &bindings)
            {
                continue;
            }
            let receiver =
                checked_module_receiver(file, facts, self.source, write, &declarations, &bindings)
                    .and_then(|name| anchors.get(name))
                    .and_then(|id| SymbolIdentity::parse(id.as_str()).ok());
            let Some((local_id, property_id)) =
                local_group_identity(&anchors[name], local.declaration_ordinal)
                    .zip(receiver.and_then(|id| literal_property_identity(&id, &property.name)))
            else {
                continue;
            };
            if occupied.contains(&local_id) || occupied.contains(&property_id) {
                continue;
            }
            occupied.extend([local_id.clone(), property_id.clone()]);
            if let Some(children) = members.get(&local.item_range) {
                place_initializer_members(file, local, &local_id, children, &mut occupied, placed);
            }
            placed.insert(
                name.to_owned(),
                BTreeMap::from([
                    (local.declaration_ordinal, local_id),
                    (property.declaration_ordinal, property_id),
                ]),
            );
        }
    }
}

fn initializer_members<'source>(
    symbols: &'source [SyntaxSymbol],
    body_members: &BTreeMap<ByteRange, &'source StaticBodyMember>,
) -> BTreeMap<ByteRange, Vec<(&'source SyntaxSymbol, &'source StaticBodyMember)>> {
    let mut holders = BTreeMap::<_, Vec<_>>::new();
    for symbol in symbols {
        if let Some(&member) = body_members.get(&symbol.item_range) {
            holders
                .entry(member.holder)
                .or_default()
                .push((symbol, member));
        }
    }
    holders
}

fn place_initializer_members(
    file: &SelectedFile<'_>,
    local: &SyntaxSymbol,
    identity: &SymbolId,
    members: &[(&SyntaxSymbol, &StaticBodyMember)],
    occupied: &mut BTreeSet<SymbolId>,
    placed: &mut ScopedIdentityAnchors,
) {
    let Ok(owner) = SymbolIdentity::parse(identity.as_str()) else {
        return;
    };
    let mut unique = BTreeMap::<&str, Option<&SyntaxSymbol>>::new();
    for &(symbol, member) in members {
        if local.body_range == Some(member.body)
            && symbol.container.as_deref() == Some(local.qualified_name.as_str())
            && ordinary_private_property(symbol, file.source)
        {
            unique
                .entry(&symbol.name)
                .and_modify(|prior| *prior = None)
                .or_insert(Some(symbol));
        }
    }
    for member in unique.into_values().flatten() {
        if placed
            .get(&member.qualified_name)
            .is_some_and(|ordinals| ordinals.contains_key(&member.declaration_ordinal))
        {
            continue;
        }
        let Some(identity) = literal_property_identity(&owner, &member.name) else {
            continue;
        };
        if occupied.insert(identity.clone()) {
            placed
                .entry(member.qualified_name.clone())
                .or_default()
                .insert(member.declaration_ordinal, identity);
        }
    }
}

fn capture_bindings<'source>(
    facts: &'source StaticFacts,
    source: &'source str,
) -> BTreeMap<&'source str, Vec<&'source StaticBinding>> {
    let mut bindings = BTreeMap::<&str, Vec<_>>::new();
    for binding in facts.bindings() {
        if let Some(name) = source_text(source, binding.name) {
            bindings.entry(name).or_default().push(binding);
        }
    }
    bindings
}

fn checked_module_receiver<'source>(
    file: &SelectedFile<'source>,
    facts: &StaticFacts,
    module: ByteRange,
    write: &StaticAssignedObject,
    declarations: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    bindings: &BTreeMap<&str, Vec<&StaticBinding>>,
) -> Option<&'source str> {
    let name = source_text(file.source, write.target)?;
    let declaration = declarations.get(name).copied().flatten()?;
    let binding = checked_at(bindings.get(name)?, write.target)?;
    (is_plain_variable(declaration)
        && declaration.container.is_none()
        && declaration.body_range.is_none()
        && declaration.name_range == Some(binding.name)
        && module == binding.scope
        && facts
            .assignments()
            .iter()
            .filter(|range| source_text(file.source, **range) == Some(name))
            .take(2)
            .count()
            == 1)
        .then_some(declaration.qualified_name.as_str())
}

fn checked_read(
    file: &SelectedFile<'_>,
    local: &SyntaxSymbol,
    property: &SyntaxSymbol,
    write: &StaticAssignedObject,
    bindings: &BTreeMap<&str, Vec<&StaticBinding>>,
) -> bool {
    let Some(binding) = property
        .name_range
        .and_then(|name| source_text(file.source, name).zip(Some(name)))
        .and_then(|(name, site)| checked_at(bindings.get(name)?, site))
    else {
        return false;
    };
    let Some(body) = local.body_range else {
        return false;
    };
    local.name_range == Some(binding.name)
        && binding.scope.encloses(write.block)
        && local.item_range.encloses(body)
        && source_text(file.source, body)
            .is_some_and(|value| value.starts_with('{') && value.ends_with('}'))
        && source_text(file.source, binding.name) == Some(property.name.as_str())
}

fn checked_at<'facts>(
    bindings: &[&'facts StaticBinding],
    site: ByteRange,
) -> Option<&'facts StaticBinding> {
    let mut visible = bindings
        .iter()
        .copied()
        .filter(|binding| binding.scope.encloses(site));
    let binding = visible
        .clone()
        .min_by_key(|binding| binding.scope.end - binding.scope.start)?;
    (binding.kind == StaticBindingKind::Local
        && binding.available_from <= site.start
        && binding.name.end <= site.start
        && !visible.any(|other| other.scope == binding.scope && other.name != binding.name))
    .then_some(binding)
}

fn is_plain_variable(symbol: &SyntaxSymbol) -> bool {
    symbol.syntax_element_kind == Some("variable_declarator")
        && private_variable_declaration(symbol)
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
