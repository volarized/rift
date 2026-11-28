//! Placement of a lexical binding and object properties that share its written name.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ReferenceRole, SymbolId};
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBinding, StaticBindingKind, StaticBodyMember,
    StaticCallee, StaticReference, SyntaxSymbol,
};

use super::{
    FileScopes, LiteralOwners, Scope, SelectedFile, callable_variable, local_group_identity,
    ordinary_private_property, private_variable_declaration,
};

struct MixedLiteralEvidence<'source> {
    owners: LiteralOwners<'source>,
    returned: BTreeSet<ByteRange>,
    value_accesses: BTreeMap<ByteRange, ReferenceRole>,
}

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
            if let Some(name) = source_text(file.source, binding.name)
                && names.contains(name)
            {
                bindings.entry(name).or_default().push(binding);
            }
        }
        let value_accesses = if groups
            .values()
            .flatten()
            .any(|symbol| symbol.syntax_element_kind == Some("pair"))
        {
            literal_value_accesses(file.source, &names, facts.references())
        } else {
            BTreeMap::new()
        };
        let evidence = MixedLiteralEvidence {
            owners: LiteralOwners::new(file, facts, anchors),
            returned: facts
                .returned_objects()
                .iter()
                .filter_map(|&(_, body)| body)
                .collect(),
            value_accesses,
        };
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
            if !self.proves_mixed_shorthand(file, local, &members, written, &evidence) {
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
        evidence: &MixedLiteralEvidence<'source>,
    ) -> bool {
        let owners = &evidence.owners;
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
        let Some(body) = owner.body_range else {
            return false;
        };
        let lexical_body = body.encloses(scope);
        let function_scoped_variable = variable
            && owner.syntax_element_kind == Some("function_declaration")
            && scope == owner.item_range
            && body.encloses(local.item_range);
        if local.container.as_deref() != Some(owner.qualified_name.as_str())
            || !(lexical_body || function_scoped_variable)
        {
            return false;
        }
        if !checked_binding(written, owner, name, scope, variable) {
            return false;
        }
        let compound_scope = variable
            .then(|| {
                initializer.map(|(range, _)| range).or_else(|| {
                    (owner.syntax_element_kind == Some("function_declaration"))
                        .then_some(owner.item_range)
                })
            })
            .flatten();
        let mut literals = BTreeSet::new();
        members.iter().all(|member| {
            let Some(row) = self.body_members.get(&member.item_range) else {
                return false;
            };
            let direct_return = evidence.returned.contains(&row.body)
                && owners.returned.get(&row.body) == Some(&Some(owner.qualified_name.as_str()));
            let shorthand = member.syntax_element_kind == Some("shorthand_property_identifier");
            let checked_call_pair = variable
                && owner.syntax_element_kind == Some("function_declaration")
                && same_name_argument_pair(file.source, owner, local, member, row, owners);
            let compound_argument = compound_scope.is_some_and(|range| {
                checked_compound_argument(file.source, local, member, row, range, evidence)
            });
            let argument = compound_argument
                || checked_call_pair
                || initializer.is_some_and(|(range, _)| {
                    shorthand && direct_callable_argument(owners, row, range)
                });
            let admitted_return =
                !object_method && member.syntax_element_kind == Some("pair") && direct_return;
            let admissible = argument || (initializer.is_none() && (shorthand || admitted_return));
            let same_owner = if initializer.is_some() || object_method {
                let expected = initializer.map_or(owner.item_range, |(range, _)| range);
                owners
                    .enclosing
                    .resolve(row.body.start, row.body.end)
                    .is_some_and(|(range, _)| *range == expected)
            } else {
                placed_function(owners, row.body, variable)
                    .is_some_and(|retained| retained.item_range == owner.item_range)
            };
            admissible
                && (lexical_body || checked_call_pair)
                && (!variable || direct_return || argument)
                && ordinary_private_property(member, file.source)
                && local.item_range.end <= row.body.start
                && scope.encloses(row.body)
                && same_owner
                && self.of(member, file) == Scope::Body(row.body, false)
                && ((initializer.is_some() && argument) || owners.proves_body(member, row))
                && literals.insert(row.body)
        })
    }
}

fn checked_binding(
    written: &[&StaticBinding],
    owner: &SyntaxSymbol,
    name: ByteRange,
    scope: ByteRange,
    variable: bool,
) -> bool {
    let mut candidates = written
        .iter()
        .filter(|binding| owner.item_range.encloses(binding.name));
    let binding_kind = if variable {
        StaticBindingKind::Local
    } else {
        StaticBindingKind::Shadow
    };
    candidates.next().is_some_and(|binding| {
        binding.kind == binding_kind && binding.name == name && binding.scope == scope
    }) && candidates.next().is_none()
}

fn literal_value_accesses(
    source: &str,
    names: &BTreeSet<&str>,
    references: &[StaticReference],
) -> BTreeMap<ByteRange, ReferenceRole> {
    let mut accesses = BTreeMap::new();
    for reference in references {
        if matches!(reference.role, ReferenceRole::Read | ReferenceRole::Write)
            && source_text(source, reference.target).is_some_and(|name| names.contains(name))
        {
            accesses
                .entry(reference.target)
                .and_modify(|role| {
                    if reference.role == ReferenceRole::Write {
                        *role = ReferenceRole::Write;
                    }
                })
                .or_insert(reference.role);
        }
    }
    accesses
}

fn checked_compound_argument(
    source: &str,
    local: &SyntaxSymbol,
    property: &SyntaxSymbol,
    member: &StaticBodyMember,
    scope: ByteRange,
    evidence: &MixedLiteralEvidence<'_>,
) -> bool {
    if property.syntax_element_kind != Some("pair") || property.name != local.name {
        return false;
    }
    let Some(key) = property.name_range else {
        return false;
    };
    let tail = ByteRange {
        start: key.end,
        end: member.member.end,
    };
    let Some(value) =
        source_text(source, tail).and_then(|text| text.trim_start().strip_prefix(':'))
    else {
        return false;
    };
    if value.trim() == local.name || !direct_callable_argument(&evidence.owners, member, scope) {
        return false;
    }
    let start = ByteRange {
        start: tail.start,
        end: tail.start,
    };
    let end = ByteRange {
        start: tail.end,
        end: tail.end,
    };
    let mut read = false;
    for (range, role) in evidence.value_accesses.range(start..end) {
        if source_text(source, *range) != Some(local.name.as_str()) {
            continue;
        }
        if *role != ReferenceRole::Read
            || !tail.encloses(*range)
            || evidence
                .owners
                .enclosing
                .resolve(range.start, range.end)
                .is_none_or(|(nearest, _)| *nearest != scope)
        {
            return false;
        }
        read = true;
    }
    read
}

fn source_text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
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

fn same_name_argument_pair(
    source: &str,
    owner: &SyntaxSymbol,
    local: &SyntaxSymbol,
    property: &SyntaxSymbol,
    member: &StaticBodyMember,
    owners: &LiteralOwners<'_>,
) -> bool {
    if property.syntax_element_kind != Some("pair") || property.name != local.name {
        return false;
    }
    let item = property.item_range;
    let written = usize::try_from(item.start)
        .ok()
        .zip(usize::try_from(item.end).ok())
        .and_then(|(start, end)| source.get(start..end));
    if !written
        .and_then(|pair| pair.split_once(':'))
        .is_some_and(|(key, value)| key.trim() == local.name && value.trim() == local.name)
    {
        return false;
    }
    owners.calls.get(&member.holder.end).is_some_and(|calls| {
        calls.iter().any(|call| {
            call.callee == StaticCallee::Direct
                && call.callee_range.end <= member.holder.start
                && call.range.encloses(member.holder)
                && owner
                    .body_range
                    .is_some_and(|body| body.encloses(call.range))
                && !owners.bound_calls.contains(&call.range)
        })
    })
}

fn placed_function<'source>(
    owners: &LiteralOwners<'source>,
    range: ByteRange,
    allow_class_method: bool,
) -> Option<&'source SyntaxSymbol> {
    let (_, owner) = owners.enclosing.resolve(range.start, range.end)?;
    owner.filter(|symbol| {
        symbol.syntax_element_kind == Some("function_declaration")
            || (allow_class_method && symbol.syntax_element_kind == Some("method_definition"))
    })
}

fn mixed_local(symbol: &SyntaxSymbol) -> bool {
    private_variable_declaration(symbol)
        && matches!(
            symbol.syntax_element_kind,
            Some("shorthand_property_identifier_pattern" | "variable_declarator")
        )
}
