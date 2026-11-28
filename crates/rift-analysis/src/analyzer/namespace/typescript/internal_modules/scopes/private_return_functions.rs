//! Separately checked private return functions in secondary ECMAScript modules.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ScopedIdentityAnchors, StaticBindingKind, StaticCallee, StaticFunctionContext, SyntaxSymbol,
};

use super::super::super::scoped_returns;
use super::{FileScopes, SelectedFile, local_group_identity, private_local_declaration};
use crate::EnclosingDefinitions;

impl<'source> FileScopes<'source> {
    /// Places a repeated function only when every sibling has a distinct checked callback scope.
    pub(in super::super) fn withheld_private_return_functions(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        if withheld.is_empty()
            || file.syntax.language().name != "javascript"
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return BTreeMap::new();
        }
        let Some(facts) = file.syntax.static_facts() else {
            return BTreeMap::new();
        };
        let calls = facts
            .calls()
            .iter()
            .filter(|call| {
                matches!(
                    call.callee,
                    StaticCallee::Direct | StaticCallee::Member { .. }
                )
            })
            .map(|call| (call.range, call.callee_range))
            .collect::<BTreeMap<_, _>>();
        let callbacks = EnclosingDefinitions::new(facts.function_contexts().iter().filter_map(
            |&(function, context)| {
                let StaticFunctionContext::Argument { call, .. } = context else {
                    return None;
                };
                let callee = calls.get(&call)?;
                (callee.end <= function.start && call.encloses(function)).then_some((
                    function.start,
                    function.end,
                    function,
                ))
            },
        ));
        let returns = scoped_returns::checked_returned_bodies(facts);
        let bindings = facts
            .bindings()
            .iter()
            .filter(|binding| binding.kind == StaticBindingKind::Local)
            .map(|binding| (binding.name, binding.scope))
            .collect::<BTreeSet<_>>();
        let mut groups = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for declaration in file.syntax.symbols() {
            let name = declaration.qualified_name.as_str();
            if withheld.contains(name) && anchors.contains_key(name) {
                groups.entry(name).or_default().push(declaration);
            }
        }
        let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
        groups
            .into_iter()
            .filter_map(|(name, group)| {
                let mut owners = BTreeSet::new();
                let original = anchors.get(name)?;
                let candidates = group
                    .into_iter()
                    .map(|symbol| {
                        if symbol.syntax_element_kind != Some("function_declaration")
                            || !symbol.kinds.contains(&SymbolKind::Callable)
                            || !private_local_declaration(symbol)
                        {
                            return None;
                        }
                        let authored = symbol.name_range?;
                        let bound = self.checked_lexical_scope(file, symbol)?;
                        let returned = returns.get(&authored)?;
                        let callback = callbacks.resolve(bound.start, bound.end)?;
                        let id = local_group_identity(original, symbol.declaration_ordinal)?;
                        (bindings.contains(&(authored, bound))
                            && bound.encloses(symbol.item_range)
                            && symbol.item_range.encloses(*returned)
                            && owners.insert(*callback)
                            && !occupied.contains(&id))
                        .then_some((symbol.declaration_ordinal, id))
                    })
                    .collect::<Option<BTreeMap<_, _>>>()?;
                if candidates.len() < 2 {
                    return None;
                }
                occupied.extend(candidates.values().cloned());
                Some((name.to_owned(), candidates))
            })
            .collect()
    }
}
