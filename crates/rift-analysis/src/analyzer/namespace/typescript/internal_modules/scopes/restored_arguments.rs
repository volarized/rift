//! Literal argument members of an independently placed JavaScript function.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::{identity::SymbolIdentity, read::SymbolKind};
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBodyMember, StaticCall, StaticCallee, StaticFacts,
    SyntaxSymbol,
};

use super::{
    FileScopes, SelectedFile, literal_property_identity, local_group_identity,
    ordinary_private_property,
};
use crate::EnclosingDefinitions;

impl<'source> FileScopes<'source> {
    /// Places exact object members without requiring their Call target.
    pub(super) fn place_restored_argument_members(
        &self,
        file: &SelectedFile<'source>,
        facts: &'source StaticFacts,
        restored: &BTreeMap<&str, &SyntaxSymbol>,
        anchors: &BTreeMap<String, SymbolId>,
        fallback: &ScopedIdentityAnchors,
        placed: &mut ScopedIdentityAnchors,
    ) {
        let owners = restored
            .iter()
            .filter_map(|(&name, &symbol)| {
                (symbol.syntax_element_kind == Some("function_declaration")
                    && anchors.contains_key(name))
                .then_some((symbol.item_range, symbol))
            })
            .collect::<BTreeMap<_, _>>();
        if owners.is_empty() {
            return;
        }
        let scope = ArgumentScopes::new(file, facts);
        let mut members = BTreeMap::new();
        let mut counts = BTreeMap::new();
        for property in file.syntax.symbols() {
            if let Some(&member) = self.body_members.get(&property.item_range)
                && anchors.contains_key(&property.qualified_name)
                && ordinary_private_property(property, file.source)
            {
                *counts
                    .entry((member.body, property.name.as_str()))
                    .or_insert(0_usize) += 1;
                members.insert(property.item_range, (property, member));
            }
        }
        let mut nested = BTreeMap::<ByteRange, (&SyntaxSymbol, SymbolId)>::new();
        let mut occupied = anchors
            .values()
            .chain(placed.values().flat_map(|ordinals| ordinals.values()))
            .chain(fallback.values().flat_map(|ordinals| ordinals.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (property, member) in members.into_values() {
            if counts.get(&(member.body, property.name.as_str())) != Some(&1) {
                continue;
            }
            let current = placed
                .get(&property.qualified_name)
                .and_then(|ordinals| ordinals.get(&property.declaration_ordinal));
            let identity = if let Some((parent, owner)) = nested.get(&member.body) {
                if member.holder != parent.item_range
                    || property.container.as_deref() != Some(parent.qualified_name.as_str())
                {
                    continue;
                }
                current.cloned().or_else(|| {
                    let owner = SymbolIdentity::parse(owner.as_str()).ok()?;
                    literal_property_identity(&owner, &property.name)
                })
            } else {
                let Some(function) = scope.call_owner(member, &owners) else {
                    continue;
                };
                if property.container.as_deref() != Some(function.qualified_name.as_str()) {
                    continue;
                }
                current.cloned().or_else(|| {
                    local_group_identity(
                        anchors.get(&property.qualified_name)?,
                        property.declaration_ordinal,
                    )
                })
            };
            let Some(identity) = identity else {
                continue;
            };
            if current.is_none() {
                if !occupied.insert(identity.clone()) {
                    continue;
                }
                let group = placed.entry(property.qualified_name.clone()).or_default();
                if let Some(binding) = fallback.get(&property.qualified_name) {
                    group.extend(binding.clone());
                }
                group.insert(property.declaration_ordinal, identity.clone());
            }
            if let Some(body) = property.body_range
                && property.syntax_element_kind == Some("pair")
                && property.item_range.encloses(body)
                && source_text(file.source, body)
                    .is_some_and(|value| value.starts_with('{') && value.ends_with('}'))
            {
                nested.insert(body, (property, identity));
            }
        }
    }
}

struct ArgumentScopes<'source> {
    file: &'source str,
    calls: BTreeMap<u64, Option<&'source StaticCall>>,
    functions: EnclosingDefinitions<ByteRange>,
}

impl<'source> ArgumentScopes<'source> {
    fn new(file: &SelectedFile<'source>, facts: &'source StaticFacts) -> Self {
        let retained_elsewhere = facts
            .destructured()
            .iter()
            .map(|(_, _, call)| *call)
            .collect::<BTreeSet<_>>();
        let mut calls = BTreeMap::new();
        for call in facts.calls() {
            if !matches!(call.callee, StaticCallee::Member { .. })
                || retained_elsewhere.contains(&call.range)
            {
                continue;
            }
            calls
                .entry(call.range.end)
                .and_modify(|old| *old = None)
                .or_insert(Some(call));
        }
        let functions = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.kinds.contains(&SymbolKind::Callable))
            .map(|symbol| symbol.item_range)
            .chain(facts.function_values().iter().map(|(span, _)| *span))
            .chain(facts.static_blocks().iter().copied())
            .collect::<BTreeSet<_>>();
        Self {
            file: file.source,
            calls,
            functions: EnclosingDefinitions::new(
                functions
                    .into_iter()
                    .map(|span| (span.start, span.end, span)),
            ),
        }
    }

    fn call_owner<'candidates>(
        &self,
        member: &StaticBodyMember,
        owners: &BTreeMap<ByteRange, &'candidates SyntaxSymbol>,
    ) -> Option<&'candidates SyntaxSymbol> {
        let call = self.calls.get(&member.holder.end)?.as_ref()?;
        let span = self.functions.resolve(call.range.start, call.range.end)?;
        let owner = owners.get(span).copied()?;
        (owner.body_range?.encloses(call.range)
            && call.callee_range.end <= member.holder.start
            && call.range.encloses(member.holder)
            && member.holder.encloses(member.body)
            && member.body.encloses(member.member)
            && source_text(self.file, member.holder)
                .is_some_and(|s| s.starts_with('(') && s.ends_with(')')))
        .then_some(owner)
    }
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
