//! Module declarations and object keys that share a spelling.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_syntax::{
    ByteRange, JavaScriptSyntaxProvider, ScopedIdentityAnchors, StaticBindingKind, SyntaxSymbol,
};

use super::{FileScopes, SelectedFile, binding_name};

impl<'source> FileScopes<'source> {
    /// Keeps a checked lexical declaration and its class members at their written ordinals.
    pub(in crate::analyzer::namespace::typescript::internal_modules) fn place_disjoint_module_bindings(
        &self,
        file: &SelectedFile<'source>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        placed: &mut ScopedIdentityAnchors,
    ) {
        let Some(facts) = file.syntax.static_facts().filter(|_| {
            !repeated.is_empty()
                && file.syntax.language().name == "javascript"
                && !file.syntax.has_errors()
                && file.syntax.left_out_declaration_count() == 0
        }) else {
            return;
        };
        let symbols = file.syntax.symbols();
        let mut groups = BTreeMap::<&str, Vec<_>>::new();
        for symbol in symbols {
            let name = symbol.qualified_name.as_str();
            if repeated.contains(name) && anchors.contains_key(name) && !placed.contains_key(name) {
                groups.entry(name).or_default().push(symbol);
            }
        }
        let written = facts
            .assignments()
            .iter()
            .filter_map(|&range| binding_name(file.source, range))
            .collect::<BTreeSet<_>>();
        let bound = facts
            .bindings()
            .iter()
            .filter(|binding| {
                binding.scope == self.source
                    && matches!(
                        binding.kind,
                        StaticBindingKind::Local | StaticBindingKind::JavaScriptClassValue { .. }
                    )
            })
            .map(|binding| binding.name)
            .collect::<BTreeSet<_>>();
        let candidates = groups
            .into_iter()
            .filter_map(|(name, group)| {
                if written.contains(name) {
                    return None;
                }
                self.disjoint_group(&group, &bound)
                    .map(|(binding, properties)| (name, binding, properties))
            })
            .collect::<Vec<_>>();
        let selected = candidates
            .iter()
            .flat_map(|(_, _, properties)| properties.iter().copied())
            .collect::<BTreeSet<_>>();
        let checked = JavaScriptSyntaxProvider::module_disjoint_object_members(
            file.source,
            file.syntax.syntax_limits().unwrap_or_default(),
            &selected,
        );
        let mut classes = BTreeMap::new();
        for (name, binding, properties) in candidates {
            if !properties.iter().all(|range| checked.contains(range)) {
                continue;
            }
            let Some(id) = anchors.get(name) else {
                continue;
            };
            placed.insert(
                name.to_owned(),
                BTreeMap::from([(binding.declaration_ordinal, id.clone())]),
            );
            if binding.syntax_element_kind == Some("class_declaration") {
                classes.insert(name, binding.item_range);
            }
        }
        self.place_disjoint_class_members(symbols, &classes, repeated, anchors, placed);
    }

    fn disjoint_group<'symbol>(
        &self,
        group: &[&'symbol SyntaxSymbol],
        bound: &BTreeSet<ByteRange>,
    ) -> Option<(&'symbol SyntaxSymbol, Vec<ByteRange>)> {
        let mut owner = None;
        let mut properties = BTreeMap::new();
        for &symbol in group {
            if symbol.container.is_some() {
                return None;
            }
            let kind = symbol.syntax_element_kind?;
            match kind {
                "class_declaration" | "function_declaration" | "variable_declarator"
                    if bound.contains(&symbol.name_range?)
                        && !symbol.is_ecmascript_package_binding() =>
                {
                    if owner.replace(symbol).is_some() {
                        return None;
                    }
                }
                "method_definition" | "pair"
                    if self.body_members.contains_key(&symbol.item_range) =>
                {
                    if properties.insert(kind, symbol.item_range).is_some() {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        (!properties.is_empty()).then_some((owner?, properties.into_values().collect()))
    }

    fn place_disjoint_class_members(
        &self,
        symbols: &[SyntaxSymbol],
        classes: &BTreeMap<&str, ByteRange>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        placed: &mut ScopedIdentityAnchors,
    ) {
        for symbol in symbols {
            let Some(class) = symbol
                .container
                .as_deref()
                .and_then(|name| classes.get(name).copied())
            else {
                continue;
            };
            let name = &symbol.qualified_name;
            let member = symbol.kind == "constructor"
                && symbol.syntax_element_kind == Some("method_definition")
                || symbol.name_range.is_some()
                    && !symbol.name.starts_with('[')
                    && matches!(
                        symbol.syntax_element_kind,
                        Some("method_definition" | "field_definition")
                    );
            if member
                && !repeated.contains(name.as_str())
                && !placed.contains_key(name)
                && self
                    .body_members
                    .get(&symbol.item_range)
                    .is_some_and(|row| row.body == class)
                && let Some(id) = anchors.get(name)
            {
                placed.insert(
                    name.clone(),
                    BTreeMap::from([(symbol.declaration_ordinal, id.clone())]),
                );
            }
        }
    }
}
