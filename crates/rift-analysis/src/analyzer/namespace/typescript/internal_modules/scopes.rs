//! Names one file declares in more than one scope.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBodyMember, StaticFacts, SyntaxFacts, SyntaxSymbol};

use super::SelectedFile;
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

/// The qualified names of `file` that declarations of different scopes share.
pub(super) fn repeated_names<'source>(file: &SelectedFile<'source>) -> BTreeSet<&'source str> {
    let scopes = FileScopes::new(file.syntax, file.source);
    let mut first = BTreeMap::new();
    let mut repeated = BTreeSet::new();
    for symbol in file.syntax.symbols() {
        let scope = scopes.of(symbol, file);
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

struct FileScopes<'source> {
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
    fn new(syntax: &'source SyntaxFacts, source: &str) -> Self {
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
