//! Direct literal arguments of checked named function initializers.

use super::{
    BTreeMap, BTreeSet, ByteRange, FileScopes, LiteralOwners, Scope, ScopedIdentityAnchors,
    SelectedFile, StaticBindingKind, StaticCallee, StaticFacts, SymbolId, SymbolKind, SyntaxSymbol,
    local_group_identity, ordinary_private_property, unique_declarations,
};

impl<'source> FileScopes<'source> {
    /// Keeps newly checked property scopes after the previously placed literals.
    pub(super) fn scope_named_function_arguments(
        &self,
        file: &SelectedFile<'source>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        owners: &LiteralOwners<'source>,
        occupied: &mut BTreeSet<SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        let Some(facts) = file.syntax.static_facts().filter(|_| !repeated.is_empty()) else {
            return;
        };
        let initializers = exact_initializer_scopes(file, facts, anchors);
        if initializers.is_empty() {
            return;
        }
        let mut groups = BTreeMap::<&str, Vec<_>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if repeated.contains(name) && !scoped.contains_key(name) && anchors.contains_key(name) {
                groups.entry(name).or_default().push(symbol);
            }
        }
        for (name, group) in groups {
            let mut bodies = BTreeSet::new();
            let identities = group
                .into_iter()
                .map(|property| {
                    let body =
                        self.checked_initializer_argument(file, property, owners, &initializers)?;
                    if !bodies.insert(body) {
                        return None;
                    }
                    let ordinal = property.declaration_ordinal;
                    let identity = local_group_identity(anchors.get(name)?, ordinal)?;
                    (!occupied.contains(&identity)).then_some((ordinal, identity))
                })
                .collect::<Option<BTreeMap<_, _>>>();
            if let Some(identities) = identities.filter(|group| group.len() > 1) {
                occupied.extend(identities.values().cloned());
                scoped.insert(name.to_owned(), identities);
            }
        }
    }

    fn checked_initializer_argument(
        &self,
        file: &SelectedFile<'source>,
        property: &SyntaxSymbol,
        owners: &LiteralOwners<'source>,
        initializers: &BTreeMap<ByteRange, &'source str>,
    ) -> Option<ByteRange> {
        if !ordinary_private_property(property, file.source)
            || property.syntax_element_kind != Some("pair")
        {
            return None;
        }
        let member = self.body_members.get(&property.item_range)?;
        if self.of(property, file) != Scope::Body(member.body, false) {
            return None;
        }
        let (scope, None) = owners
            .enclosing
            .resolve(member.body.start, member.body.end)
            .copied()?
        else {
            return None;
        };
        let initializer = initializers.get(&scope).or_else(|| {
            if !owners.callbacks.contains(&scope) {
                return None;
            }
            let mut enclosing = owners.enclosing.containing(scope.start, scope.end)?;
            let (enclosing_scope, _) = enclosing.find(|(bounds, _)| *bounds != scope)?;
            initializers.get(enclosing_scope)
        })?;
        if property.container.as_deref() != Some(*initializer) {
            return None;
        }
        owners
            .calls
            .get(&member.holder.end)?
            .iter()
            .any(|call| {
                call.callee_range.end <= member.holder.start
                    && call.range.encloses(member.holder)
                    && scope.encloses(call.range)
                    && matches!(
                        call.callee,
                        StaticCallee::Direct | StaticCallee::Member { .. }
                    )
                    && !owners.bound_calls.contains(&call.range)
            })
            .then_some(member.body)
    }
}

fn exact_initializer_scopes<'source>(
    file: &SelectedFile<'source>,
    facts: &StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeMap<ByteRange, &'source str> {
    let named = named_function_scopes(file, facts);
    let mut values = BTreeMap::new();
    for &(scope, value) in facts.function_values() {
        if named.contains(&scope) {
            values
                .entry(value)
                .and_modify(|prior| *prior = None)
                .or_insert(Some(scope));
        }
    }
    let mut owners = BTreeMap::new();
    for owner in unique_declarations(file.syntax.symbols())
        .into_values()
        .flatten()
    {
        if owner.syntax_element_kind != Some("variable_declarator")
            || owner.kind != "variable"
            || !anchors.contains_key(&owner.qualified_name)
            || !owner.kinds.contains(&SymbolKind::Callable)
        {
            continue;
        }
        let Some(value) = owner
            .body_range
            .filter(|value| owner.item_range.encloses(*value))
        else {
            continue;
        };
        if let Some(Some(scope)) = values.get(&value) {
            owners
                .entry(*scope)
                .and_modify(|prior| *prior = None)
                .or_insert(Some(owner.qualified_name.as_str()));
        }
    }
    owners
        .into_iter()
        .filter_map(|(scope, owner)| Some((scope, owner?)))
        .collect()
}

fn named_function_scopes(file: &SelectedFile<'_>, facts: &StaticFacts) -> BTreeSet<ByteRange> {
    let mut earliest = BTreeMap::<ByteRange, ByteRange>::new();
    for binding in facts.bindings() {
        if binding.kind == StaticBindingKind::Local && binding.available_from == binding.scope.start
        {
            earliest
                .entry(binding.scope)
                .and_modify(|prior| *prior = (*prior).min(binding.name))
                .or_insert(binding.name);
        }
    }
    facts
        .function_scopes()
        .iter()
        .copied()
        .filter(|scope| {
            let Some(name) = earliest.get(scope) else {
                return false;
            };
            let (Ok(start), Ok(end)) = (usize::try_from(scope.start), usize::try_from(name.start))
            else {
                return false;
            };
            let Some(prefix) = file.source.get(start..end) else {
                return false;
            };
            let mut words = prefix.split_ascii_whitespace();
            let opener = match words.next() {
                Some("async") => words.next(),
                keyword => keyword,
            };
            opener == Some("function") && words.next().is_none()
        })
        .collect()
}
