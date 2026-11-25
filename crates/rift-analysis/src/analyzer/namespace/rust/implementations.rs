//! Places implementations written outside their type's module on the type's identity.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::SymbolId;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBindingKind, StaticFacts, SyntaxSymbol};

use super::super::{Anchors, SelectedFile};
use super::{Modules, macros, source_identifier};
use crate::NamespaceInput;

/// Import bindings followed for one written type path; a longer chain stays unresolved.
const IMPORTS_MAX: usize = 64;

/// Segments a followed path keeps; import bindings that lengthen it further are a cycle.
const PATH_SEGMENTS_MAX: usize = 64;

/// Declarations of the captured library modules that a written type path can name.
struct Types {
    /// Paths of the type declarations of modules that are not inactive.
    declared: BTreeMap<Vec<String>, Declared>,
    /// Paths of the modules with whether the configuration admits one declaration of each:
    /// `Some(true)` when it does, `Some(false)` when every one is inactive.
    modules: BTreeMap<Vec<String>, Option<bool>>,
    /// Modules with a glob import that is not inactive.
    globs: BTreeSet<Vec<String>>,
    /// What a module's `use` declarations bind; `None` when an unknown condition or a
    /// second binding of the name leaves the target open.
    bindings: BTreeMap<Vec<String>, Option<Vec<String>>>,
}

/// The type declarations of one path.
struct Declared {
    /// Whether every one of them is a struct, enum, or union.
    nominal: bool,
    /// Whether the configuration admits one of them.
    admitted: bool,
}

/// Counts the declarations, bindings, and path steps one placement pass reads.
struct Budget {
    left: usize,
}

impl Budget {
    fn charge(&mut self, count: usize) -> Option<()> {
        self.left = self.left.checked_sub(count)?;
        Some(())
    }
}

/// Rewrites the anchors of each implementation, and of its members, whose written type
/// names one package-local struct, enum, or union declared in another module.
///
/// An implementation keeps the identity under its own module when the type comes from a
/// glob import, a dependency, a type alias, a type parameter, or an import whose condition
/// is unknown, when two bindings compete, when an import or a glob competes with a type or a
/// module whose own condition is unknown, or when it sits in a block. The implementation's own
/// condition does not move it: an inactive one belongs to the type its imports name.
/// Exhausting the work bound leaves every implementation in place.
pub(super) fn place(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
    file_modules: &BTreeMap<String, Vec<String>>,
    modules: &Modules<'_, '_>,
    anchors: &mut Anchors,
) {
    let Some(left) = input
        .syntax()
        .syntax_elements_max()
        .checked_mul(input.syntax().syntax_depth_max())
    else {
        return;
    };
    let mut budget = Budget { left };
    let Some(placed) = placements(selected, file_modules, modules, anchors, &mut budget) else {
        return;
    };
    for (file, name, owner) in placed {
        let Some(anchors) = anchors.get_mut(file) else {
            continue;
        };
        if let Some(moved) = moved_anchors(input, anchors, name, &owner) {
            anchors.extend(moved);
        }
    }
}

/// The identities of `name` and its members under `owner`, or none when one is invalid.
fn moved_anchors(
    input: &NamespaceInput<'_>,
    anchors: &BTreeMap<String, SymbolId>,
    name: &str,
    owner: &[String],
) -> Option<Vec<(String, SymbolId)>> {
    let depth = SymbolIdentity::parse(anchors.get(name)?.as_str())
        .ok()?
        .qualified_path()
        .len();
    let member = format!("{name}::");
    std::iter::once(name)
        .chain(
            anchors
                .range(member.clone()..)
                .map(|(key, _)| key.as_str())
                .take_while(|key| key.starts_with(&member)),
        )
        .map(|key| {
            let previous = SymbolIdentity::parse(anchors.get(key)?.as_str()).ok()?;
            let mut path = owner.to_vec();
            path.extend_from_slice(previous.qualified_path().get(depth..)?);
            let identity =
                SymbolIdentity::new(input.owner().clone(), previous.language().clone(), path)
                    .ok()?;
            Some((
                key.to_owned(),
                SymbolId::new(identity.wire_identity()).ok()?,
            ))
        })
        .collect()
}

/// Each implementation name of a file whose owner is another module's type, with the type's
/// path. `None` when the work bound is exhausted.
fn placements<'source>(
    selected: &'source [SelectedFile<'source>],
    file_modules: &BTreeMap<String, Vec<String>>,
    modules: &Modules<'_, '_>,
    anchors: &Anchors,
    budget: &mut Budget,
) -> Option<Vec<(&'source str, &'source str, Vec<String>)>> {
    let mut files = Vec::new();
    for file in selected {
        let path = file.path.as_str();
        if let Some(facts) = file.syntax.static_facts()
            && let Some(module) = file_modules.get(path)
            && let Some(anchors) = anchors.get(path)
            && file.syntax.language().name == "rust"
        {
            files.push((file, facts, module.as_slice(), anchors));
        }
    }
    let mut types = Types {
        declared: BTreeMap::new(),
        modules: BTreeMap::new(),
        globs: BTreeSet::new(),
        bindings: BTreeMap::new(),
    };
    for (file, facts, module, anchors) in &files {
        types.index(file, facts, module, anchors, modules, budget)?;
    }
    let mut placed = Vec::new();
    for (file, facts, module, anchors) in &files {
        let written = WrittenOwners::new(file, facts, module, budget)?;
        let mut owners = BTreeMap::<&str, Option<Vec<String>>>::new();
        let items = module_items(file)
            .map(|symbol| symbol.item_range)
            .collect::<BTreeSet<_>>();
        for symbol in file.syntax.symbols() {
            budget.charge(1)?;
            let name = symbol.qualified_name.as_str();
            if symbol.kind != "impl_item" || !items.contains(&symbol.item_range) {
                // A declaration of the name in this file owns it already, and an
                // implementation in a block reads the block's own names.
                owners.insert(name, None);
                continue;
            }
            let owner = written
                .path(file, anchors, symbol)
                .and_then(|path| types.named(path, budget));
            budget.charge(1)?;
            owners
                .entry(name)
                .and_modify(|previous| {
                    if *previous != owner {
                        *previous = None;
                    }
                })
                .or_insert(owner);
        }
        placed.extend(owners.into_iter().filter_map(|(name, owner)| {
            let owner = owner?;
            let previous = SymbolIdentity::parse(anchors.get(name)?.as_str()).ok()?;
            (previous.qualified_path() != owner).then(|| (file.path.as_str(), name, owner))
        }));
    }
    Some(placed)
}

/// The types one file's implementations write, by implementation range.
struct WrittenOwners<'source> {
    module: &'source [String],
    implementations: BTreeMap<ByteRange, ByteRange>,
    parameters: BTreeSet<(ByteRange, &'source str)>,
    inline_modules: BTreeSet<&'source str>,
}

impl<'source> WrittenOwners<'source> {
    fn new(
        file: &SelectedFile<'source>,
        facts: &'source StaticFacts,
        module: &'source [String],
        budget: &mut Budget,
    ) -> Option<Self> {
        let mut owners = Self {
            module,
            implementations: BTreeMap::new(),
            parameters: BTreeSet::new(),
            inline_modules: BTreeSet::new(),
        };
        for implementation in facts.inherent_impls() {
            budget.charge(1)?;
            owners
                .implementations
                .insert(implementation.range, implementation.type_path);
        }
        for binding in facts.bindings() {
            budget.charge(1)?;
            if binding.kind == StaticBindingKind::RustTypeParameter
                && let Some(name) = macros::text_range(file.source, binding.name)
            {
                owners.parameters.insert((binding.scope, name));
            }
        }
        for symbol in file.syntax.symbols() {
            budget.charge(1)?;
            if symbol.kind == "module" {
                owners.inline_modules.insert(&symbol.qualified_name);
            }
        }
        Some(owners)
    }

    /// The crate path an implementation's written type spells from its module.
    fn path(
        &self,
        file: &SelectedFile<'_>,
        anchors: &BTreeMap<String, SymbolId>,
        symbol: &SyntaxSymbol,
    ) -> Option<Vec<String>> {
        let range = symbol.item_range;
        let written = macros::text_range(file.source, *self.implementations.get(&range)?)?;
        let segments = written
            .split("::")
            .map(|segment| segment.trim().to_owned())
            .collect::<Vec<_>>();
        if self
            .parameters
            .contains(&(range, segments.first()?.as_str()))
        {
            return None;
        }
        match symbol.container.as_deref() {
            None => absolute_path(self.module, &segments),
            Some(container) if self.inline_modules.contains(container) => {
                absolute_path(&anchored_path(anchors, container)?, &segments)
            }
            Some(_) => None,
        }
    }
}

impl Types {
    fn index(
        &mut self,
        file: &SelectedFile<'_>,
        facts: &StaticFacts,
        module: &[String],
        anchors: &BTreeMap<String, SymbolId>,
        modules: &Modules<'_, '_>,
        budget: &mut Budget,
    ) -> Option<()> {
        let mut scopes = BTreeMap::new();
        self.declare_module(module.to_vec(), modules.file(file));
        for symbol in module_items(file) {
            budget.charge(1)?;
            let Some(path) = anchored_path(anchors, &symbol.qualified_name) else {
                continue;
            };
            if symbol.kind == "module" {
                if let Some(body) = symbol.body_range {
                    scopes.insert(body, path.clone());
                }
                self.declare_module(path, modules.module(file, symbol));
                continue;
            }
            if symbol.kind == "impl_item" || !symbol.kinds.contains(&SymbolKind::Type) {
                continue;
            }
            let admitted = modules.declared(file, symbol.item_range);
            if admitted == Some(false) {
                continue;
            }
            let nominal = matches!(
                symbol.kind,
                "struct" | "enum" | "union_item" | "extern_type"
            );
            let declared = self.declared.entry(path).or_insert(Declared {
                nominal,
                admitted: false,
            });
            declared.nominal &= nominal;
            declared.admitted |= admitted == Some(true);
        }
        let bindings = facts
            .bindings()
            .iter()
            .filter_map(|binding| match binding.kind {
                StaticBindingKind::RustUse { path } => Some((path, binding.name)),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        for imported in facts.rust_uses() {
            budget.charge(1)?;
            let admitted = modules.admitted(file, imported.range);
            if admitted == Some(false) {
                continue;
            }
            let module = if file_scope(file.source) == Some(imported.scope) {
                module
            } else if let Some(path) = scopes.get(&imported.scope) {
                path
            } else {
                continue;
            };
            if imported.glob {
                self.globs.insert(module.to_vec());
                continue;
            }
            let Some(name) = bindings
                .get(&imported.range)
                .and_then(|name| macros::text_range(file.source, *name))
                .and_then(source_identifier)
            else {
                continue;
            };
            let target = (admitted == Some(true))
                .then(|| import_path(file.source, module, &imported.path))
                .flatten();
            let mut path = module.to_vec();
            path.push(name);
            self.bindings
                .entry(path)
                .and_modify(|previous| {
                    if *previous != target {
                        *previous = None;
                    }
                })
                .or_insert(target);
        }
        Some(())
    }

    fn declare_module(&mut self, path: Vec<String>, admitted: Option<bool>) {
        self.modules
            .entry(path)
            .and_modify(|status| {
                *status = match (*status, admitted) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                }
            })
            .or_insert(admitted);
    }

    /// The path of the one type `path` names, followed through the modules' import bindings.
    fn named(&self, mut path: Vec<String>, budget: &mut Budget) -> Option<Vec<String>> {
        for _ in 0..IMPORTS_MAX {
            budget.charge(path.len())?;
            // The longest bound prefix is the innermost module that imports the next name.
            // An import that shares the name of an admitted module binds another namespace.
            let bound = (1..=path.len()).rev().find(|length| {
                let prefix = &path[..*length];
                self.bindings.contains_key(prefix) && self.modules.get(prefix) != Some(&Some(true))
            });
            if let Some(declared) = self.declared.get(&path) {
                // Beside a declaration with an unknown condition an import or a glob can be
                // the type of the other configuration.
                let competes = !declared.admitted
                    && (bound.is_some()
                        || (1..path.len()).any(|length| self.globs.contains(&path[..length])));
                return (declared.nominal && !competes).then_some(path);
            }
            let length = bound?;
            if self.modules.get(&path[..length]) == Some(&None) {
                return None;
            }
            let mut next = self.bindings.get(&path[..length])?.clone()?;
            next.extend_from_slice(&path[length..]);
            if next.len() > PATH_SEGMENTS_MAX || next == path {
                return None;
            }
            path = next;
        }
        None
    }
}

/// The declarations whose innermost scope is the file or an inline module's body. A block
/// of a function, constant, static, or any other declaration holds the rest.
fn module_items<'source>(
    file: &SelectedFile<'source>,
) -> impl Iterator<Item = &'source SyntaxSymbol> {
    let mut enclosing = Vec::<&SyntaxSymbol>::new();
    file.syntax.symbols().iter().filter(move |symbol| {
        while enclosing
            .last()
            .is_some_and(|parent| parent.item_range.end <= symbol.item_range.start)
        {
            enclosing.pop();
        }
        let item = enclosing.iter().all(|parent| parent.kind == "module");
        enclosing.push(symbol);
        item
    })
}

/// The range `use` declarations outside every inline module apply to: the file from its
/// first token on.
pub(crate) fn file_scope(source: &str) -> Option<ByteRange> {
    let written =
        source.trim_start_matches(|first: char| first.is_whitespace() || first == '\u{feff}');
    Some(ByteRange {
        start: u64::try_from(source.len() - written.len()).ok()?,
        end: u64::try_from(source.len()).ok()?,
    })
}

fn anchored_path(anchors: &BTreeMap<String, SymbolId>, name: &str) -> Option<Vec<String>> {
    let identity = SymbolIdentity::parse(anchors.get(name)?.as_str()).ok()?;
    Some(identity.qualified_path().to_vec())
}

/// The crate path one `use` leaf names from `module`.
fn import_path(source: &str, module: &[String], ranges: &[ByteRange]) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    for range in ranges {
        let text = macros::text_range(source, *range)?;
        segments.extend(text.split("::").map(|segment| segment.trim().to_owned()));
    }
    if segments.len() > 1 && segments.last().is_some_and(|last| last == "self") {
        segments.pop();
    }
    absolute_path(module, &segments)
}

/// A written path read from `module`: `crate`, `self`, and each leading `super` as written;
/// any other first segment is a name of the module. Edition 2015 reads such a `use` path from
/// the crate root; there it names nothing and the implementation keeps its module.
fn absolute_path(module: &[String], segments: &[String]) -> Option<Vec<String>> {
    let mut rest = segments;
    let mut base = module;
    match rest.first()?.as_str() {
        "crate" => {
            base = module.get(..1)?;
            rest = &rest[1..];
        }
        "self" => rest = &rest[1..],
        _ => {}
    }
    while rest.first().is_some_and(|segment| segment == "super") {
        let (_, parent) = base.split_last()?;
        if parent.is_empty() {
            return None;
        }
        base = parent;
        rest = &rest[1..];
    }
    let mut path = base.to_vec();
    for segment in rest {
        path.push(source_identifier(segment)?);
    }
    Some(path)
}
