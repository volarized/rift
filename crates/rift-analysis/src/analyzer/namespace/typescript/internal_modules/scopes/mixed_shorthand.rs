//! Placement of a lexical binding and object properties that share its written name.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBinding, StaticBindingKind, StaticBodyMember,
    StaticCallee, SyntaxSymbol,
};

use super::{
    FileScopes, LiteralOwners, Scope, SelectedFile, callable_variable, local_group_identity,
    ordinary_private_property, private_variable_declaration,
};

impl<'source> FileScopes<'source> {
    /// Keeps separate identities for a checked binding and its same-named literal properties.
    pub(in crate::analyzer::namespace::typescript::internal_modules) fn place_mixed_shorthand(
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
            group.iter().any(|symbol| mixed_local(symbol))
                && group
                    .iter()
                    .any(|symbol| ordinary_private_property(symbol, file.source))
        });
        if groups.is_empty() {
            return;
        }
        let names = groups
            .values()
            .flatten()
            .filter(|symbol| mixed_local(symbol))
            .map(|symbol| symbol.name.as_str())
            .collect::<BTreeSet<_>>();
        let mut bindings = BTreeMap::<&str, Vec<&StaticBinding>>::new();
        for binding in facts.bindings() {
            let start = usize::try_from(binding.name.start).ok();
            let end = usize::try_from(binding.name.end).ok();
            if let Some(name) = start
                .zip(end)
                .and_then(|(start, end)| file.source.get(start..end))
                && names.contains(name)
            {
                bindings.entry(name).or_default().push(binding);
            }
        }
        let owners = LiteralOwners::new(file, facts, anchors);
        let returned = facts
            .returned_objects()
            .iter()
            .filter_map(|&(_, body)| body)
            .collect::<BTreeSet<_>>();
        let mut occupied = anchors
            .values()
            .chain(placed.values().flat_map(|group| group.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (name, declarations) in groups {
            let (locals, members): (Vec<_>, Vec<_>) = declarations
                .into_iter()
                .partition(|symbol| mixed_local(symbol));
            let [local] = locals.as_slice() else { continue };
            let Some(written) = bindings.get(local.name.as_str()) else {
                continue;
            };
            if !self.proves_mixed_shorthand(file, local, &members, written, &owners, &returned) {
                continue;
            }
            let exact = std::iter::once(*local)
                .chain(members)
                .map(|symbol| {
                    let ordinal = symbol.declaration_ordinal;
                    let id = local_group_identity(anchors.get(name)?, ordinal)?;
                    (!occupied.contains(&id)).then_some((ordinal, id))
                })
                .collect::<Option<BTreeMap<_, _>>>();
            if let Some(exact) = exact {
                occupied.extend(exact.values().cloned());
                placed.insert(name.to_owned(), exact);
            }
        }
    }

    fn proves_mixed_shorthand(
        &self,
        file: &SelectedFile<'source>,
        local: &SyntaxSymbol,
        members: &[&SyntaxSymbol],
        written: &[&StaticBinding],
        owners: &LiteralOwners<'source>,
        returned: &BTreeSet<ByteRange>,
    ) -> bool {
        let Some((scope, name)) = self
            .checked_lexical_scope(file, local)
            .zip(local.name_range)
        else {
            return false;
        };
        let variable = local.syntax_element_kind == Some("variable_declarator");
        let prior_owner = placed_function(owners, local.item_range, variable);
        let initializer = variable
            .then(|| placed_initializer(owners, local.item_range))
            .flatten();
        let object_method = variable && prior_owner.is_none() && initializer.is_none();
        let owner = if let Some((_, callable)) = initializer {
            Some(callable)
        } else if object_method {
            owners
                .enclosing
                .resolve(local.item_range.start, local.item_range.end)
                .and_then(|(range, _)| owners.object_methods.get(range))
                .copied()
        } else {
            prior_owner
        };
        let Some(owner) = owner else {
            return false;
        };
        if local.container.as_deref() != Some(owner.qualified_name.as_str())
            || !owner.body_range.is_some_and(|body| body.encloses(scope))
        {
            return false;
        }
        let mut candidates = written
            .iter()
            .filter(|binding| owner.item_range.encloses(binding.name));
        let binding_kind = if variable {
            StaticBindingKind::Local
        } else {
            StaticBindingKind::Shadow
        };
        if !candidates.next().is_some_and(|binding| {
            binding.kind == binding_kind && binding.name == name && binding.scope == scope
        }) || candidates.next().is_some()
        {
            return false;
        }
        let mut literals = BTreeSet::new();
        members.iter().all(|member| {
            let Some(row) = self.body_members.get(&member.item_range) else {
                return false;
            };
            let direct_return = returned.contains(&row.body)
                && owners.returned.get(&row.body) == Some(&Some(owner.qualified_name.as_str()));
            let shorthand = member.syntax_element_kind == Some("shorthand_property_identifier");
            let initializer_argument = initializer.is_some_and(|(range, _)| {
                shorthand && direct_callable_argument(owners, row, range)
            });
            let admissible = if initializer.is_some() {
                initializer_argument
            } else {
                shorthand
                    || (!object_method
                        && member.syntax_element_kind == Some("pair")
                        && direct_return)
            };
            let same_owner = if let Some((range, _)) = initializer {
                owners
                    .enclosing
                    .resolve(row.body.start, row.body.end)
                    .is_some_and(|(nearest, _)| *nearest == range)
            } else if object_method {
                owners
                    .enclosing
                    .resolve(row.body.start, row.body.end)
                    .is_some_and(|(range, _)| *range == owner.item_range)
            } else {
                placed_function(owners, row.body, variable)
                    .is_some_and(|retained| retained.item_range == owner.item_range)
            };
            admissible
                && (!variable || direct_return || initializer_argument)
                && ordinary_private_property(member, file.source)
                && local.item_range.end <= row.body.start
                && scope.encloses(row.body)
                && same_owner
                && self.of(member, file) == Scope::Body(row.body, false)
                && (initializer_argument || owners.proves_body(member, row))
                && literals.insert(row.body)
        })
    }
}

fn placed_initializer<'source>(
    owners: &LiteralOwners<'source>,
    range: ByteRange,
) -> Option<(ByteRange, &'source SyntaxSymbol)> {
    let (scope, owner) = owners.enclosing.resolve(range.start, range.end)?;
    let owner = owner
        .filter(|symbol| callable_variable(symbol))
        .or_else(|| owners.function_initializers.get(scope).copied())?;
    Some((*scope, owner))
}

fn direct_callable_argument(
    owners: &LiteralOwners<'_>,
    member: &StaticBodyMember,
    scope: ByteRange,
) -> bool {
    owners.calls.get(&member.holder.end).is_some_and(|calls| {
        calls.iter().any(|call| {
            matches!(
                call.callee,
                StaticCallee::Direct | StaticCallee::Member { .. }
            ) && call.callee_range.end <= member.holder.start
                && call.range.encloses(member.holder)
                && scope.encloses(call.range)
                && !owners.bound_calls.contains(&call.range)
        })
    })
}

fn placed_function<'source>(
    owners: &LiteralOwners<'source>,
    range: ByteRange,
    allow_static_method: bool,
) -> Option<&'source SyntaxSymbol> {
    let (_, owner) = owners.enclosing.resolve(range.start, range.end)?;
    owner.filter(|symbol| {
        symbol.syntax_element_kind == Some("function_declaration")
            || (allow_static_method
                && symbol.syntax_element_kind == Some("method_definition")
                && symbol.kinds.contains(&SymbolKind::Static))
    })
}

fn mixed_local(symbol: &SyntaxSymbol) -> bool {
    private_variable_declaration(symbol)
        && matches!(
            symbol.syntax_element_kind,
            Some("shorthand_property_identifier_pattern" | "variable_declarator")
        )
}
