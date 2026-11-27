//! Placement of a destructured binding and object properties that share its written name.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBinding, StaticBindingKind, SyntaxSymbol,
};

use super::{
    FileScopes, LiteralOwners, Scope, SelectedFile, local_group_identity,
    ordinary_private_property, private_variable_declaration,
};

impl<'source> FileScopes<'source> {
    /// Keeps separate identities for a checked binding and the literals that read it.
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
            group.iter().any(|symbol| destructured_local(symbol))
                && group
                    .iter()
                    .any(|symbol| shorthand_member(symbol, file.source))
        });
        if groups.is_empty() {
            return;
        }
        let names = groups
            .values()
            .flatten()
            .filter(|symbol| destructured_local(symbol))
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
        let mut occupied = anchors
            .values()
            .chain(placed.values().flat_map(|group| group.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (name, declarations) in groups {
            let (locals, members): (Vec<_>, Vec<_>) = declarations
                .into_iter()
                .partition(|symbol| destructured_local(symbol));
            let [local] = locals.as_slice() else { continue };
            let Some(written) = bindings.get(local.name.as_str()) else {
                continue;
            };
            if !self.proves_mixed_shorthand(file, local, &members, written, &owners) {
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
    ) -> bool {
        let Some((scope, name)) = self
            .checked_lexical_scope(file, local)
            .zip(local.name_range)
        else {
            return false;
        };
        let Some(owner) = placed_function(owners, local.item_range) else {
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
        if !candidates.next().is_some_and(|binding| {
            binding.kind == StaticBindingKind::Shadow
                && binding.name == name
                && binding.scope == scope
        }) || candidates.next().is_some()
        {
            return false;
        }
        let mut literals = BTreeSet::new();
        members.iter().all(|member| {
            let Some(row) = self.body_members.get(&member.item_range) else {
                return false;
            };
            shorthand_member(member, file.source)
                && local.item_range.end <= row.body.start
                && scope.encloses(row.body)
                && placed_function(owners, row.body)
                    .is_some_and(|function| function.item_range == owner.item_range)
                && self.of(member, file) == Scope::Body(row.body, false)
                && owners.proves_body(member, row)
                && literals.insert(row.body)
        })
    }
}

fn placed_function<'source>(
    owners: &LiteralOwners<'source>,
    range: ByteRange,
) -> Option<&'source SyntaxSymbol> {
    let (_, owner) = owners.enclosing.resolve(range.start, range.end)?;
    owner.filter(|symbol| symbol.syntax_element_kind == Some("function_declaration"))
}

fn destructured_local(symbol: &SyntaxSymbol) -> bool {
    private_variable_declaration(symbol)
        && symbol.syntax_element_kind == Some("shorthand_property_identifier_pattern")
}

fn shorthand_member(symbol: &SyntaxSymbol, source: &str) -> bool {
    symbol.syntax_element_kind == Some("shorthand_property_identifier")
        && ordinary_private_property(symbol, source)
}
