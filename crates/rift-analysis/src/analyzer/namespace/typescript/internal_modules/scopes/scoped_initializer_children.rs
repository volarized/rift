//! Initializer children of already separated JavaScript local bindings.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_syntax::{ByteRange, ScopedIdentityAnchors, StaticBindingKind, SyntaxSymbol};

use super::{FileScopes, SelectedFile, literal_property_identity, ordinary_private_property};

impl<'source> FileScopes<'source> {
    /// Retains literal members under the distinct physical variable that wrote them.
    pub(in super::super) fn place_scoped_initializer_children(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        if file.syntax.language().name != "javascript" {
            return;
        }
        let Some(facts) = file.syntax.static_facts().filter(|_| !scoped.is_empty()) else {
            return;
        };
        let mut occupied = anchors
            .values()
            .chain(scoped.values().flat_map(BTreeMap::values))
            .cloned()
            .collect::<BTreeSet<_>>();
        let local_names = facts
            .bindings()
            .iter()
            .filter(|b| b.kind == StaticBindingKind::Local)
            .map(|b| (b.name, b.scope))
            .collect::<BTreeSet<_>>();
        let mut holders = BTreeMap::<ByteRange, (&SyntaxSymbol, ByteRange, SymbolIdentity)>::new();
        for symbol in file.syntax.symbols() {
            if symbol.kind != "variable"
                || symbol.syntax_element_kind != Some("variable_declarator")
            {
                continue;
            }
            let Some(physical) = scoped
                .get(&symbol.qualified_name)
                .and_then(|ids| ids.get(&symbol.declaration_ordinal))
            else {
                continue;
            };
            let Some(((name, scope), body)) = symbol
                .name_range
                .zip(self.checked_lexical_scope(file, symbol))
                .zip(symbol.body_range)
            else {
                continue;
            };
            let Ok(identity) = SymbolIdentity::parse(physical.as_str()) else {
                continue;
            };
            if scope.encloses(symbol.item_range)
                && symbol.item_range.encloses(body)
                && local_names.contains(&(name, scope))
            {
                holders.insert(symbol.item_range, (symbol, body, identity));
            }
        }
        let mut members = Vec::new();
        let mut counts = BTreeMap::new();
        for property in file.syntax.symbols() {
            let Some(span) = self.body_members.get(&property.item_range) else {
                continue;
            };
            let Some((binding, body, identity)) = holders.get(&span.holder) else {
                continue;
            };
            if *body == span.body
                && property.container.as_deref() == Some(binding.qualified_name.as_str())
                && ordinary_private_property(property, file.source)
            {
                let key = (span.body, property.name.as_str());
                *counts.entry(key).or_insert(0_usize) += 1;
                members.push((property, key, identity));
            }
        }
        let mut placed_parents = BTreeSet::new();
        for (property, key, owner) in members {
            if counts.get(&key) != Some(&1)
                || scoped
                    .get(&property.qualified_name)
                    .is_some_and(|ids| ids.contains_key(&property.declaration_ordinal))
            {
                continue;
            }
            let Some(id) = literal_property_identity(owner, &property.name) else {
                continue;
            };
            if occupied.insert(id.clone()) {
                placed_parents.insert(property.item_range);
                scoped
                    .entry(property.qualified_name.clone())
                    .or_default()
                    .insert(property.declaration_ordinal, id);
            }
        }
        if !placed_parents.is_empty() {
            self.scope_nested_properties(
                file,
                withheld,
                anchors,
                &mut occupied,
                scoped,
                Some(&mut placed_parents),
            );
        }
    }
}
