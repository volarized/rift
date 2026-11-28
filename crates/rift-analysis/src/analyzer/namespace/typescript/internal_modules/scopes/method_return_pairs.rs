//! Written return properties of a checked object method.

use super::{
    BTreeMap, BTreeSet, ByteRange, FileScopes, LiteralOwners, Scope, ScopedIdentityAnchors,
    SelectedFile, StaticBindingKind, StaticBodyMember, StaticCallee, SymbolId, SyntaxSymbol,
    local_group_identity, ordinary_private_property, private_variable_declaration,
};

impl<'source> FileScopes<'source> {
    /// Places a checked return pair separately from a same-name local or literal argument.
    pub(in crate::analyzer::namespace::typescript::internal_modules) fn place_checked_method_return_pairs(
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
        let mut groups = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if repeated.contains(name) && anchors.contains_key(name) && !placed.contains_key(name) {
                groups.entry(name).or_default().push(symbol);
            }
        }
        groups.retain(|_, group| {
            group.iter().any(|symbol| {
                symbol.syntax_element_kind == Some("pair")
                    && ordinary_private_property(symbol, file.source)
            })
        });
        if groups.is_empty() {
            return;
        }
        let owners = LiteralOwners::new(file, facts, anchors);
        if owners.object_methods.is_empty() {
            return;
        }
        let local_bindings = facts
            .bindings()
            .iter()
            .filter(|binding| binding.kind == StaticBindingKind::Local)
            .map(|binding| (binding.name, binding.scope))
            .collect::<BTreeSet<_>>();
        let mut occupied = anchors
            .values()
            .chain(placed.values().flat_map(|group| group.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (name, group) in groups {
            let Some((pair, local)) =
                self.checked_method_return_pair(file, &group, &owners, &local_bindings)
            else {
                continue;
            };
            let exact = std::iter::once(pair)
                .chain(local)
                .map(|symbol| {
                    let ordinal = symbol.declaration_ordinal;
                    let identity = local_group_identity(anchors.get(name)?, ordinal)?;
                    (!occupied.contains(&identity)).then_some((ordinal, identity))
                })
                .collect::<Option<BTreeMap<_, _>>>();
            if let Some(exact) = exact {
                occupied.extend(exact.values().cloned());
                placed.insert(name.to_owned(), exact);
            }
        }
    }

    fn checked_method_return_pair<'symbol>(
        &self,
        file: &SelectedFile<'source>,
        group: &[&'symbol SyntaxSymbol],
        owners: &LiteralOwners<'source>,
        local_bindings: &BTreeSet<(ByteRange, ByteRange)>,
    ) -> Option<(&'symbol SyntaxSymbol, Option<&'symbol SyntaxSymbol>)> {
        let mut returned = None;
        let mut lexical = None;
        let mut argument = false;
        let mut bodies = BTreeSet::new();
        for &property in group {
            let method = checked_method(owners, property.item_range)?;
            if property.container.as_deref() != Some(method.qualified_name.as_str()) {
                return None;
            }
            if private_variable_declaration(property)
                && property.syntax_element_kind == Some("variable_declarator")
            {
                let (range, scope) = property
                    .name_range
                    .zip(self.checked_lexical_scope(file, property))?;
                if lexical.replace(property).is_some()
                    || !local_bindings.contains(&(range, scope))
                    || !method.body_range?.encloses(scope)
                {
                    return None;
                }
                continue;
            }
            if !ordinary_private_property(property, file.source) {
                return None;
            }
            let member = self.body_members.get(&property.item_range)?;
            if !bodies.insert(member.body)
                || self.of(property, file) != Scope::Body(member.body, false)
                || checked_method(owners, member.body)?.item_range != method.item_range
            {
                return None;
            }
            if owners.returned.get(&member.body) == Some(&Some(method.qualified_name.as_str())) {
                if property.syntax_element_kind != Some("pair")
                    || returned.replace(property).is_some()
                {
                    return None;
                }
            } else if checked_argument(owners, method.body_range?, member) {
                argument = true;
            } else {
                return None;
            }
        }
        let pair = returned?;
        (argument || lexical.is_some()).then_some((pair, lexical))
    }
}

fn checked_method<'source>(
    owners: &LiteralOwners<'source>,
    range: ByteRange,
) -> Option<&'source SyntaxSymbol> {
    let (scope, _) = owners.enclosing.resolve(range.start, range.end)?;
    owners.object_methods.get(scope).copied()
}

fn checked_argument(
    owners: &LiteralOwners<'_>,
    body: ByteRange,
    member: &StaticBodyMember,
) -> bool {
    owners.calls.get(&member.holder.end).is_some_and(|calls| {
        calls.iter().any(|call| {
            call.callee_range.end <= member.holder.start
                && call.range.encloses(member.holder)
                && body.encloses(call.range)
                && matches!(
                    call.callee,
                    StaticCallee::Direct | StaticCallee::Member { .. }
                )
                && !owners.bound_calls.contains(&call.range)
        })
    })
}
