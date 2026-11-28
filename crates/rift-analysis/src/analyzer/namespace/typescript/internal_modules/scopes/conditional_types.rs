//! Separate property identities in the written constraints and results of a type alias.

use rift_syntax::{ByteRange, SyntaxLimits, SyntaxSource, TypeScriptDialect};

use super::{
    BTreeMap, BTreeSet, FileScopes, ScopedIdentityAnchors, SelectedFile, SymbolId, SyntaxSymbol,
    conflicting_assignment_name, local_group_identity, unique_declarations,
};

impl<'source> FileScopes<'source> {
    pub(in crate::analyzer::namespace::typescript::internal_modules) fn withheld_conditional_properties(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        earlier: &ScopedIdentityAnchors,
        limits: SyntaxLimits,
    ) -> ScopedIdentityAnchors {
        if file.syntax.language().name != "typescript"
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
            || withheld.is_empty()
            || !file.source.contains("extends")
        {
            return BTreeMap::new();
        }
        let dialect = if file.syntax.language().dialect.as_deref() == Some("tsx") {
            TypeScriptDialect::Tsx
        } else {
            TypeScriptDialect::TypeScript
        };
        let mut groups = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        let symbols = file.syntax.symbols();
        for symbol in symbols {
            let name = symbol.qualified_name.as_str();
            if withheld.contains(name) && anchors.contains_key(name) && !earlier.contains_key(name)
            {
                groups.entry(name).or_default().push(symbol);
            }
        }
        if groups.is_empty() {
            return BTreeMap::new();
        }
        let owners = unique_declarations(symbols);
        let groups = groups
            .into_iter()
            .filter_map(|(name, members)| {
                let container = members.first()?.container.as_deref()?;
                let owner = owners.get(container).copied().flatten()?;
                (owner.syntax_element_kind == Some("type_alias_declaration")
                    && anchors.contains_key(container)
                    && !conflicting_assignment_name(container, withheld))
                .then_some((name, owner, members))
            })
            .collect::<Vec<_>>();
        let candidates = groups
            .iter()
            .flat_map(|(_, owner, members)| {
                members.iter().filter_map(|member| {
                    let ranges = *self.body_members.get(&member.item_range).copied()?;
                    Some((owner.item_range, ranges))
                })
            })
            .collect::<Vec<_>>();
        let source = SyntaxSource {
            path: file.path,
            text: file.source,
        };
        let Ok(checked) =
            dialect
                .provider()
                .checked_conditional_type_members(source, limits, &candidates)
        else {
            return BTreeMap::new();
        };
        let mut occupied = anchors
            .values()
            .chain(earlier.values().flat_map(BTreeMap::values))
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut scoped = BTreeMap::new();
        for (name, owner, members) in groups {
            let Some(branches) = self.checked_conditional_branches(file, owner, &members, &checked)
            else {
                continue;
            };
            let Some(original) = anchors.get(name) else {
                continue;
            };
            let additions = branches
                .values()
                .map(|ordinals| {
                    let [ordinal] = ordinals.as_slice() else {
                        return None;
                    };
                    let id = local_group_identity(original, *ordinal)?;
                    (!occupied.contains(&id)).then_some((*ordinal, id))
                })
                .collect::<Option<BTreeMap<_, _>>>();
            if let Some(additions) = additions {
                occupied.extend(additions.values().cloned());
                scoped.insert(name.to_owned(), additions);
            }
        }
        scoped
    }

    fn checked_conditional_branches(
        &self,
        file: &SelectedFile<'source>,
        owner: &SyntaxSymbol,
        members: &[&SyntaxSymbol],
        checked: &BTreeSet<ByteRange>,
    ) -> Option<BTreeMap<ByteRange, Vec<usize>>> {
        let bodies = members
            .iter()
            .map(|member| {
                if !checked.contains(&member.item_range) {
                    return None;
                }
                self.body_members
                    .get(&member.item_range)
                    .map(|row| row.body)
            })
            .collect::<Option<BTreeSet<_>>>()?;
        self.checked_union_branches(file, owner, members, &bodies)
    }
}
