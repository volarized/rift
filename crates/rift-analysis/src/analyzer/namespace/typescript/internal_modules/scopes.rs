//! Names one file declares in more than one scope.

mod assigned_literals;

use std::collections::{BTreeMap, BTreeSet};

use rift_core::{ReferenceRole, SymbolId};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, JavaScriptSyntaxProvider, ScopedIdentityAnchors, StaticBindingKind,
    StaticBodyMember, StaticCall, StaticCallee, StaticFacts, StaticFunctionContext,
    SyntaxExportKind, SyntaxFacts, SyntaxSymbol,
};

use super::super::conflicting_assignment_name;
use super::{SelectedFile, binding_name};
use crate::EnclosingDefinitions;

mod conditional_types;
mod default_array_properties;
mod disjoint_properties;
mod function_arguments;
mod mixed_shorthand;
mod module_assignment_shorthand;
mod private_return_functions;
mod restored_arguments;
mod scoped_initializer_children;

/// Where a declaration binds its name. Two declarations of one qualified name are one entity
/// only when they bind it in the same place: overloads, merged interfaces and namespaces,
/// an accessor pair and a redeclared `var` do.
#[derive(PartialEq, Eq)]
enum Scope {
    /// The innermost lexical scope of the name.
    Lexical(ByteRange),
    /// The instances of the containing declaration.
    Member,
    /// The containing declaration itself: a static member, or an export of a namespace.
    Static,
    /// One literal or class that no declaration holds alone, and whether the member is static.
    Body(ByteRange, bool),
}

/// Keeps the older identity of the first private lexical group and names later groups
/// by the source ordinal of their first physical declaration.
pub(in crate::analyzer::namespace::typescript) fn private_local_identities(
    file: &SelectedFile<'_>,
    anchors: &BTreeMap<String, SymbolId>,
) -> ScopedIdentityAnchors {
    if file.syntax.has_errors() || file.syntax.left_out_declaration_count() != 0 {
        return BTreeMap::new();
    }
    let symbols = file.syntax.symbols();
    let repeated = symbols
        .iter()
        .filter(|symbol| {
            symbol.declaration_ordinal > 0
                && private_local_declaration(symbol)
                && anchors.contains_key(&symbol.qualified_name)
        })
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    let Some(bindings) = file
        .syntax
        .export_bindings()
        .filter(|_| !repeated.is_empty())
    else {
        return BTreeMap::new();
    };
    let exported = bindings
        .iter()
        .flat_map(|binding| [binding.local, binding.exported])
        .flatten()
        .filter_map(|range| binding_name(file.source, range))
        .collect::<BTreeSet<_>>();
    let mut named = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
    for symbol in symbols {
        let name = symbol.qualified_name.as_str();
        if repeated.contains(name) && !exported.contains(name) {
            named.entry(name).or_default().push(symbol);
        }
    }
    if named.is_empty() {
        return BTreeMap::new();
    }
    let scopes = FileScopes::new(file.syntax, file.source);
    let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
    let mut identities_by_name = BTreeMap::new();
    for (name, declarations) in named {
        if !declarations
            .iter()
            .all(|symbol| private_local_declaration(symbol))
        {
            continue;
        }
        let Some(groups) = scopes.lexical_groups(file, &declarations) else {
            continue;
        };
        let Some(first) = anchors.get(name) else {
            continue;
        };
        let identities = groups
            .into_iter()
            .skip(1)
            .map(|ordinals| {
                let identity = local_group_identity(first, ordinals.first().copied()?)?;
                (!occupied.contains(&identity)).then_some((ordinals, identity))
            })
            .collect::<Option<Vec<_>>>();
        let Some(identities) = identities.filter(|groups| !groups.is_empty()) else {
            continue;
        };
        let mut overrides = BTreeMap::new();
        for (ordinals, identity) in identities {
            occupied.insert(identity.clone());
            for ordinal in ordinals {
                overrides.insert(ordinal, identity.clone());
            }
        }
        identities_by_name.insert(name.to_owned(), overrides);
    }
    identities_by_name
}

fn unique_declarations(symbols: &[SyntaxSymbol]) -> BTreeMap<&str, Option<&SyntaxSymbol>> {
    let mut unique = BTreeMap::new();
    for symbol in symbols {
        unique
            .entry(symbol.qualified_name.as_str())
            .and_modify(|owner| *owner = None)
            .or_insert(Some(symbol));
    }
    unique
}

fn private_local_declaration(symbol: &SyntaxSymbol) -> bool {
    symbol.container.is_none()
        && symbol.qualified_name == symbol.name
        && !symbol.kinds.contains(&SymbolKind::Public)
        && matches!(
            symbol.syntax_element_kind,
            Some("function_declaration" | "variable_declarator")
        )
}

fn private_variable_declaration(symbol: &SyntaxSymbol) -> bool {
    symbol.kind == "variable"
        && matches!(
            symbol.syntax_element_kind,
            Some("variable_declarator" | "identifier" | "shorthand_property_identifier_pattern")
        )
        && !symbol.kinds.contains(&SymbolKind::Public)
        && !symbol.kinds.contains(&SymbolKind::Callable)
}

fn local_group_identity(original: &SymbolId, ordinal: usize) -> Option<SymbolId> {
    let original = SymbolIdentity::parse(original.as_str()).ok()?;
    let mut path = original.qualified_path().to_vec();
    let name = path.last_mut()?;
    *name = format!("{name}#{ordinal}");
    let identity =
        SymbolIdentity::new(original.owner().clone(), original.language().clone(), path).ok()?;
    SymbolId::new(identity.wire_identity()).ok()
}

pub(super) struct FileScopes<'source> {
    source: ByteRange,
    /// The scope of each bound name, by the range of the name.
    bound: BTreeMap<ByteRange, ByteRange>,
    enclosing: EnclosingDefinitions<ByteRange>,
    /// Functions and classes, whose own members differ from those of their instances.
    constructors: BTreeSet<&'source str>,
    /// Members of literals and classes, by their own range.
    body_members: BTreeMap<ByteRange, &'source StaticBodyMember>,
    /// The literal or class each declaration holds, for its values and for its types, or
    /// `None` for a name declared with several.
    held: BTreeMap<(&'source str, bool), Option<ByteRange>>,
    /// Each namespace by its name and the end of its block.
    namespaces: BTreeSet<(&'source str, u64)>,
}

impl<'source> FileScopes<'source> {
    pub(super) fn new(syntax: &'source SyntaxFacts, source: &str) -> Self {
        let facts = syntax.static_facts();
        let mut bound = BTreeMap::<ByteRange, ByteRange>::new();
        for binding in facts.map_or(&[][..], StaticFacts::bindings) {
            // A function value binds its name a second time, inside its own body.
            bound
                .entry(binding.name)
                .and_modify(|scope| {
                    if binding.scope.end - binding.scope.start > scope.end - scope.start {
                        *scope = binding.scope;
                    }
                })
                .or_insert(binding.scope);
        }
        let symbols = syntax.symbols();
        let body_members = facts
            .map_or(&[][..], StaticFacts::body_members)
            .iter()
            .map(|row| (row.member, row))
            .collect::<BTreeMap<_, _>>();
        let declared = symbols
            .iter()
            .map(|symbol| (symbol.qualified_name.as_str(), symbol.item_range))
            .collect::<BTreeSet<_>>();
        let mut held = BTreeMap::<(&str, bool), Option<ByteRange>>::new();
        for symbol in symbols {
            let (Some(container), Some(row)) = (
                symbol.container.as_deref(),
                body_members.get(&symbol.item_range),
            ) else {
                continue;
            };
            // `name = { .. }`, `name: { .. }` and `class name { .. }` declare members of `name`.
            if [row.holder, row.body]
                .iter()
                .any(|range| declared.contains(&(container, *range)))
            {
                held.entry((container, is_type_member(symbol)))
                    .and_modify(|body| *body = body.filter(|body| *body == row.body))
                    .or_insert(Some(row.body));
            }
        }
        Self {
            source: ByteRange {
                start: 0,
                end: u64::try_from(source.len()).unwrap_or(u64::MAX),
            },
            enclosing: EnclosingDefinitions::new(
                bound
                    .values()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|scope| (scope.start, scope.end, *scope)),
            ),
            bound,
            constructors: symbols
                .iter()
                .filter(|symbol| {
                    symbol.kind == "class" || symbol.kinds.contains(&SymbolKind::Callable)
                })
                .map(|symbol| symbol.qualified_name.as_str())
                .collect(),
            body_members,
            held,
            namespaces: symbols
                .iter()
                .filter(|symbol| symbol.kind == "namespace")
                .map(|symbol| (symbol.qualified_name.as_str(), symbol.item_range.end))
                .collect(),
        }
    }

    /// The qualified names that physical declarations of different scopes share.
    pub(super) fn repeated_names(&self, file: &SelectedFile<'source>) -> BTreeSet<&'source str> {
        let mut first = BTreeMap::new();
        let mut repeated = BTreeSet::new();
        for symbol in file.syntax.symbols() {
            let scope = self.of(symbol, file);
            match first.entry(symbol.qualified_name.as_str()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(scope);
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if *entry.get() != scope {
                        repeated.insert(symbol.qualified_name.as_str());
                    }
                }
            }
        }
        repeated
    }

    /// Keeps an authored module binding and scopes a same-named direct argument property.
    pub(super) fn separate_module_argument_properties(
        &self,
        file: &SelectedFile<'source>,
        repeated: &mut BTreeSet<&'source str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        let Some(facts) = file.syntax.static_facts().filter(|_| {
            !repeated.is_empty()
                && file.syntax.language().name == "javascript"
                && !file.syntax.has_errors()
                && file.syntax.left_out_declaration_count() == 0
        }) else {
            return BTreeMap::new();
        };
        let mut groups = BTreeMap::<&str, Vec<_>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if repeated.contains(name) && anchors.contains_key(name) {
                groups.entry(name).or_default().push(symbol);
            }
        }
        let bound = facts
            .bindings()
            .iter()
            .filter(|binding| {
                matches!(
                    binding.kind,
                    StaticBindingKind::Local | StaticBindingKind::JavaScriptClassValue { .. }
                )
            })
            .map(|binding| binding.name)
            .collect::<BTreeSet<_>>();
        let written = facts
            .assignments()
            .iter()
            .filter_map(|&range| binding_name(file.source, range))
            .collect::<BTreeSet<_>>();
        let candidates = groups
            .into_iter()
            .filter_map(|(name, group)| {
                let [a, b] = group.as_slice() else {
                    return None;
                };
                let (binding, property) = if ordinary_private_property(a, file.source) {
                    (*b, *a)
                } else {
                    (*a, *b)
                };
                if !ordinary_private_property(property, file.source)
                    || property.syntax_element_kind != Some("pair")
                    || !matches!(
                        binding.syntax_element_kind,
                        Some("class_declaration" | "variable_declarator")
                    )
                    || binding.is_ecmascript_package_binding()
                    || !bound.contains(&binding.name_range?)
                    || written.contains(binding.name.as_str())
                    || !self.body_members.contains_key(&property.item_range)
                {
                    return None;
                }
                let nested = (self.lexical(binding, file.source) != self.source)
                    .then_some(binding.item_range);
                if nested.is_none() && (binding.container.is_some() || property.container.is_some())
                {
                    return None;
                }
                let identity =
                    local_group_identity(anchors.get(name)?, property.declaration_ordinal)?;
                Some((
                    property.item_range,
                    (name, property.declaration_ordinal, identity, nested),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let limits = file.syntax.syntax_limits().unwrap_or_default();
        let module = candidates
            .iter()
            .filter_map(|(&property, (_, _, _, binding))| binding.is_none().then_some(property))
            .collect();
        let nested = candidates
            .iter()
            .filter_map(|(&property, (_, _, _, binding))| {
                binding.map(|binding| (property, binding))
            })
            .collect();
        let accepted = JavaScriptSyntaxProvider::scoped_call_argument_properties(
            file.source,
            limits,
            &module,
            &nested,
        );
        let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
        let mut physical = BTreeMap::new();
        for (range, (name, ordinal, identity, _)) in candidates {
            if accepted.contains(&range) && occupied.insert(identity.clone()) {
                repeated.remove(name);
                physical.insert(name.to_owned(), BTreeMap::from([(ordinal, identity)]));
            }
        }
        physical
    }

    /// Names repeated private keys by their captured literal and physical ordinal.
    pub(super) fn literal_properties(
        &self,
        file: &SelectedFile<'source>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        let language = file.syntax.language().name.as_str();
        let typed = language == "typescript";
        if repeated.is_empty()
            || !matches!(language, "javascript" | "typescript")
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return BTreeMap::new();
        }
        let symbols = file.syntax.symbols();
        let mut candidates = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for symbol in symbols {
            let name = symbol.qualified_name.as_str();
            if repeated.contains(name) && anchors.contains_key(name) {
                candidates.entry(name).or_default().push(symbol);
            }
        }
        candidates.retain(|_, group| {
            group.iter().all(|symbol| {
                (!typed || symbol.syntax_element_kind == Some("pair"))
                    && ordinary_private_property(symbol, file.source)
            })
        });
        let Some(facts) = file
            .syntax
            .static_facts()
            .filter(|_| !candidates.is_empty())
        else {
            return BTreeMap::new();
        };
        let owners = LiteralOwners::new(file, facts, anchors);
        let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
        let mut scoped = BTreeMap::new();
        for (name, declarations) in candidates {
            let mut groups = BTreeMap::<ByteRange, Vec<usize>>::new();
            for declaration in declarations {
                let Some(row) = self.body_members.get(&declaration.item_range) else {
                    continue;
                };
                if self.of(declaration, file) != Scope::Body(row.body, false) {
                    continue;
                }
                let proven = if typed {
                    owners.proves_typed_method_argument(declaration, row)
                } else {
                    owners.proves_body(declaration, row)
                };
                if !proven {
                    continue;
                }
                groups
                    .entry(row.body)
                    .or_default()
                    .push(declaration.declaration_ordinal);
            }
            if groups.len() < 2 {
                continue;
            }
            let Some(original) = anchors.get(name) else {
                continue;
            };
            let identities = groups
                .into_values()
                .map(|ordinals| {
                    let identity = local_group_identity(original, *ordinals.iter().min()?)?;
                    (!occupied.contains(&identity)).then_some((ordinals, identity))
                })
                .collect::<Option<Vec<_>>>();
            if let Some(identities) = identities {
                let mut overrides = BTreeMap::new();
                for (ordinals, identity) in identities {
                    occupied.insert(identity.clone());
                    for ordinal in ordinals {
                        overrides.insert(ordinal, identity.clone());
                    }
                }
                scoped.insert(name.to_owned(), overrides);
            }
        }
        if !typed {
            self.scope_initialized_properties(
                file,
                facts,
                anchors,
                &owners,
                &mut occupied,
                &mut scoped,
            );
            self.scope_nested_properties(file, repeated, anchors, &mut occupied, &mut scoped);
            self.scope_named_function_arguments(
                file,
                repeated,
                anchors,
                &owners,
                &mut occupied,
                &mut scoped,
            );
        }
        scoped
    }

    /// A placed property can hold a literal or an array of literal slots.
    fn scope_nested_properties(
        &self,
        file: &SelectedFile<'source>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        occupied: &mut BTreeSet<SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        let properties = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| ordinary_private_property(symbol, file.source));
        let parents = properties
            .clone()
            .filter(|symbol| symbol.syntax_element_kind == Some("pair"))
            .map(|symbol| (symbol.item_range, symbol))
            .collect::<BTreeMap<_, _>>();
        let arrays = parents
            .values()
            .filter_map(|&parent| {
                let body = parent.body_range?;
                let name = parent.name_range?;
                (parent.item_range.encloses(body)
                    && name.start == parent.item_range.start
                    && !parent.name.starts_with('[')
                    && written_array(file.source, body))
                .then_some((body, parent))
            })
            .collect::<BTreeMap<_, _>>();
        let mut members = BTreeMap::new();
        let mut counts = BTreeMap::new();
        for child in properties {
            let Some(member) = self.body_members.get(&child.item_range) else {
                continue;
            };
            let parent = parents
                .get(&member.holder)
                .map(|&parent| (parent, false))
                .or_else(|| arrays.get(&member.holder).map(|&parent| (parent, true)));
            let Some((parent, array)) = parent else {
                continue;
            };
            if child.container.as_deref() != Some(parent.qualified_name.as_str())
                || !conflicting_assignment_name(&child.qualified_name, repeated)
            {
                continue;
            }
            *counts
                .entry((member.body, child.name.as_str()))
                .or_insert(0_usize) += 1;
            members.insert(child.item_range, (parent, child, member.body, array));
        }
        // Parent pairs start before their literal's members.
        for (parent, child, body, array) in members.into_values() {
            if counts.get(&(body, child.name.as_str())) != Some(&1)
                || scoped
                    .get(&child.qualified_name)
                    .is_some_and(|group| group.contains_key(&child.declaration_ordinal))
            {
                continue;
            }
            let identity = scoped
                .get(&parent.qualified_name)
                .and_then(|group| group.get(&parent.declaration_ordinal))
                .or_else(|| {
                    (array && !conflicting_assignment_name(&parent.qualified_name, repeated))
                        .then(|| anchors.get(&parent.qualified_name))
                        .flatten()
                });
            let Some(owner) = identity.and_then(|id| SymbolIdentity::parse(id.as_str()).ok())
            else {
                continue;
            };
            let id = literal_property_identity(&owner, &child.name).and_then(|id| {
                if array {
                    local_group_identity(&id, child.declaration_ordinal)
                } else {
                    Some(id)
                }
            });
            let Some(id) = id else {
                continue;
            };
            if occupied.insert(id.clone()) {
                scoped
                    .entry(child.qualified_name.clone())
                    .or_default()
                    .insert(child.declaration_ordinal, id);
            }
        }
    }

    /// The authored variable, initializer and member must share one physical holder.
    fn scope_initialized_properties(
        &self,
        file: &SelectedFile<'source>,
        facts: &StaticFacts,
        anchors: &BTreeMap<String, SymbolId>,
        owners: &LiteralOwners<'source>,
        occupied: &mut BTreeSet<SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        let captured = facts
            .bindings()
            .iter()
            .filter(|binding| binding.kind == StaticBindingKind::Local)
            .map(|binding| (binding.name, binding.scope))
            .collect::<BTreeSet<_>>();
        let mut declarations = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        let mut members = BTreeMap::<ByteRange, Vec<(&SyntaxSymbol, &StaticBodyMember)>>::new();
        for symbol in file.syntax.symbols() {
            declarations
                .entry(&symbol.qualified_name)
                .or_default()
                .push(symbol);
            if let Some(member) = self.body_members.get(&symbol.item_range) {
                members
                    .entry(member.holder)
                    .or_default()
                    .push((symbol, member));
            }
        }
        for (&name, bindings) in &declarations {
            if bindings.len() < 2 {
                continue;
            }
            let Some(original) = anchors.get(name) else {
                continue;
            };
            let mut lexical_scopes = BTreeSet::new();
            let mut proposed = BTreeSet::new();
            let mut additions = ScopedIdentityAnchors::new();
            let complete = bindings.iter().all(|symbol| {
                let Some((scope, body)) = self.scoped_initializer(file, symbol, owners, &captured)
                else {
                    return false;
                };
                let Some(properties) = members.get(&symbol.item_range) else {
                    return false;
                };
                let Some(identity) = local_group_identity(original, symbol.declaration_ordinal)
                else {
                    return false;
                };
                if !lexical_scopes.insert(scope)
                    || occupied.contains(&identity)
                    || !proposed.insert(identity.clone())
                {
                    return false;
                }
                let Some(owner) = SymbolIdentity::parse(identity.as_str()).ok() else {
                    return false;
                };
                for &(property, member) in properties {
                    if member.body != body
                        || property.container.as_deref() != Some(name)
                        || !ordinary_private_property(property, file.source)
                    {
                        return false;
                    }
                    let Some(identity) = literal_property_identity(&owner, &property.name) else {
                        return false;
                    };
                    if occupied.contains(&identity) || !proposed.insert(identity.clone()) {
                        return false;
                    }
                    additions
                        .entry(property.qualified_name.clone())
                        .or_default()
                        .insert(property.declaration_ordinal, identity);
                }
                additions
                    .entry(name.to_owned())
                    .or_default()
                    .insert(symbol.declaration_ordinal, identity);
                true
            });
            if complete
                && additions.iter().all(|(name, group)| {
                    group.len() >= 2
                        && !scoped.contains_key(name)
                        && declarations
                            .get(name.as_str())
                            .is_some_and(|rows| rows.len() == group.len())
                })
            {
                occupied.extend(proposed);
                scoped.extend(additions);
            }
        }
    }

    fn scoped_initializer(
        &self,
        file: &SelectedFile<'_>,
        symbol: &SyntaxSymbol,
        owners: &LiteralOwners<'_>,
        captured: &BTreeSet<(ByteRange, ByteRange)>,
    ) -> Option<(ByteRange, ByteRange)> {
        if symbol.syntax_element_kind != Some("variable_declarator")
            || symbol.kind != "variable"
            || symbol.kinds.contains(&SymbolKind::Public)
        {
            return None;
        }
        let name = symbol.name_range?;
        let body = symbol.body_range?;
        let Scope::Lexical(scope) = self.of(symbol, file) else {
            return None;
        };
        let owner = owners
            .enclosing
            .resolve(symbol.item_range.start, symbol.item_range.end)
            .and_then(|(_, owner)| *owner)?;
        (owner.syntax_element_kind == Some("function_declaration")
            && symbol.container.as_deref() == Some(owner.qualified_name.as_str())
            && owner.body_range?.encloses(scope)
            && scope.encloses(symbol.item_range)
            && captured.contains(&(name, scope)))
        .then_some((scope, body))
    }

    /// Places literal argument keys beneath an already scoped private initializer.
    pub(super) fn place_scoped_call_argument_properties(
        &self,
        file: &SelectedFile<'source>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
        scoped: &mut ScopedIdentityAnchors,
    ) {
        let Some(facts) = file.syntax.static_facts().filter(|_| {
            !scoped.is_empty()
                && file.syntax.language().name == "javascript"
                && !file.syntax.has_errors()
                && file.syntax.left_out_declaration_count() == 0
        }) else {
            return;
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
            .map(|call| (call.range, call.callee_range.end))
            .collect::<BTreeMap<_, _>>();
        let parents = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| {
                private_variable_declaration(symbol)
                    && symbol.syntax_element_kind == Some("variable_declarator")
            })
            .filter_map(|symbol| {
                let scope = self.checked_lexical_scope(file, symbol)?;
                if !scope.encloses(symbol.item_range) {
                    return None;
                }
                let body = symbol.body_range?;
                let callee_end = *calls.get(&body)?;
                let identity = scoped
                    .get(&symbol.qualified_name)?
                    .get(&symbol.declaration_ordinal)?;
                let owner = SymbolIdentity::parse(identity.as_str()).ok()?;
                Some((
                    (symbol.qualified_name.as_str(), body.end),
                    (body, callee_end, owner),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        let mut proposed = BTreeMap::new();
        for property in file.syntax.symbols() {
            if !ordinary_private_property(property, file.source)
                || !withheld.contains(property.qualified_name.as_str())
                || scoped
                    .get(&property.qualified_name)
                    .is_some_and(|group| group.contains_key(&property.declaration_ordinal))
            {
                continue;
            }
            let Some((container, member)) = property
                .container
                .as_deref()
                .zip(self.body_members.get(&property.item_range))
            else {
                continue;
            };
            let Some((body, callee_end, owner)) = parents.get(&(container, member.holder.end))
            else {
                continue;
            };
            if *callee_end > member.holder.start
                || !body.encloses(member.holder)
                || !member.holder.encloses(member.body)
                || !member.body.encloses(property.item_range)
            {
                continue;
            }
            if let Some(identity) = literal_property_identity(owner, &property.name) {
                proposed
                    .entry(identity)
                    .and_modify(|candidate| *candidate = None)
                    .or_insert(Some(property));
            }
        }
        let occupied = anchors
            .values()
            .chain(scoped.values().flat_map(|group| group.values()))
            .cloned()
            .collect::<BTreeSet<_>>();
        for (identity, property) in proposed {
            if let Some(property) = property
                && !occupied.contains(&identity)
            {
                scoped
                    .entry(property.qualified_name.clone())
                    .or_default()
                    .insert(property.declaration_ordinal, identity);
            }
        }
    }

    /// Names distinct private lexical declarations that the file module cannot share.
    pub(super) fn withheld_lexical_bindings(
        &self,
        file: &SelectedFile<'_>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        let language = file.syntax.language().name.as_str();
        let typed = language == "typescript";
        if !matches!(language, "javascript" | "typescript")
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return BTreeMap::new();
        }
        let mut groups = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if withheld.contains(name) && anchors.contains_key(name) {
                groups.entry(name).or_default().push(symbol);
            }
        }
        groups
            .into_iter()
            .filter_map(|(name, declarations)| {
                if !declarations.iter().all(|symbol| {
                    private_variable_declaration(symbol)
                        && symbol.container.as_deref().is_none_or(|container| {
                            anchors.contains_key(container)
                                && !super::super::conflicting_assignment_name(container, withheld)
                        })
                }) {
                    return None;
                }
                if typed
                    && !declarations
                        .iter()
                        .any(|declaration| self.checked_for_initializer(file, declaration))
                {
                    return None;
                }
                let original = anchors.get(name)?;
                let groups = self.lexical_groups(file, &declarations)?;
                if groups.len() < 2 {
                    return None;
                }
                let mut exact = BTreeMap::new();
                for ordinals in groups {
                    let ordinal = ordinals.first().copied()?;
                    let identity = local_group_identity(original, ordinal)?;
                    exact.extend(
                        ordinals
                            .into_iter()
                            .map(|ordinal| (ordinal, identity.clone())),
                    );
                }
                Some((name.to_owned(), exact))
            })
            .collect()
    }

    /// A lexical binding whose captured scope starts at its traditional loop header.
    fn checked_for_initializer(
        &self,
        file: &SelectedFile<'source>,
        declaration: &SyntaxSymbol,
    ) -> bool {
        if declaration.syntax_element_kind != Some("variable_declarator") {
            return false;
        }
        self.checked_lexical_scope(file, declaration)
            .filter(|scope| scope.encloses(declaration.item_range))
            .and_then(|scope| {
                let start = usize::try_from(scope.start).ok()?;
                let end = usize::try_from(declaration.item_range.start).ok()?;
                // The syntax-owned scope includes the header's punctuation.
                file.source.get(start..end)
            })
            .and_then(|prefix| prefix.strip_prefix("for"))
            .is_some_and(|header| header.trim_start().starts_with('('))
    }

    /// Names the same property separately when the source writes distinct union branches.
    pub(super) fn withheld_union_properties(
        &self,
        file: &SelectedFile<'_>,
        withheld: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        if file.syntax.language().name != "typescript"
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return BTreeMap::new();
        }
        let mut owners = BTreeMap::<&str, Option<&SyntaxSymbol>>::new();
        let mut groups = BTreeMap::<&str, Vec<&SyntaxSymbol>>::new();
        for symbol in file.syntax.symbols() {
            let name = symbol.qualified_name.as_str();
            if anchors.contains_key(name) {
                owners
                    .entry(name)
                    .and_modify(|owner| *owner = None)
                    .or_insert(Some(symbol));
                if withheld.contains(name) {
                    groups.entry(name).or_default().push(symbol);
                }
            }
        }
        let mut captured = BTreeMap::<ByteRange, BTreeMap<ByteRange, BTreeSet<ByteRange>>>::new();
        for row in file
            .syntax
            .static_facts()
            .map_or(&[][..], StaticFacts::typescript_union_branches)
        {
            captured
                .entry(row.owner)
                .or_default()
                .entry(row.union)
                .or_default()
                .insert(row.branch);
        }
        let mut direct = BTreeMap::new();
        groups
            .into_iter()
            .filter_map(|(name, members)| {
                let container = members.first()?.container.as_deref()?;
                let owner = owners.get(container).copied().flatten()?;
                let alias = owner.syntax_element_kind == Some("type_alias_declaration");
                if (!alias && !owner.kinds.contains(&SymbolKind::Callable))
                    || super::super::conflicting_assignment_name(container, withheld)
                {
                    return None;
                }
                let original = anchors.get(name)?;
                let old = alias
                    .then(|| {
                        direct
                            .entry(container)
                            .or_insert_with(|| self.checked_union_bodies(file, owner))
                            .as_ref()
                            .and_then(|bodies| {
                                self.checked_union_branches(file, owner, &members, bodies)
                            })
                    })
                    .flatten();
                let branches = old.or_else(|| {
                    let mut complete =
                        captured
                            .get(&owner.item_range)?
                            .values()
                            .filter_map(|bodies| {
                                self.checked_union_branches(file, owner, &members, bodies)
                            });
                    let first = complete.next()?;
                    complete.next().is_none().then_some(first)
                })?;
                let identities = branches
                    .into_values()
                    .map(|ordinals| {
                        let identity = local_group_identity(original, *ordinals.first()?)?;
                        Some(
                            ordinals
                                .into_iter()
                                .map(move |ordinal| (ordinal, identity.clone())),
                        )
                    })
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect();
                Some((name.to_owned(), identities))
            })
            .collect()
    }

    fn checked_union_bodies(
        &self,
        file: &SelectedFile<'_>,
        owner: &SyntaxSymbol,
    ) -> Option<BTreeSet<ByteRange>> {
        let written = owner.body_range?;
        let at = |start| ByteRange { start, end: start };
        let captured = self
            .body_members
            .range(at(written.start)..at(written.end))
            .filter_map(|(_, member)| {
                (written.encloses(member.body) && written.encloses(member.holder))
                    .then_some(member.body)
            })
            .collect::<BTreeSet<_>>();
        let mut bodies: Vec<ByteRange> = Vec::new();
        for body in captured {
            if let Some(outer) = bodies.last() {
                if outer.encloses(body) {
                    continue;
                }
                if outer.end > body.start {
                    return None;
                }
            }
            bodies.push(body);
        }
        let source_between = |start: u64, end: u64| {
            let start = usize::try_from(start).ok()?;
            let end = usize::try_from(end).ok()?;
            file.source.get(start..end).map(str::trim)
        };
        if bodies.len() < 2
            || !matches!(
                source_between(written.start, bodies.first()?.start)?,
                "" | "|"
            )
            || !source_between(bodies.last()?.end, written.end)?.is_empty()
            || !bodies
                .windows(2)
                .all(|pair| source_between(pair[0].end, pair[1].start) == Some("|"))
        {
            return None;
        }
        Some(bodies.into_iter().collect())
    }

    fn checked_union_branches(
        &self,
        file: &SelectedFile<'_>,
        owner: &SyntaxSymbol,
        members: &[&SyntaxSymbol],
        bodies: &BTreeSet<ByteRange>,
    ) -> Option<BTreeMap<ByteRange, Vec<usize>>> {
        let mut branches = BTreeMap::<ByteRange, Vec<usize>>::new();
        for member in members {
            let body = self.body_members.get(&member.item_range)?;
            if member.kind != "property"
                || member.syntax_element_kind != Some("property_signature")
                || member.container.as_deref() != Some(owner.qualified_name.as_str())
                || self.of(member, file) != Scope::Body(body.body, false)
                || !bodies.contains(&body.body)
                || !body.body.encloses(member.item_range)
            {
                return None;
            }
            branches
                .entry(body.body)
                .or_default()
                .push(member.declaration_ordinal);
        }
        (branches.len() > 1).then_some(branches)
    }

    /// A captured private name can keep its scope in a checked file even if its
    /// containing declaration has no established identity.
    pub(super) fn unplaced_lexical_bindings(
        &self,
        file: &SelectedFile<'_>,
        anchors: &BTreeMap<String, SymbolId>,
        earlier: &ScopedIdentityAnchors,
    ) -> ScopedIdentityAnchors {
        if file.syntax.language().name != "javascript"
            || file.syntax.has_errors()
            || file.syntax.left_out_declaration_count() != 0
        {
            return BTreeMap::new();
        }
        let Some(module) = anchors
            .get(".")
            .and_then(|id| SymbolIdentity::parse(id.as_str()).ok())
        else {
            return BTreeMap::new();
        };
        let type_names = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.kinds.contains(&SymbolKind::Type))
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<BTreeSet<_>>();
        let owners = EnclosingDefinitions::new(file.syntax.symbols().iter().filter_map(|symbol| {
            let name = symbol.qualified_name.as_str();
            if !symbol.kinds.contains(&SymbolKind::Callable)
                || symbol.kinds.contains(&SymbolKind::Public)
                || anchors.contains_key(name)
                || type_names.contains(name)
            {
                return None;
            }
            Some((
                symbol.item_range.start,
                symbol.item_range.end,
                (name, symbol.body_range?),
            ))
        }));
        let candidates = file
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| {
                private_variable_declaration(symbol)
                    && !anchors.contains_key(&symbol.qualified_name)
                    && !earlier.contains_key(&symbol.qualified_name)
            })
            .filter_map(|symbol| {
                let scope = self.checked_lexical_scope(file, symbol)?;
                let &(owner, body) = owners.resolve(scope.start, scope.end)?;
                let name = symbol.name_range?;
                let spelling = file
                    .source
                    .get(usize::try_from(name.start).ok()?..usize::try_from(name.end).ok()?)?;
                (symbol.container.as_deref() == Some(owner)
                    && body.encloses(symbol.item_range)
                    && scope.encloses(symbol.item_range)
                    && spelling == symbol.name)
                    .then_some((scope, symbol))
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return BTreeMap::new();
        }
        let scopes = self
            .bound
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .enumerate()
            .map(|(ordinal, scope)| (scope, ordinal))
            .collect::<BTreeMap<_, _>>();
        let mut placed = ScopedIdentityAnchors::new();
        for (scope, symbol) in candidates {
            let Some(ordinal) = scopes.get(&scope) else {
                continue;
            };
            let mut path = module.qualified_path().to_vec();
            path.push(format!("$scope#{ordinal}"));
            path.push(symbol.name.clone());
            if let Ok(identity) =
                SymbolIdentity::new(module.owner().clone(), module.language().clone(), path)
                && let Ok(id) = SymbolId::new(identity.wire_identity())
            {
                placed
                    .entry(symbol.qualified_name.clone())
                    .or_default()
                    .insert(symbol.declaration_ordinal, id);
            }
        }
        placed
    }

    fn checked_lexical_scope(
        &self,
        file: &SelectedFile<'_>,
        symbol: &SyntaxSymbol,
    ) -> Option<ByteRange> {
        let captured = *self.bound.get(&symbol.name_range?)?;
        let scope = self.lexical(symbol, file.source);
        (captured == scope && scope != self.source).then_some(scope)
    }

    fn lexical_groups(
        &self,
        file: &SelectedFile<'_>,
        declarations: &[&SyntaxSymbol],
    ) -> Option<Vec<Vec<usize>>> {
        let mut groups = BTreeMap::<ByteRange, Vec<usize>>::new();
        for &symbol in declarations {
            let scope = self.checked_lexical_scope(file, symbol)?;
            groups
                .entry(scope)
                .or_default()
                .push(symbol.declaration_ordinal);
        }
        let mut groups = groups.into_values().collect::<Vec<_>>();
        groups.sort_by_key(|ordinals| ordinals.first().copied());
        Some(groups)
    }

    fn of(&self, symbol: &SyntaxSymbol, file: &SelectedFile<'_>) -> Scope {
        let container = symbol.container.as_deref();
        if let Some(row) = self.body_members.get(&symbol.item_range) {
            let fixed = symbol.kinds.contains(&SymbolKind::Static);
            let held = container.is_some_and(|container| {
                self.held.get(&(container, is_type_member(symbol))) == Some(&Some(row.body))
            });
            return match (held, fixed) {
                (false, fixed) => Scope::Body(row.body, fixed),
                (true, false) => Scope::Member,
                (true, true) => Scope::Static,
            };
        }
        let assigned = symbol.syntax_element_kind == Some("assignment_expression");
        let Some(container) = container else {
            // An assignment without a container writes an export of the module.
            return Scope::Lexical(if assigned {
                self.source
            } else {
                self.lexical(symbol, file.source)
            });
        };
        if matches!(
            symbol.kind,
            "method" | "property" | "field" | "enum_member" | "constructor"
        ) {
            let own = if assigned {
                self.constructors.contains(container) && !writes_instance(symbol, file.source)
            } else {
                symbol.kinds.contains(&SymbolKind::Static)
            };
            return if own { Scope::Static } else { Scope::Member };
        }
        let scope = self.lexical(symbol, file.source);
        let exported = symbol.kinds.contains(&SymbolKind::Public)
            || crate::static_resolution::is_declaration_file(file.path.as_str());
        if exported && self.namespaces.contains(&(container, scope.end)) {
            return Scope::Static;
        }
        Scope::Lexical(scope)
    }

    /// The innermost scope of the name, with the whole file for the module's own scope.
    fn lexical(&self, symbol: &SyntaxSymbol, source: &str) -> ByteRange {
        let scope = symbol
            .name_range
            .and_then(|name| self.bound.get(&name))
            .or_else(|| {
                // The declaration's own scope can span the same range as the declaration.
                self.enclosing.resolve(
                    symbol.item_range.start.saturating_sub(1),
                    symbol.item_range.end.saturating_add(1),
                )
            });
        let Some(scope) = scope else {
            return self.source;
        };
        let outside = |range: std::ops::Range<u64>| {
            usize::try_from(range.start)
                .ok()
                .zip(usize::try_from(range.end).ok())
                .and_then(|(start, end)| source.get(start..end))
                .is_some_and(|text| text.trim().is_empty())
        };
        if outside(0..scope.start) && outside(scope.end..self.source.end) {
            return self.source;
        }
        *scope
    }
}

/// The physical callable or module that can own one repeated literal property.
struct LiteralOwners<'source> {
    enclosing: EnclosingDefinitions<(ByteRange, Option<&'source SyntaxSymbol>)>,
    callbacks: BTreeSet<ByteRange>,
    invoked_initializers: BTreeMap<ByteRange, &'source str>,
    calls: BTreeMap<u64, Vec<&'source StaticCall>>,
    bound_calls: BTreeSet<ByteRange>,
    returned: BTreeMap<ByteRange, Option<&'source str>>,
    object_methods: BTreeMap<ByteRange, &'source SyntaxSymbol>,
    array_initializers: BTreeMap<&'source str, ByteRange>,
    assigned_arrays: BTreeMap<ByteRange, &'source str>,
    assigned_literal_bodies: BTreeMap<ByteRange, &'source str>,
}

impl<'source> LiteralOwners<'source> {
    fn new(
        file: &SelectedFile<'source>,
        facts: &'source StaticFacts,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> Self {
        let symbols = file.syntax.symbols();
        let source = file.source;
        let receiver_scopes = facts
            .function_scopes()
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let values = facts
            .function_values()
            .iter()
            .filter(|(function, _)| !receiver_scopes.contains(function))
            .map(|&(function, value)| (value, function))
            .collect::<BTreeMap<_, _>>();
        let unique = unique_declarations(symbols);
        let functions = literal_callable_owners(&unique, facts, anchors, &values);
        let object_methods = literal_object_method_owners(&unique, facts, anchors);
        let array_initializers = array_literal_owners(file, &unique, anchors);
        let mut initializers = BTreeMap::new();
        for function in functions.values() {
            let range = match function.syntax_element_kind {
                Some("variable_declarator") => function
                    .body_range
                    .and_then(|value| values.get(&value).copied()),
                Some("assignment_expression") => closed_callable_scope(facts, function),
                _ => None,
            };
            if let Some(range) = range {
                initializers
                    .entry(range)
                    .and_modify(|owner| *owner = None)
                    .or_insert(Some(*function));
            }
        }
        let enclosing = EnclosingDefinitions::new(
            symbols
                .iter()
                .filter(|symbol| {
                    symbol.kind == "class" || symbol.kinds.contains(&SymbolKind::Callable)
                })
                .map(|symbol| {
                    let function = symbol
                        .name_range
                        .and_then(|name| functions.get(&name).copied())
                        .filter(|owner| owner.item_range == symbol.item_range);
                    let scope = symbol.item_range;
                    (scope.start, scope.end, (scope, function))
                })
                .chain(
                    facts
                        .function_scopes()
                        .iter()
                        .copied()
                        .chain(facts.function_values().iter().map(|&(scope, _)| scope))
                        .chain(facts.static_blocks().iter().copied())
                        .map(|scope| {
                            let owner = initializers.get(&scope).copied().flatten();
                            (scope.start, scope.end, (scope, owner))
                        }),
                ),
        );
        let mut calls = BTreeMap::<u64, Vec<_>>::new();
        for call in facts.calls() {
            calls.entry(call.range.end).or_default().push(call);
        }
        let assigned_arrays =
            assigned_literals::array_owners(file, facts, anchors, &unique, &enclosing);
        let assigned_literal_bodies =
            assigned_literals::local_assignment_owners(file, facts, anchors, &unique, &enclosing);
        Self {
            enclosing,
            callbacks: facts
                .function_contexts()
                .iter()
                .filter_map(|&(function, context)| {
                    let last = usize::try_from(function.end.checked_sub(1)?).ok()?;
                    // A function range ends at its written body. Keep the existing refusal
                    // for a concise callback whose call is its expression body.
                    (matches!(context, StaticFunctionContext::Argument { .. })
                        && source.as_bytes().get(last) == Some(&b'}'))
                    .then_some(function)
                })
                .collect(),
            invoked_initializers: invoked_initializer_owners(file, facts, &unique, anchors),
            calls,
            bound_calls: facts
                .destructured()
                .iter()
                .map(|&(_, _, call)| call)
                .collect(),
            returned: returned_literal_owners(&functions, &object_methods, facts),
            object_methods: object_methods
                .into_values()
                .map(|method| (method.item_range, method))
                .collect(),
            array_initializers,
            assigned_arrays,
            assigned_literal_bodies,
        }
    }

    /// Requires the direct argument and its nearest placed class method.
    fn proves_typed_method_argument(
        &self,
        property: &SyntaxSymbol,
        member: &StaticBodyMember,
    ) -> bool {
        let Some((_, Some(method))) = self.enclosing.resolve(member.body.start, member.body.end)
        else {
            return false;
        };
        method.syntax_element_kind == Some("method_definition")
            && property.container.as_deref() == Some(method.qualified_name.as_str())
            && self.calls.get(&member.holder.end).is_some_and(|calls| {
                calls.iter().any(|call| {
                    call.callee == StaticCallee::Direct
                        && call.callee_range.end <= member.holder.start
                        && call.range.encloses(member.holder)
                        && method
                            .body_range
                            .is_some_and(|body| body.encloses(call.range))
                        && !self.bound_calls.contains(&call.range)
                })
            })
    }

    fn proves_body(&self, property: &SyntaxSymbol, member: &StaticBodyMember) -> bool {
        let scope = self
            .enclosing
            .resolve(member.body.start, member.body.end)
            .copied();
        let container = property.container.as_deref();
        if let Some(owner) = container
            && (self.returned.get(&member.body) == Some(&Some(owner))
                || self.array_initializers.get(owner) == Some(&member.holder)
                || self.assigned_arrays.get(&member.holder) == Some(&owner)
                || self.assigned_literal_bodies.get(&member.body) == Some(&owner))
        {
            return true;
        }
        self.calls.get(&member.holder.end).is_some_and(|calls| {
            calls.iter().any(|call| {
                call.callee_range.end <= member.holder.start
                    && call.range.encloses(member.holder)
                    && match (container, scope) {
                        (None, None) => property.qualified_name == property.name,
                        (Some(owner), Some((_, Some(function)))) => {
                            function.qualified_name == owner
                                && function
                                    .body_range
                                    .is_some_and(|body| body.encloses(call.range))
                                && matches!(
                                    call.callee,
                                    StaticCallee::Direct | StaticCallee::Member { .. }
                                )
                                && !self.bound_calls.contains(&call.range)
                        }
                        (container, Some((scope, None))) if self.callbacks.contains(&scope) => {
                            scope.encloses(call.range)
                                && matches!(
                                    call.callee,
                                    StaticCallee::Direct | StaticCallee::Member { .. }
                                )
                                && !self.bound_calls.contains(&call.range)
                                && self.callback_literal_container(scope, container)
                        }
                        (Some(owner), Some((scope, None))) => {
                            self.invoked_initializers.get(&scope) == Some(&owner)
                                && scope.encloses(call.range)
                                && matches!(
                                    call.callee,
                                    StaticCallee::Direct | StaticCallee::Member { .. }
                                )
                                && !self.bound_calls.contains(&call.range)
                        }
                        _ => false,
                    }
            })
        })
    }

    fn callback_literal_container(&self, scope: ByteRange, container: Option<&str>) -> bool {
        let Some(mut owners) = self.enclosing.containing(scope.start, scope.end) else {
            return false;
        };
        match owners.find(|(body, _)| *body != scope) {
            Some((_, Some(owner))) => container == Some(owner.qualified_name.as_str()),
            None | Some((_, None)) => false,
        }
    }
}

/// Finds the function scope of a uniquely placed variable's immediate invocation.
fn invoked_initializer_owners<'source>(
    file: &SelectedFile<'source>,
    facts: &StaticFacts,
    unique: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeMap<ByteRange, &'source str> {
    let expressions = facts
        .function_values()
        .iter()
        .map(|&(scope, value)| (value, scope))
        .collect::<BTreeMap<_, _>>();
    let invocations = facts
        .calls()
        .iter()
        .filter(|call| {
            call.callee == StaticCallee::Unsupported && call.range.start == call.callee_range.start
        })
        .filter_map(|call| Some((call.range, *expressions.get(&call.callee_range)?)))
        .collect::<BTreeMap<_, _>>();
    if invocations.is_empty() {
        return BTreeMap::new();
    }
    let writes = facts
        .references()
        .iter()
        .filter(|reference| reference.role == ReferenceRole::Write)
        .filter_map(|reference| binding_name(file.source, reference.target))
        .collect::<BTreeSet<_>>();
    unique
        .values()
        .flatten()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("variable_declarator")
                && symbol.kind == "variable"
                && symbol.container.is_none()
                && anchors.contains_key(&symbol.qualified_name)
                && !writes.contains(symbol.name.as_str())
        })
        .filter_map(|symbol| {
            let initializer = symbol.body_range?;
            let scope = *invocations.get(&initializer)?;
            (symbol.item_range.encloses(initializer) && initializer.encloses(scope))
                .then_some((scope, symbol.qualified_name.as_str()))
        })
        .collect()
}

/// Selects placed declarations whose initializer is the written array itself.
fn array_literal_owners<'source>(
    file: &SelectedFile<'source>,
    unique: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeMap<&'source str, ByteRange> {
    let assigned_exports = file
        .syntax
        .export_bindings()
        .unwrap_or_default()
        .iter()
        .filter_map(|binding| {
            let name = binding.exported?;
            (binding.kind == SyntaxExportKind::Named
                && binding.local == Some(name)
                && binding.source.is_none()
                && binding.container.is_none()
                && !binding.type_only)
                .then_some((binding.range, name))
        })
        .collect::<BTreeSet<_>>();
    unique
        .iter()
        .filter_map(|(&name, &declaration)| {
            let declaration = declaration?;
            let value = declaration.body_range?;
            let initializer = match declaration.syntax_element_kind {
                Some("variable_declarator") => true,
                Some("assignment_expression") => {
                    declaration.container.is_none()
                        && declaration.kinds.contains(&SymbolKind::Public)
                        && assigned_exports
                            .contains(&(declaration.item_range, declaration.name_range?))
                }
                _ => false,
            };
            (initializer
                && declaration.kind == "variable"
                && declaration.item_range.encloses(value)
                && anchors.contains_key(name)
                && written_array(file.source, value))
            .then_some((name, value))
        })
        .collect()
}

fn written_array(source: &str, range: ByteRange) -> bool {
    usize::try_from(range.start)
        .ok()
        .zip(usize::try_from(range.end).ok())
        .and_then(|(start, end)| source.get(start..end))
        .is_some_and(|value| value.starts_with('[') && value.ends_with(']'))
}

/// Names callables with one authored and placed declaration.
fn literal_callable_owners<'source>(
    unique: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    facts: &StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
    values: &BTreeMap<ByteRange, ByteRange>,
) -> BTreeMap<ByteRange, &'source SyntaxSymbol> {
    let classes = unique
        .values()
        .flatten()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("class_declaration")
                && anchors.contains_key(&symbol.qualified_name)
        })
        .map(|symbol| (symbol.item_range, symbol.qualified_name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let class_methods = facts
        .body_members()
        .iter()
        .filter_map(|member| Some((member.member, *classes.get(&member.body)?)))
        .collect::<BTreeMap<_, _>>();
    unique
        .values()
        .flatten()
        .copied()
        .filter(|symbol| {
            anchors.contains_key(&symbol.qualified_name)
                && match symbol.syntax_element_kind {
                    Some("function_declaration") => true,
                    Some("assignment_expression") => closed_callable_scope(facts, symbol).is_some(),
                    Some("variable_declarator") if symbol.kind == "variable" => {
                        symbol.kinds.contains(&SymbolKind::Callable)
                            && symbol.body_range.is_some_and(|value| {
                                symbol.item_range.encloses(value) && values.contains_key(&value)
                            })
                    }
                    Some("method_definition") if symbol.kind == "method" => {
                        class_methods.get(&symbol.item_range).copied()
                            == symbol.container.as_deref()
                            && symbol.container.is_some()
                    }
                    _ => false,
                }
        })
        .filter_map(|symbol| Some((symbol.name_range?, symbol)))
        .collect()
}

/// The unique physical function expression of an already placed anonymous default callable.
fn closed_callable_scope(facts: &StaticFacts, symbol: &SyntaxSymbol) -> Option<ByteRange> {
    let export = facts.commonjs_export()?;
    let callable = export.anonymous_callable?;
    (symbol.name == "default"
        && symbol.kind == "function"
        && symbol.item_range == export.assignment
        && callable.encloses(symbol.body_range?))
    .then_some(callable)
}

/// Names a placed method whose literal is the entire value of one checked declaration.
fn literal_object_method_owners<'source>(
    unique: &BTreeMap<&'source str, Option<&'source SyntaxSymbol>>,
    facts: &StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeMap<ByteRange, &'source SyntaxSymbol> {
    let mut holders = BTreeMap::new();
    for declaration in unique.values().flatten() {
        let Some(body) = declaration.body_range else {
            continue;
        };
        if matches!(
            declaration.syntax_element_kind,
            Some("variable_declarator" | "assignment_expression" | "pair")
        ) && matches!(declaration.kind, "variable" | "property")
            && declaration.item_range.encloses(body)
            && anchors.contains_key(&declaration.qualified_name)
        {
            holders
                .entry((declaration.item_range, body))
                .and_modify(|owner| *owner = None)
                .or_insert(Some(declaration.qualified_name.as_str()));
        }
    }
    let members = facts
        .body_members()
        .iter()
        .filter_map(|member| {
            Some((
                member.member,
                (*holders.get(&(member.holder, member.body))?)?,
            ))
        })
        .collect::<BTreeMap<_, _>>();
    unique
        .values()
        .flatten()
        .copied()
        .filter_map(|method| {
            let owner = members.get(&method.item_range)?;
            (method.syntax_element_kind == Some("method_definition")
                && method.kind == "method"
                && !method.name.starts_with('[')
                && method.container.as_deref() == Some(owner)
                && anchors.contains_key(&method.qualified_name))
            .then_some((method.name_range?, method))
        })
        .collect()
}

/// Selects written literal returns of a placed function or method.
fn returned_literal_owners<'source>(
    declarations: &BTreeMap<ByteRange, &'source SyntaxSymbol>,
    object_methods: &BTreeMap<ByteRange, &'source SyntaxSymbol>,
    facts: &StaticFacts,
) -> BTreeMap<ByteRange, Option<&'source str>> {
    let mut owners = BTreeMap::new();
    let bodies = facts.returned_objects().iter().copied().chain(
        facts
            .returned_branch_objects()
            .iter()
            .map(|&(name, object)| (name, Some(object))),
    );
    for (name, object) in bodies {
        let owner = declarations
            .get(&name)
            .or_else(|| object_methods.get(&name));
        let Some((owner, body)) = owner.zip(object) else {
            continue;
        };
        let named = match owner.syntax_element_kind {
            Some("function_declaration" | "variable_declarator") => true,
            Some("method_definition") => !owner.name.starts_with('['),
            _ => false,
        };
        if !named || !owner.item_range.encloses(body) {
            continue;
        }
        let name = owner.qualified_name.as_str();
        owners
            .entry(body)
            .and_modify(|owner| {
                if *owner != Some(name) {
                    *owner = None;
                }
            })
            .or_insert(Some(name));
    }
    owners
}

fn literal_property_identity(owner: &SymbolIdentity, name: &str) -> Option<SymbolId> {
    let mut path = owner.qualified_path().to_vec();
    path.push(name.to_owned());
    let identity =
        SymbolIdentity::new(owner.owner().clone(), owner.language().clone(), path).ok()?;
    SymbolId::new(identity.wire_identity()).ok()
}

fn ordinary_private_property(symbol: &SyntaxSymbol, source: &str) -> bool {
    if symbol.kind != "property"
        || symbol.kinds.contains(&SymbolKind::Public)
        || !matches!(
            symbol.syntax_element_kind,
            Some("pair" | "shorthand_property_identifier")
        )
    {
        return false;
    }
    let name = symbol.name_range.and_then(|range| {
        let start = usize::try_from(range.start).ok()?;
        let end = usize::try_from(range.end).ok()?;
        source.get(start..end)
    });
    name.is_some_and(|name| name == symbol.name && !name.contains(['"', '\'', '`']))
}

/// Whether a type literal or an interface declares the member.
fn is_type_member(symbol: &SyntaxSymbol) -> bool {
    matches!(
        symbol.syntax_element_kind,
        Some("property_signature" | "method_signature")
    )
}

/// Whether an assignment writes `this.name` or `Owner.prototype.name`, rather than the
/// owner's own `Owner.name`.
fn writes_instance(symbol: &SyntaxSymbol, source: &str) -> bool {
    let receiver = symbol
        .name_range
        .and_then(|name| {
            let start = usize::try_from(symbol.item_range.start).ok()?;
            let end = usize::try_from(name.start).ok()?;
            source.get(start..end)
        })
        .and_then(|written| written.trim_end().strip_suffix('.'));
    receiver.is_some_and(|receiver| receiver == "this" || receiver.ends_with(".prototype"))
}
