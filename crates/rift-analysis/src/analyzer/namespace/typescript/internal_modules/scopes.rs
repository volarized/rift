//! Names one file declares in more than one scope.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, ScopedIdentityAnchors, StaticBodyMember, StaticFacts, SyntaxFacts, SyntaxSymbol,
};

use super::{SelectedFile, binding_name};
use crate::EnclosingDefinitions;

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
        let Some(groups) = scopes.private_local_groups(file, &declarations) else {
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

fn private_local_declaration(symbol: &SyntaxSymbol) -> bool {
    symbol.container.is_none()
        && symbol.qualified_name == symbol.name
        && !symbol.kinds.contains(&SymbolKind::Public)
        && matches!(
            symbol.syntax_element_kind,
            Some("function_declaration" | "variable_declarator")
        )
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

    /// Names repeated private keys by their captured literal and physical ordinal.
    pub(super) fn literal_properties(
        &self,
        file: &SelectedFile<'source>,
        repeated: &BTreeSet<&str>,
        anchors: &BTreeMap<String, SymbolId>,
    ) -> ScopedIdentityAnchors {
        if repeated.is_empty()
            || file.syntax.language().name != "javascript"
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
            group
                .iter()
                .all(|symbol| ordinary_private_property(symbol, file.source))
        });
        let Some(facts) = file
            .syntax
            .static_facts()
            .filter(|_| !candidates.is_empty())
        else {
            return BTreeMap::new();
        };
        let nested = EnclosingDefinitions::new(
            symbols
                .iter()
                .filter(|symbol| {
                    symbol.kind == "class" || symbol.kinds.contains(&SymbolKind::Callable)
                })
                .map(|symbol| symbol.item_range)
                .chain(facts.function_scopes().iter().copied())
                .chain(facts.function_values().iter().map(|&(scope, _)| scope))
                .chain(facts.static_blocks().iter().copied())
                .map(|scope| (scope.start, scope.end, ())),
        );
        let mut calls = BTreeMap::<u64, Vec<_>>::new();
        for call in facts.calls() {
            calls.entry(call.range.end).or_default().push(call);
        }
        let returned = returned_literal_owners(symbols, facts, anchors);
        let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
        let mut scoped = BTreeMap::new();
        for (name, declarations) in candidates {
            let mut groups = BTreeMap::<ByteRange, Vec<usize>>::new();
            for declaration in declarations {
                let Some(row) = self.body_members.get(&declaration.item_range) else {
                    groups.clear();
                    break;
                };
                let exact_body = match declaration.container.as_deref() {
                    Some(owner) => returned.get(&row.body) == Some(&Some(owner)),
                    None => {
                        declaration.qualified_name == declaration.name
                            && nested.resolve(row.body.start, row.body.end).is_none()
                            && calls.get(&row.holder.end).is_some_and(|calls| {
                                calls.iter().any(|call| {
                                    call.callee_range.end <= row.holder.start
                                        && call.range.encloses(row.holder)
                                })
                            })
                    }
                };
                if self.of(declaration, file) != Scope::Body(row.body, false) || !exact_body {
                    groups.clear();
                    break;
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
        scoped
    }

    fn private_local_groups(
        &self,
        file: &SelectedFile<'_>,
        declarations: &[&SyntaxSymbol],
    ) -> Option<Vec<Vec<usize>>> {
        let mut groups = BTreeMap::<ByteRange, Vec<usize>>::new();
        for &symbol in declarations {
            if !private_local_declaration(symbol) {
                return None;
            }
            let captured = *self.bound.get(&symbol.name_range?)?;
            let scope = self.lexical(symbol, file.source);
            if captured != scope || scope == self.source {
                return None;
            }
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

/// Selects returned literal ranges from one admitted, physically named function declaration.
fn returned_literal_owners<'source>(
    symbols: &'source [SyntaxSymbol],
    facts: &StaticFacts,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeMap<ByteRange, Option<&'source str>> {
    let mut owners = BTreeMap::new();
    if facts.returned_objects().is_empty() {
        return owners;
    }
    let mut unique = BTreeMap::<&str, Option<&SyntaxSymbol>>::new();
    for symbol in symbols {
        unique
            .entry(&symbol.qualified_name)
            .and_modify(|owner| *owner = None)
            .or_insert(Some(symbol));
    }
    let declarations = unique
        .into_values()
        .flatten()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("function_declaration")
                && anchors.contains_key(&symbol.qualified_name)
        })
        .filter_map(|symbol| Some((symbol.name_range?, symbol)))
        .collect::<BTreeMap<_, _>>();
    for &(name, object) in facts.returned_objects() {
        let Some((owner, body)) = declarations.get(&name).zip(object) else {
            continue;
        };
        if !owner.item_range.encloses(body) {
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
