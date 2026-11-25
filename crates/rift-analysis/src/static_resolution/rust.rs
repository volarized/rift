//! Resolves Rust paths admitted by the captured crate and module inventory.

mod deref;
mod field_sites;
mod receivers;
mod references;
mod value_reads;

// Exhausting initializer lookup depth leaves the authored site unresolved.
const RECEIVER_DEPTH_MAX: usize = 64;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, StaticBindingKind, StaticCall, StaticCallee, StaticRustUse, SyntaxSymbol,
};

use super::{Binding, File, Target, Work};

/// Qualified Rust declarations grouped by their admitted crate and module path.
#[derive(Default)]
pub(super) struct Namespaces<'source> {
    files: BTreeMap<&'source str, FileNamespace<'source>>,
    /// The file scopes where type aliases declare their right-hand types.
    source_files: Arc<BTreeMap<&'source str, File<'source>>>,
    values: BTreeMap<Vec<String>, Option<Target<'source>>>,
    constants_and_statics: BTreeMap<Vec<String>, Option<Target<'source>>>,
    types: BTreeMap<Vec<String>, Option<Target<'source>>>,
    structural_types: BTreeMap<(String, String), Option<Target<'source>>>,
    methods: BTreeMap<Vec<String>, Vec<Method<'source>>>,
    members: BTreeMap<Vec<String>, Option<Target<'source>>>,
    fields: BTreeMap<Vec<String>, Option<Target<'source>>>,
    member_calls: BTreeMap<(&'source str, ByteRange), Option<&'source StaticCall>>,
    field_types: BTreeMap<Vec<String>, Target<'source>>,
    bounds: BTreeMap<(&'source str, ByteRange), Vec<ByteRange>>,
    unavailable: BTreeSet<(&'source str, ByteRange)>,
    return_owners: BTreeMap<(&'source str, ByteRange), CallableReturn<'source>>,
    local_deref: BTreeMap<deref::WrapperKey<'source>, Option<deref::Implementation<'source>>>,
    referents: BTreeMap<(&'source str, ByteRange), (Target<'source>, Target<'source>)>,
    initializers: BTreeMap<(&'source str, ByteRange), Option<&'source StaticCall>>,
    conditional_modules: BTreeSet<Vec<String>>,
    inactive_modules: BTreeSet<Vec<String>>,
    /// Module path of each selected file.
    bases: BTreeSet<Vec<String>>,
    modules: BTreeMap<Vec<String>, Option<Target<'source>>>,
    crate_roots: BTreeMap<String, Option<&'source str>>,
    exports: BTreeMap<Vec<String>, Option<Export<'source>>>,
    glob_imports: BTreeMap<&'source str, Vec<&'source StaticRustUse>>,
}

struct Export<'source> {
    module: Vec<String>,
    target: Vec<String>,
    visibility: Option<&'source str>,
}

struct FileNamespace<'source> {
    base: Vec<String>,
    inline: Vec<(&'source SyntaxSymbol, Vec<String>)>,
}

#[derive(Clone, Copy)]
struct Method<'source> {
    target: Target<'source>,
    implementation: &'source rift_syntax::StaticInherentImpl,
}

#[derive(Clone, Copy)]
enum Namespace {
    Type,
    Value,
    ConstantOrStatic,
}

#[derive(Clone, Copy)]
enum GlobTarget<'source> {
    Resolved(Target<'source>),
    Ambiguous,
}

impl<'source> GlobTarget<'source> {
    fn target(self) -> Option<Target<'source>> {
        match self {
            Self::Resolved(target) => Some(target),
            Self::Ambiguous => None,
        }
    }
}

#[derive(Clone, Copy)]
struct CallableReturn<'source> {
    owner: Target<'source>,
    written: ByteRange,
}

impl<'source> Namespaces<'source> {
    pub(super) fn edge_type_target(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.conditional(range, work) {
            return None;
        }
        self.module(file, range.start, work)?;
        self.type_target(file, range, work)
    }

    fn type_target(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let Some(target) = file.target_at(range)
            && structural_owner(target)
        {
            return self.available(target).then_some(target);
        }
        if file.text(range)? == "Self"
            && let Some(owner) = self.structural_self(file, range.start, work)
        {
            return Some(owner);
        }
        if file.text(range)?.starts_with('(') {
            let crate_name = self.module(file, range.start, work)?.first()?.clone();
            let name = file.text(range)?.split_whitespace().collect::<String>();
            return self
                .structural_types
                .get(&(crate_name, name))
                .copied()
                .flatten()
                .filter(|target| self.available(*target));
        }
        let parts = path_segments(file.text(range)?, work)?;
        if let ["Self", name] = parts.as_slice()
            && let Some(target) = Self::associated_type(file, range.start, name, work)
        {
            return self.available(target).then_some(target);
        }
        if let [name] = parts.as_slice()
            && let Some(target) = self.local_type(file, range.start, name, work).ok()?
        {
            return self.available(target).then_some(target);
        }
        let path = self.resolved_path(file, range.start, &parts, work)?;
        self.lookup(file, range.start, &path, Namespace::Type, work)
            .or_else(|| {
                self.members.get(&path).copied().flatten().filter(|target| {
                    !self.types.contains_key(&path)
                        && target.symbol.kinds.contains(&SymbolKind::Type)
                        && self.available(*target)
                })
            })
    }

    /// The associated type `Self::name` of the implementation around `position`, whatever
    /// other implementations of the type declare.
    fn associated_type(
        file: &File<'source>,
        position: u64,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let implementation = file
            .facts
            .inherent_impls()
            .iter()
            .filter(|implementation| work.charge() && implementation.range.contains(position))
            .min_by_key(|implementation| implementation.range.end - implementation.range.start)?;
        let header = file.target_at(implementation.type_path)?;
        let qualified = format!("{}::{name}", header.symbol.qualified_name);
        file.by_qualified_name
            .get(qualified.as_str())?
            .iter()
            .find(|symbol| {
                symbol.kinds.contains(&SymbolKind::Type)
                    && implementation.range.encloses(symbol.item_range)
            })
            .map(|symbol| Target {
                file: file.document,
                symbol,
            })
    }

    /// A type item in a block can be used only inside its own module. A nearer type
    /// import or type parameter takes precedence, while a value-only import can coexist.
    fn local_type(
        &self,
        file: &File<'source>,
        position: u64,
        name: &str,
        work: &mut Work,
    ) -> Result<Option<Target<'source>>, ()> {
        let binding = match file.binding(
            name,
            position,
            |kind| kind == StaticBindingKind::RustBlockType,
            work,
        ) {
            Binding::Found(binding) => binding,
            Binding::Missing => return Ok(None),
            Binding::Blocked => return Err(()),
        };
        let module = self.module(file, position, work).ok_or(())?;
        if self.module(file, binding.name.start, work) != Some(module) {
            return Ok(None);
        }
        match file.binding(name, position, is_type_binding, work) {
            Binding::Found(closer)
                if closer.scope != binding.scope && binding.scope.encloses(closer.scope) =>
            {
                let StaticBindingKind::RustUse { path } = closer.kind else {
                    return Err(());
                };
                let imported = self.import_path(file, path, work).ok_or(())?;
                if let Some(target) =
                    self.lookup(file, path.start, &imported, Namespace::Type, work)
                {
                    return Ok(Some(target));
                }
                if self
                    .lookup(file, path.start, &imported, Namespace::Value, work)
                    .is_none()
                    && self
                        .lookup(
                            file,
                            path.start,
                            &imported,
                            Namespace::ConstantOrStatic,
                            work,
                        )
                        .is_none()
                {
                    return Err(());
                }
            }
            Binding::Blocked => return Err(()),
            _ => {}
        }
        file.target_at(binding.name)
            .filter(|target| target.symbol.kinds.contains(&SymbolKind::Type))
            .map(Some)
            .ok_or(())
    }

    fn local_named(
        file: &File<'source>,
        position: u64,
        name: &str,
        work: &mut Work,
        accepts: impl Fn(Target<'source>) -> bool,
    ) -> Option<Target<'source>> {
        let enclosing = file.enclosing_callable(ByteRange {
            start: position,
            end: position,
        })?;
        let mut container = enclosing.symbol.qualified_name.as_str();
        loop {
            if !work.charge() {
                return None;
            }
            let qualified = format!("{container}::{name}");
            if let Some(target) = file
                .qualified_target(&qualified)
                .filter(|target| accepts(*target))
            {
                return Some(target);
            }
            container = container.rsplit_once("::")?.0;
            // Items declared in a body are in scope there; an implementation's or trait's
            // associated items are reachable only through a path such as `Self::Name`.
            file.qualified_target(container)
                .filter(|target| target.symbol.kinds.contains(&SymbolKind::Callable))?;
        }
    }

    pub(super) fn new(
        files: &Arc<BTreeMap<&'source str, File<'source>>>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut namespaces = Self {
            source_files: Arc::clone(files),
            ..Self::default()
        };
        let files = files.as_ref();
        let mut alternatives = unadmitted_alternatives(files, work)?;
        alternatives.extend(unadmitted_chain_modules(files, work)?);
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "rust")
        {
            if alternatives.contains(file.document.path.as_str()) {
                continue;
            }
            namespaces.index_file(file, work)?;
        }
        namespaces.index_exports(files, work)?;
        namespaces.index_aliases(files, work)?;
        namespaces.index_members(files, work)?;
        namespaces.index_methods(files, work)?;
        namespaces.index_field_types(files, work)?;
        namespaces.index_bounds(files, work)?;
        namespaces.index_returns(files, work)?;
        namespaces.index_referents(files, work)?;
        Some(namespaces)
    }

    fn index_exports(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "rust")
        {
            for imported in file.facts.rust_uses() {
                if !work.charge() {
                    return None;
                }
                if file.conditional(imported.range, work) {
                    continue;
                }
                if imported.glob {
                    self.glob_imports
                        .entry(file.document.path.as_str())
                        .or_default()
                        .push(imported);
                    continue;
                }
                let Some(module) = self
                    .module(file, imported.range.start, work)
                    .map(<[String]>::to_vec)
                else {
                    continue;
                };
                let namespace = self.files.get(file.document.path.as_str())?;
                let scope = namespace
                    .inline
                    .iter()
                    .find(|(_, path)| path.as_slice() == module.as_slice())
                    .and_then(|(symbol, _)| symbol.body_range)
                    .unwrap_or(crate::analyzer::namespace::rust_file_scope(
                        file.document.source,
                    )?);
                if imported.scope != scope {
                    continue;
                }
                let Some(binding) = file.facts.bindings().iter().find(|binding| work.charge() && matches!(binding.kind, StaticBindingKind::RustUse { path } if path == imported.range)) else { continue; };
                let mut path = module.clone();
                path.push(file.text(binding.name)?.to_owned());
                let Some(parts) = file.use_parts(imported.range, work) else {
                    if work.exhausted {
                        return None;
                    }
                    // A use path outside the resolver stays unresolved for this site.
                    continue;
                };
                let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
                // A first segment naming no item of the module starts at a dependency
                // crate, as `std`, `core` and `alloc` do (the extern prelude).
                let external = parts.first().is_some_and(|head| {
                    matches!(*head, "std" | "core" | "alloc")
                        || (!matches!(*head, "crate" | "self" | "super") && parts.len() > 1 && {
                            let mut local = module.clone();
                            local.push((*head).to_owned());
                            !self.types.contains_key(&local)
                                && !self.values.contains_key(&local)
                                && matches!(
                                    file.binding(head, imported.range.start, is_type_binding, work),
                                    Binding::Missing
                                )
                        })
                });
                let target = if external {
                    parts.iter().map(|part| (*part).to_owned()).collect()
                } else {
                    let Some(target) = self.absolute_path(file, imported.range.start, &parts, work)
                    else {
                        continue;
                    };
                    target
                };
                let visibility = imported.visibility.and_then(|range| file.text(range));
                insert(
                    &mut self.exports,
                    path,
                    Export {
                        module,
                        target,
                        visibility,
                    },
                );
            }
        }
        Some(())
    }

    fn index_returns(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        for (&path, file) in files
            .iter()
            .filter(|(_, file)| file.document.facts.language().name == "rust")
        {
            for call in file.facts.calls() {
                if !work.charge() {
                    return None;
                }
                if matches!(call.callee, StaticCallee::Member { .. }) {
                    self.member_calls
                        .entry((path, call.callee_range))
                        .and_modify(|call| *call = None)
                        .or_insert(Some(call));
                }
                self.initializers
                    .entry((path, call.range))
                    .and_modify(|call| *call = None)
                    .or_insert(Some(call));
            }
            for &(name, result) in file.facts.declared_types() {
                if file.declared_types.get(&name).copied().flatten() != Some(result) {
                    continue;
                }
                if !work.charge() {
                    return None;
                }
                if file.conditional(result, work) {
                    continue;
                }
                let Some(callable) = file.target_at(name).filter(|target| {
                    target.symbol.kinds.contains(&SymbolKind::Callable)
                        || target.symbol.kind == "field_declaration"
                }) else {
                    continue;
                };
                let owner = self.edge_type_target(file, result, work).or_else(|| {
                    if file.text(result)? != "Self" {
                        return None;
                    }
                    file.qualified_target(callable.symbol.container.as_deref()?)
                        .filter(|target| target.symbol.kind == "trait" && self.available(*target))
                });
                if let Some(owner) = owner {
                    self.return_owners.insert(
                        (path, name),
                        CallableReturn {
                            owner,
                            written: result,
                        },
                    );
                }
            }
        }
        Some(())
    }

    /// Records the nominal type behind each `&T` / `&mut T` implementation owner: method
    /// and field lookup through such a receiver also reaches `T`'s members.
    fn index_referents(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        for (&path, file) in files
            .iter()
            .filter(|(_, file)| file.document.facts.language().name == "rust")
        {
            let deref_traits = deref::trait_headers(file, work)?;
            for implementation in file.facts.inherent_impls() {
                if !work.charge() {
                    return None;
                }
                if !deref_traits.is_empty() {
                    self.index_local_deref(file, implementation, &deref_traits, work);
                }
                let Some(owner) = file
                    .target_at(implementation.type_path)
                    .filter(|owner| structural_owner(*owner) && owner.symbol.name.starts_with('&'))
                else {
                    continue;
                };
                let Some(reference) = file.facts.references().iter().find(|reference| {
                    work.charge()
                        && reference.role == rift_core::ReferenceRole::Type
                        && implementation.type_path.encloses(reference.target)
                }) else {
                    continue;
                };
                // Only `&`, a lifetime and `mut` may precede the referent's path.
                let Some(prefix) = file
                    .text(ByteRange {
                        start: implementation.type_path.start,
                        end: reference.target.start,
                    })
                    .and_then(|prefix| prefix.strip_prefix('&'))
                else {
                    continue;
                };
                let prefix = prefix.trim_start();
                let prefix = prefix
                    .strip_prefix('\'')
                    .map_or(prefix, |lifetime| {
                        lifetime.trim_start_matches(|character: char| {
                            character.is_alphanumeric() || character == '_'
                        })
                    })
                    .trim_start();
                let prefix = prefix.strip_prefix("mut").unwrap_or(prefix);
                if !prefix.trim().is_empty() {
                    continue;
                }
                if let Some(referent) =
                    self.type_target(file, reference.target, work)
                        .filter(|referent| {
                            matches!(referent.symbol.kind, "struct" | "enum" | "union_item")
                        })
                {
                    self.referents
                        .insert((path, owner.symbol.item_range), (owner, referent));
                }
            }
        }
        Some(())
    }

    /// The nominal type a reference implementation owner refers to.
    fn referent(&self, owner: Target<'source>) -> Option<Target<'source>> {
        self.referents
            .get(&(owner.file.path.as_str(), owner.symbol.item_range))
            .map(|(_, referent)| *referent)
    }

    /// The owner whose fields a receiver of `owner` reads: a reference owner's referent.
    fn field_owner(&self, owner: Target<'source>) -> Target<'source> {
        self.referent(owner).unwrap_or(owner)
    }

    fn index_aliases(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        let mut declarations = BTreeMap::new();
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "rust")
        {
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return None;
                }
                let target = Target {
                    file: file.document,
                    symbol,
                };
                // A declaration in a module only an inactive or unknown configuration
                // admits is never an alias endpoint.
                if symbol.kind == "impl_item"
                    || file.conditional(symbol.item_range, work)
                    || self
                        .module_at(file.document.path.as_str(), symbol.item_range.start, work)
                        .is_none()
                {
                    continue;
                }
                let identity = rift_core::symbol_identity(
                    "rust",
                    file.document.placement.identity_path(),
                    &symbol.qualified_name,
                );
                declarations
                    .entry(identity)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(target));
            }
        }
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "rust")
        {
            for alias in file.document.placement.aliases() {
                if !work.charge() {
                    return None;
                }
                let Some(target) = declarations.get(&alias.target_identity).copied().flatten()
                else {
                    continue;
                };
                let path = rift_protocol::identity::SymbolIdentity::parse(alias.identity.as_str())
                    .ok()?
                    .qualified_path()
                    .to_vec();
                if target.symbol.kinds.contains(&SymbolKind::Type) || target.symbol.kind == "module"
                {
                    insert(&mut self.types, path, target);
                } else {
                    insert(&mut self.values, path, target);
                }
            }
        }
        Some(())
    }

    fn index_bounds(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        for (&path, file) in files
            .iter()
            .filter(|(_, file)| file.document.facts.language().name == "rust")
        {
            for &(name, bound) in file.facts.declared_types() {
                if file
                    .target_at(name)
                    .is_some_and(|target| target.symbol.kinds.contains(&SymbolKind::Callable))
                {
                    continue;
                }
                if file.conditional(name, work) {
                    continue;
                }
                if !work.charge() {
                    return None;
                }
                let Binding::Found(parameter) =
                    file.binding(file.text(name)?, name.start, is_type_binding, work)
                else {
                    continue;
                };
                if parameter.kind == StaticBindingKind::RustTypeParameter {
                    self.bounds
                        .entry((path, parameter.name))
                        .or_default()
                        .push(bound);
                }
            }
        }
        Some(())
    }

    fn index_members(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        let mut members = BTreeMap::new();
        for (mut path, target) in std::mem::take(&mut self.members) {
            if !work.charge() {
                return None;
            }
            if let Some(target) = target {
                let file = files.get(target.file.path.as_str())?;
                if let Some(implementation) = inherent_member(file, target.symbol, work)
                    && let Some(owner) = self.type_target(file, implementation.type_path, work)
                {
                    path = owner.admitted_path()?;
                    path.push(target.symbol.name.clone());
                }
            }
            members
                .entry(path)
                .and_modify(|entry| *entry = None)
                .or_insert(target);
        }
        self.members = members;
        Some(())
    }

    fn index_methods(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        let mut methods = BTreeMap::<_, Vec<_>>::new();
        for (path, candidates) in std::mem::take(&mut self.methods) {
            for method in candidates {
                if !work.charge() {
                    return None;
                }
                let file = files.get(method.target.file.path.as_str())?;
                let range = method.implementation.type_path;
                let Some(owner) = self.type_target(file, range, work) else {
                    continue;
                };
                let Some(mut canonical) = owner.admitted_path() else {
                    continue;
                };
                canonical.push(path.last()?.clone());
                methods.entry(canonical).or_default().push(method);
            }
        }
        self.methods = methods;
        Some(())
    }

    fn index_field_types(
        &mut self,
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<()> {
        let mut types = BTreeMap::new();
        for (path, target) in &self.fields {
            if !work.charge() {
                return None;
            }
            let Some(target) = target.filter(|target| {
                target.symbol.kind == "field_declaration" && self.available(*target)
            }) else {
                continue;
            };
            let file = files.get(target.file.path.as_str())?;
            // Function-pointer annotations describe an invocation's return owner,
            // not a nominal value stored in the field itself.
            if target
                .symbol
                .name_range
                .is_some_and(|name| file.declared_types.contains_key(&name))
            {
                continue;
            }
            let reference = file.facts.references().iter().find(|reference| {
                work.charge()
                    && reference.role == rift_core::ReferenceRole::Type
                    && target.symbol.item_range.encloses(reference.target)
            });
            if let Some(reference) = reference
                && let Some(owner) = self.edge_type_target(file, reference.target, work)
            {
                types.insert(path.clone(), owner);
            }
        }
        self.field_types = types;
        Some(())
    }

    fn index_file(&mut self, file: &File<'source>, work: &mut Work) -> Option<()> {
        let mut namespace = FileNamespace {
            base: file.document.placement.module_path().to_vec(),
            inline: Vec::new(),
        };
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let Some(path) = target.admitted_path() else {
                continue;
            };
            if file.inactive(symbol.item_range, work) {
                if symbol.kind == "module" {
                    self.inactive_modules.insert(path);
                }
                continue;
            }
            if file.conditional(symbol.item_range, work) {
                self.unavailable
                    .insert((file.document.path.as_str(), symbol.item_range));
            }
            if symbol.kind == "impl_item" {
                if structural_owner(target) && self.available(target) {
                    let key = (path.first()?.clone(), symbol.name.clone());
                    self.structural_types
                        .entry(key)
                        .and_modify(|previous| {
                            if previous.is_some_and(|previous| {
                                previous.admitted_path().as_deref() != Some(path.as_slice())
                            }) {
                                *previous = None;
                            }
                        })
                        .or_insert(Some(target));
                }
                continue;
            }
            if symbol.kind == "module" {
                if file.conditional(symbol.item_range, work) {
                    self.conditional_modules.insert(path.clone());
                }
                if symbol.body_range.is_some() {
                    namespace.inline.push((symbol, path.clone()));
                }
                insert(&mut self.types, path.clone(), target);
                insert(&mut self.modules, path, target);
                continue;
            }
            self.index_target(file, symbol, path, work);
        }
        if let [crate_name] = namespace.base.as_slice() {
            self.crate_roots
                .entry(crate_name.clone())
                .and_modify(|root| *root = None)
                .or_insert(Some(file.document.path.as_str()));
        }
        self.bases.insert(namespace.base.clone());
        self.files.insert(file.document.path.as_str(), namespace);
        Some(())
    }

    fn index_target(
        &mut self,
        file: &File<'source>,
        symbol: &'source SyntaxSymbol,
        path: Vec<String>,
        work: &mut Work,
    ) {
        let target = Target {
            file: file.document,
            symbol,
        };
        if symbol.container.is_some() {
            let members = if symbol.kind == "field_declaration" {
                &mut self.fields
            } else {
                &mut self.members
            };
            insert(members, path.clone(), target);
        }
        if matches!(symbol.kind, "constant" | "static") && file.is_module_item(symbol) {
            insert(&mut self.constants_and_statics, path, target);
        } else if symbol.kind == "variant" {
            insert(&mut self.values, path, target);
        } else if symbol.kinds.contains(&SymbolKind::Type) && file.is_module_item(symbol) {
            insert(&mut self.types, path, target);
        } else if symbol.kinds.contains(&SymbolKind::Callable) {
            if let Some(implementation) = inherent_member(file, symbol, work) {
                self.methods.entry(path).or_default().push(Method {
                    target,
                    implementation,
                });
            } else if file.is_module_item(symbol) {
                insert(&mut self.values, path, target);
            }
        }
    }

    pub(super) fn call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        self.call_at_depth(file, call, work, RECEIVER_DEPTH_MAX)
    }

    fn call_at_depth(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
        depth: usize,
    ) -> Option<Target<'source>> {
        if depth == 0 {
            return None;
        }
        if file.conditional(call.range, work) {
            return None;
        }
        self.module(file, call.range.start, work)?;
        match call.callee {
            StaticCallee::Direct => self.direct_call(file, call, work),
            StaticCallee::Qualified => self.qualified_call(file, call, work),
            StaticCallee::Member { receiver, member } => {
                self.receiver_method_at_depth(file, receiver, file.text(member)?, work, depth)
            }
            StaticCallee::Unsupported | StaticCallee::Constructor => None,
        }
    }

    pub(super) fn import(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let path = self.import_path(file, range, work)?;
        self.lookup(file, range.start, &path, Namespace::Value, work)
            .or_else(|| self.lookup(file, range.start, &path, Namespace::Type, work))
    }

    fn import_path(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Vec<String>> {
        if file.conditional(range, work) || file.blocked(range.start, work) {
            return None;
        }
        let parts = file.use_parts(range, work)?;
        let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
        self.absolute_path(file, range.start, &parts, work)
    }

    fn import_source(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let path = self.module(file, range.start, work)?;
        let namespace = self.files.get(file.document.path.as_str())?;
        let scope = namespace
            .inline
            .iter()
            .find(|(_, inline)| inline.as_slice() == path)
            .and_then(|(symbol, _)| symbol.body_range)
            .unwrap_or(crate::analyzer::namespace::rust_file_scope(
                file.document.source,
            )?);
        let imported = file
            .facts
            .rust_uses()
            .iter()
            .find(|imported| work.charge() && imported.range == range)?;
        if imported.scope != scope {
            return None;
        }
        self.types
            .get(path)
            .copied()
            .flatten()
            .filter(|target| target.symbol.kind == "module" && self.available(*target))
    }

    fn written_import(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if file.conditional(range, work) || file.blocked(range.start, work) {
            return None;
        }
        let parts = file.use_parts(range, work)?;
        let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
        let path = self.absolute_path(file, range.start, &parts, work)?;
        let (_, parent) = path.split_last()?;
        let module = self
            .lookup(file, range.start, parent, Namespace::Type, work)
            .filter(|target| target.symbol.kind == "module")?;
        if self.import(file, range, work).is_some() {
            return Some(module);
        }
        let source = self.module(file, range.start, work)?;
        let mut current = path;
        // A bounded local re-export chain can end at an authored external import.
        // Only the actual written local module is emitted as an endpoint.
        for _ in 0..RECEIVER_DEPTH_MAX {
            if !work.charge() {
                return None;
            }
            let export = self.exports.get(&current)?.as_ref()?;
            if !namespace_visible(source, &export.module, export.visibility) {
                return None;
            }
            if export.target.first() != source.first() {
                return Some(module);
            }
            if self
                .lookup(file, range.start, &export.target, Namespace::Type, work)
                .is_some()
                || self
                    .lookup(file, range.start, &export.target, Namespace::Value, work)
                    .is_some()
            {
                return Some(module);
            }
            current = export.target.clone();
        }
        None
    }

    /// Absolute path of one use declaration when no selected source declares its root.
    pub(super) fn spelled_use(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Vec<String>> {
        if file.conditional(range, work) {
            return None;
        }
        let parts = file.use_parts(range, work)?;
        self.undeclared_root(file, range.start, parts, work)
    }

    /// Absolute path an authored path spells through the file's use declarations when no
    /// selected source declares its root.
    pub(super) fn spelled_path(
        &self,
        file: &File<'source>,
        position: u64,
        text: &str,
        value: bool,
        work: &mut Work,
    ) -> Option<Vec<String>> {
        let range = ByteRange {
            start: position,
            end: position,
        };
        if file.conditional(range, work) {
            return None;
        }
        let text = text.strip_prefix("::").unwrap_or(text);
        let parts = path_segments(text, work)?;
        let (head, tail) = parts.split_first()?;
        let accepts = if value && tail.is_empty() {
            is_value_binding
        } else {
            is_type_binding
        };
        let spelled = match file.binding(head, position, accepts, work) {
            Binding::Found(binding) => {
                let StaticBindingKind::RustUse { path } = binding.kind else {
                    return None;
                };
                // A `use` of an enclosing module does not name anything inside an inline
                // child module; only a binding of the same module spells the path.
                if self.module(file, binding.name.start, work)?
                    != self.module(file, position, work)?
                {
                    return None;
                }
                let mut imported = file.use_parts(path, work)?;
                imported.extend(tail.iter().map(|part| (*part).to_owned()));
                imported
            }
            Binding::Missing => {
                for imported in file.facts.rust_uses() {
                    if !work.charge() || imported.glob && imported.scope.contains(position) {
                        return None;
                    }
                }
                parts.iter().map(|part| (*part).to_owned()).collect()
            }
            Binding::Blocked => return None,
        };
        self.undeclared_root(file, position, spelled, work)
    }

    fn undeclared_root(
        &self,
        file: &File<'source>,
        position: u64,
        path: Vec<String>,
        work: &mut Work,
    ) -> Option<Vec<String>> {
        let head = path.first()?;
        if matches!(head.as_str(), "crate" | "self" | "super" | "Self") {
            return None;
        }
        let module = self.module(file, position, work)?;
        for base in [module, module.get(..1)?] {
            let mut prefix = base.to_vec();
            prefix.push(head.clone());
            let from = || prefix.clone()..;
            let starts = |path: &Vec<String>| path.starts_with(&prefix);
            if self.modules.contains_key(&prefix)
                || self.bases.range(from()).next().is_some_and(starts)
                || self
                    .types
                    .range(from())
                    .next()
                    .is_some_and(|row| starts(row.0))
                || self
                    .values
                    .range(from())
                    .next()
                    .is_some_and(|row| starts(row.0))
            {
                return None;
            }
        }
        Some(path)
    }

    fn direct_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = file.text(call.callee_range)?;
        let position = call.range.start;
        let module = self.module(file, position, work)?;
        let binding = match file.binding(name, position, is_value_binding, work) {
            Binding::Found(binding) if self.module(file, binding.name.start, work)? == module => {
                Some(binding)
            }
            Binding::Found(_) | Binding::Missing => None,
            Binding::Blocked => return None,
        };
        if let Some(binding) = binding {
            if let Some(imported) = self.glob_target(
                file,
                position,
                name,
                Namespace::Value,
                Some(binding.scope),
                work,
            ) {
                return imported.target().filter(callable_value);
            }
            if work.exhausted {
                return None;
            }
            return match binding.kind {
                StaticBindingKind::Local => Self::local_callable(file, binding.name, work),
                StaticBindingKind::RustUse { path } => {
                    self.import(file, path, work).filter(callable_value)
                }
                _ => None,
            };
        }
        let mut path = module.to_vec();
        path.push(name.to_owned());
        if let Some(target) = self.lookup(file, position, &path, Namespace::Value, work) {
            return Some(target);
        }
        if let Some(imported) = self.glob_target(file, position, name, Namespace::Value, None, work)
        {
            return imported.target().filter(callable_value);
        }
        // A tuple constructor can also be named by an admitted type path.
        if let Some(target) = self.local_type(file, position, name, work).ok()? {
            return (target.symbol.kind == "struct" && self.available(target)).then_some(target);
        }
        self.lookup(file, position, &path, Namespace::Type, work)
            .filter(|target| target.symbol.kind == "struct")
    }

    fn local_callable(
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let target = file.target_at(range).filter(callable_value)?;
        if file.conditional(target.symbol.item_range, work)
            || file.in_impl_or_trait(target.symbol, work)
        {
            return None;
        }
        Some(target)
    }

    fn qualified_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let parts = path_segments(file.text(call.callee_range)?, work)?;
        let position = call.range.start;
        // A struct declared in an enclosing body shadows module items of its name.
        if let [name] = parts.as_slice()
            && let Some(target) = self.local_type(file, position, name, work).ok()?
        {
            return (target.symbol.kind == "struct" && self.available(target)).then_some(target);
        }
        if let ["Self", member] = parts.as_slice()
            && let Some(owner) = self.structural_self(file, position, work)
        {
            // An inherent method of the standard library comes before one of a trait.
            if self.standard_associated(file, owner, member, work)? {
                return None;
            }
            return self.inherent_method(file, position, owner, member, work);
        }
        if let [parameter, member] = parts.as_slice()
            && let Binding::Found(binding) =
                file.binding(parameter, position, is_type_binding, work)
            && binding.kind == StaticBindingKind::RustTypeParameter
        {
            return self.bound_member(file, position, binding, member, work);
        }
        let path = self.resolved_path(file, position, &parts, work)?;
        if let Some(target) = self.lookup(file, position, &path, Namespace::Value, work) {
            return Some(target);
        }
        if let Some(target) = self
            .lookup(file, position, &path, Namespace::Type, work)
            .filter(|target| target.symbol.kind == "struct")
        {
            return Some(target);
        }
        let (member, receiver) = path.split_last()?;
        if let Some(owner) = self.lookup(file, position, receiver, Namespace::Type, work) {
            return self.inherent_method(file, position, owner, member, work);
        }
        self.trait_path_call(file, call, &parts, work)
    }

    /// `Trait::method(value, ..)` where `Trait` is not a package-local declaration: the
    /// first argument's known owner selects the one implementation of a trait spelled
    /// `Trait` that declares `method` with a `self` receiver. Any other candidate count,
    /// or an unknown argument, leaves the call unresolved.
    fn trait_path_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        parts: &[&str],
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let [.., written_trait, member] = parts else {
            return None;
        };
        let (_, argument) = file
            .facts
            .receiver_arguments()
            .iter()
            .find(|(range, _)| work.charge() && *range == call.range)?;
        let argument = reference_operand(file, *argument)?;
        let references::ReceiverOwner::Target(owner) =
            self.receiver_owner_at_depth(file, argument, work, RECEIVER_DEPTH_MAX)?
        else {
            return None;
        };
        let mut owners = vec![owner];
        owners.extend(self.referent(owner));
        owners.extend(
            self.referents
                .values()
                .filter(|(_, referent)| {
                    referent.file.path == owner.file.path
                        && referent.symbol.item_range == owner.symbol.item_range
                })
                .map(|(reference, _)| *reference),
        );
        let mut selected = None;
        for candidate in owners {
            let Some(mut path) = candidate.admitted_path() else {
                continue;
            };
            path.push((*member).to_owned());
            for method in self.methods.get(&path).into_iter().flatten() {
                if !work.charge() {
                    return None;
                }
                let facts = method.target.file.facts.static_facts()?;
                let implemented = facts
                    .heritage_edges()
                    .iter()
                    .filter(|edge| edge.from == method.implementation.type_path)
                    .filter_map(|edge| {
                        method.target.file.source.get(
                            usize::try_from(edge.target.start).ok()?
                                ..usize::try_from(edge.target.end).ok()?,
                        )
                    })
                    .any(|name| trait_name(name) == Some(*written_trait));
                let receiver = matches!(
                    self_kind(method.target),
                    Some(SelfKind::Value | SelfKind::Shared | SelfKind::Unique)
                );
                if !implemented || !receiver {
                    continue;
                }
                if !self.available(method.target) || selected.replace(method.target).is_some() {
                    return None;
                }
            }
        }
        selected
    }

    fn inherent_method(
        &self,
        file: &File<'source>,
        position: u64,
        owner: Target<'source>,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if owner.symbol.kind == "trait" {
            let mut path = owner.admitted_path()?;
            path.push(member.to_owned());
            return self
                .members
                .get(&path)
                .copied()
                .flatten()
                .filter(|target| self.available(*target))?
                .callable();
        }
        if !matches!(owner.symbol.kind, "struct" | "enum" | "union_item")
            && !structural_owner(owner)
        {
            return None;
        }
        let mut path = owner.admitted_path()?;
        path.push(member.to_owned());
        let candidates = self.methods.get(&path)?;
        let mut inherent = Vec::new();
        let mut implemented = Vec::new();
        for method in candidates {
            if !work.charge() {
                return None;
            }
            if !self.available(method.target) {
                return None;
            }
            let range = method.implementation.type_path;
            let trait_impl = method
                .target
                .file
                .facts
                .static_facts()?
                .heritage_edges()
                .iter()
                .any(|edge| edge.from == range);
            if trait_impl {
                implemented.push(*method);
            } else {
                inherent.push(*method);
            }
        }
        // Inherent methods take precedence.
        let target = match (inherent.as_slice(), implemented.as_slice()) {
            ([method], _) | ([], [method]) => method.target,
            ([], methods) => receivers::enclosing_method(file, position, methods)?.target,
            _ => return None,
        };
        (target.symbol.visibility.as_deref() == Some("pub")
            || self.declaration_visible(file, position, target, work)
            || target.symbol.visibility.is_none())
        .then_some(target)
    }

    fn declaration_visible(
        &self,
        file: &File<'source>,
        position: u64,
        target: Target<'source>,
        work: &mut Work,
    ) -> bool {
        let Some(source) = self.module(file, position, work) else {
            return false;
        };
        let Some(declared) = self.module_at(
            target.file.path.as_str(),
            target.symbol.item_range.start,
            work,
        ) else {
            return false;
        };
        namespace_visible(source, declared, target.symbol.visibility.as_deref())
    }

    fn lookup(
        &self,
        file: &File<'source>,
        position: u64,
        path: &[String],
        namespace: Namespace,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let index = match namespace {
            Namespace::Type => &self.types,
            Namespace::Value => &self.values,
            Namespace::ConstantOrStatic => &self.constants_and_statics,
        };
        // A `use` binding of a module names its target there. Without a declaration or an
        // alias at the path, a visible binding (a private import of an ancestor module, too)
        // is followed to its target; a cycle of bindings resolves nothing.
        let mut path = std::borrow::Cow::Borrowed(path);
        let mut followed = BTreeSet::new();
        let mut binding_module = None;
        while let Some(export) = self.exports.get(path.as_ref()) {
            let export = export.as_ref()?;
            if !namespace_visible(
                self.module(file, position, work)?,
                &export.module,
                export.visibility,
            ) {
                return None;
            }
            if index.contains_key(path.as_ref()) {
                break;
            }
            if !work.charge() || !followed.insert(path.to_vec()) {
                return None;
            }
            binding_module = Some(&export.module);
            path = std::borrow::Cow::Owned(export.target.clone());
        }
        let target = index
            .get(path.as_ref())
            .copied()
            .flatten()
            .filter(|target| self.available(*target))?;
        // Through a binding, the item needs to be visible where the last binding names it;
        // the modules on its own path need not be visible from the source, but each must be
        // admitted.
        match binding_module {
            Some(module) => {
                let (_, declared) = path.split_last()?;
                ((1..=declared.len()).all(|length| !self.module_unavailable(&declared[..length]))
                    && namespace_visible(module, declared, target.symbol.visibility.as_deref()))
                .then_some(target)
            }
            None => self
                .accessible(file, position, &path, target, work)
                .then_some(target),
        }
    }

    fn available(&self, target: Target<'source>) -> bool {
        !self
            .unavailable
            .contains(&(target.file.path.as_str(), target.symbol.item_range))
    }

    fn accessible(
        &self,
        file: &File<'source>,
        position: u64,
        path: &[String],
        target: Target<'source>,
        work: &mut Work,
    ) -> bool {
        let Some((_, target_namespace)) = path.split_last() else {
            return false;
        };
        let Some(source) = self.module(file, position, work) else {
            return false;
        };
        for length in 1..=target_namespace.len() {
            let module = &target_namespace[..length];
            if !work.charge() || self.module_unavailable(module) {
                return false;
            }
            if let Some(target) = self.modules.get(module) {
                let Some(target) = target else {
                    return false;
                };
                if !namespace_visible(
                    source,
                    &module[..length - 1],
                    target.symbol.visibility.as_deref(),
                ) {
                    return false;
                }
            }
        }
        let visibility = target.symbol.visibility.as_deref();
        let within_crate = source.first() == target_namespace.first();
        let restricted = match visibility {
            Some("pub(crate)") => within_crate,
            Some("pub(super)") => target_namespace
                .split_last()
                .is_some_and(|(_, parent)| source.starts_with(parent)),
            Some("pub(self)" | "private") => source.starts_with(target_namespace),
            _ => false,
        };
        visibility == Some("pub")
            || restricted
            || source == target_namespace
            || (target.symbol.kinds.contains(&SymbolKind::Callable)
                && self.methods.contains_key(path)
                && target_namespace
                    .split_last()
                    .is_some_and(|(_, module)| source == module))
    }

    fn resolved_path(
        &self,
        file: &File<'source>,
        position: u64,
        parts: &[&str],
        work: &mut Work,
    ) -> Option<Vec<String>> {
        let (head, tail) = parts.split_first()?;
        if *head == "Self" {
            if let Some(owner) = self.structural_self(file, position, work) {
                let mut path = owner.admitted_path()?;
                path.extend(tail.iter().map(|part| (*part).to_owned()));
                return Some(path);
            }
            let implementation = file
                .facts
                .inherent_impls()
                .iter()
                .filter(|implementation| implementation.range.contains(position))
                .min_by_key(|implementation| implementation.range.end - implementation.range.start);
            let Some(implementation) = implementation else {
                let member = file.enclosing_callable(ByteRange {
                    start: position,
                    end: position,
                })?;
                let owner = file.qualified_target(member.symbol.container.as_deref()?)?;
                if owner.symbol.kind != "trait" {
                    return None;
                }
                let mut path = owner.admitted_path()?;
                path.extend(tail.iter().map(|part| (*part).to_owned()));
                return Some(path);
            };
            let parts = path_segments(file.text(implementation.type_path)?, work)?;
            if parts.first() == Some(&"Self") {
                return None;
            }
            let mut path =
                self.resolved_path(file, implementation.type_path.start, &parts, work)?;
            path.extend(tail.iter().map(|part| (*part).to_owned()));
            return Some(path);
        }
        if matches!(*head, "crate" | "self" | "super") {
            return self.absolute_path(file, position, parts, work);
        }
        let binding = file.binding(head, position, is_type_binding, work);
        let module = self.module(file, position, work)?;
        let mut local = module.to_vec();
        local.push((*head).to_owned());
        if let Some(target) = self.types.get(&local).copied().flatten()
            && target.admitted_path().as_deref() == Some(local.as_slice())
            && let Binding::Found(imported) = binding
        {
            let namespace = self.files.get(file.document.path.as_str())?;
            let scope = namespace
                .inline
                .iter()
                .find(|(_, path)| path.as_slice() == module)
                .and_then(|(symbol, _)| symbol.body_range)
                .unwrap_or(crate::analyzer::namespace::rust_file_scope(
                    file.document.source,
                )?);
            if imported.scope == scope {
                return None;
            }
            if imported.scope.encloses(scope) {
                local.extend(tail.iter().map(|part| (*part).to_owned()));
                return Some(local);
            }
        }
        match binding {
            Binding::Found(binding) => match binding.kind {
                StaticBindingKind::RustUse { path } => {
                    if file.conditional(path, work) {
                        return None;
                    }
                    let imported = file.use_parts(path, work)?;
                    let imported = imported.iter().map(String::as_str).collect::<Vec<_>>();
                    let mut prefix = self.absolute_path(file, path.start, &imported, work)?;
                    prefix.extend(tail.iter().map(|part| (*part).to_owned()));
                    Some(prefix)
                }
                _ => None,
            },
            Binding::Missing => {
                let mut path = self.module(file, position, work)?.to_vec();
                path.extend(parts.iter().map(|part| (*part).to_owned()));
                if !self.types.contains_key(&path)
                    && !self.values.contains_key(&path)
                    && let Some(target) = self
                        .glob_target(file, position, head, Namespace::Type, None, work)
                        .and_then(GlobTarget::target)
                {
                    let mut path = target.admitted_path()?;
                    path.extend(tail.iter().map(|part| (*part).to_owned()));
                    return Some(path);
                }
                Some(path)
            }
            Binding::Blocked => None,
        }
    }

    fn structural_self(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let implementation = file
            .facts
            .inherent_impls()
            .iter()
            .filter(|implementation| work.charge() && implementation.range.contains(position))
            .min_by_key(|implementation| implementation.range.end - implementation.range.start)?;
        file.target_at(implementation.type_path)
            .filter(|target| structural_owner(*target) && self.available(*target))
    }

    fn absolute_path(
        &self,
        file: &File<'source>,
        position: u64,
        parts: &[&str],
        work: &mut Work,
    ) -> Option<Vec<String>> {
        let module = self.module(file, position, work)?;
        let (head, tail) = parts.split_first()?;
        let (base, tail) = match *head {
            "crate" => (module.get(..1)?, tail),
            "self" => (module, tail),
            "super" => {
                let supers = parts.iter().take_while(|part| **part == "super").count();
                let depth = module.len().checked_sub(supers)?;
                if depth == 0 {
                    return None;
                }
                (module.get(..depth)?, parts.get(supers..)?)
            }
            _ => (module, parts),
        };
        let mut path = base.to_vec();
        path.extend(tail.iter().map(|part| (*part).to_owned()));
        Some(path)
    }

    /// A glob in the nearest scope that imports this name. An equal-scope conflict
    /// has an applicable binding but no one target.
    fn glob_target(
        &self,
        file: &File<'source>,
        position: u64,
        name: &str,
        namespace: Namespace,
        outer: Option<ByteRange>,
        work: &mut Work,
    ) -> Option<GlobTarget<'source>> {
        let imports = self.glob_imports.get(file.document.path.as_str())?;
        let module = self.module(file, position, work)?;
        let mut selected: Option<(ByteRange, GlobTarget<'source>)> = None;
        for imported in imports {
            if !work.charge() {
                return None;
            }
            let scope = imported.scope;
            if !scope.contains(position)
                || outer.is_some_and(|outer| scope == outer || !outer.encloses(scope))
                || selected
                    .is_some_and(|(previous, _)| previous != scope && scope.encloses(previous))
                || self.module(file, imported.range.start, work)? != module
            {
                continue;
            }
            let parts = file.use_parts(imported.range, work)?;
            let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
            let Some(mut path) = self.absolute_path(file, imported.range.start, &parts, work)
            else {
                continue;
            };
            path.push(name.to_owned());
            let Some(target) = self.lookup(file, position, &path, namespace, work) else {
                continue;
            };
            let target = match selected {
                Some((previous, selected))
                    if previous == scope
                        && selected
                            .target()
                            .is_none_or(|previous| previous.symbol != target.symbol) =>
                {
                    GlobTarget::Ambiguous
                }
                _ => GlobTarget::Resolved(target),
            };
            selected = Some((scope, target));
        }
        selected.map(|(_, target)| target)
    }

    fn module(&self, file: &File<'source>, position: u64, work: &mut Work) -> Option<&[String]> {
        self.module_at(file.document.path.as_str(), position, work)
    }

    fn module_at(&self, file: &str, position: u64, work: &mut Work) -> Option<&[String]> {
        self.module_at_with_scope(file, position, work)
            .map(|(path, _)| path)
    }

    fn module_scope(
        &self,
        file: &File<'source>,
        position: u64,
        work: &mut Work,
    ) -> Option<ByteRange> {
        let (_, inline) = self.module_at_with_scope(file.document.path.as_str(), position, work)?;
        inline.or_else(|| crate::analyzer::namespace::rust_file_scope(file.document.source))
    }

    fn module_at_with_scope(
        &self,
        file: &str,
        position: u64,
        work: &mut Work,
    ) -> Option<(&[String], Option<ByteRange>)> {
        let namespace = self.files.get(file)?;
        let mut path = namespace.base.as_slice();
        let mut scope = None;
        let crate_name = path.first()?;
        if self
            .crate_roots
            .get(crate_name)
            .is_some_and(Option::is_none)
        {
            return None;
        }
        for (symbol, inline) in &namespace.inline {
            if !work.charge() {
                return None;
            }
            if inline.len() > path.len()
                && let Some(body) = symbol.body_range.filter(|body| body.contains(position))
            {
                path = inline;
                scope = Some(body);
            }
        }
        for length in 1..=path.len() {
            if !work.charge() || self.module_unavailable(&path[..length]) {
                return None;
            }
        }
        Some((path, scope))
    }

    fn module_unavailable(&self, path: &[String]) -> bool {
        self.conditional_modules.contains(path)
            || self.inactive_modules.contains(path) && !self.types.contains_key(path)
    }
}

impl File<'_> {
    fn use_parts(&self, range: ByteRange, work: &mut Work) -> Option<Vec<String>> {
        let mut parts = Vec::new();
        for imported in self.facts.rust_uses() {
            if !work.charge() {
                return None;
            }
            if imported.range != range {
                continue;
            }
            for &range in &imported.path {
                if !work.charge() {
                    return None;
                }
                parts.extend(
                    path_segments(self.text(range)?, work)?
                        .into_iter()
                        .map(str::to_owned),
                );
            }
            if parts.last().is_some_and(|part| part == "self") {
                parts.pop();
            }
            return Some(parts);
        }
        Some(
            path_segments(self.text(range)?, work)?
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    pub(super) fn conditional(&self, range: ByteRange, work: &mut Work) -> bool {
        self.conditions
            .any(range.start, work, |admitted| admitted != Some(true))
    }

    pub(super) fn inactive(&self, range: ByteRange, work: &mut Work) -> bool {
        self.conditions
            .any(range.start, work, |admitted| admitted == Some(false))
    }

    fn in_impl_or_trait(&self, symbol: &SyntaxSymbol, work: &mut Work) -> bool {
        let Some(container) = symbol.container.as_deref() else {
            return false;
        };
        let Some(target) = self
            .parent_callable(symbol)
            .or_else(|| self.qualified_target(container))
        else {
            // An impl through an alias can lack a physical declaration in this unit.
            return true;
        };
        !work.charge()
            || target.symbol.kind != "module"
                && !(target.symbol.kind == "function"
                    && target
                        .symbol
                        .body_range
                        .is_some_and(|body| body.encloses(symbol.item_range)))
    }

    fn is_module_item(&self, symbol: &SyntaxSymbol) -> bool {
        symbol.container.as_deref().is_none_or(|container| {
            self.qualified_target(container)
                .is_some_and(|target| target.symbol.kind == "module")
        })
    }
}

fn inherent_member<'source>(
    file: &File<'source>,
    symbol: &SyntaxSymbol,
    work: &mut Work,
) -> Option<&'source rift_syntax::StaticInherentImpl> {
    let container = symbol.container.as_deref()?;
    file.facts.inherent_impls().iter().find(|implementation| {
        work.charge()
            && implementation.range.encloses(symbol.item_range)
            && file
                .target_at(implementation.type_path)
                .is_some_and(|target| {
                    target.symbol.kind == "impl_item" && target.symbol.qualified_name == container
                })
    })
}

/// Files of a module that several `#[path]` alternatives declare, other than the one the
/// admitted configuration selects.
///
/// Each alternative file keeps its declarations, but only the file an active `mod` item
/// names contributes resolved facts and endpoints. A module whose selection is unknown
/// (no active item, an unknown condition, or a path relative to an inline module) admits
/// none of its alternatives.
fn unadmitted_alternatives<'source>(
    files: &BTreeMap<&'source str, File<'source>>,
    work: &mut Work,
) -> Option<BTreeSet<&'source str>> {
    let mut modules = BTreeMap::<Vec<String>, Vec<&'source str>>::new();
    let mut declared = Vec::new();
    for (&path, file) in files
        .iter()
        .filter(|(_, file)| file.document.facts.language().name == "rust")
    {
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let Some(qualified) = target.admitted_path() else {
                continue;
            };
            if symbol.kind == "module" && symbol.body_range.is_none() {
                declared.push((file, symbol, qualified));
            }
        }
        let base = file.document.placement.module_path();
        if !base.is_empty() {
            modules.entry(base.to_vec()).or_default().push(path);
        }
    }
    let mut unadmitted = BTreeSet::new();
    for (module, paths) in modules.into_iter().filter(|(_, paths)| paths.len() > 1) {
        let mut selected = BTreeSet::new();
        let mut unknown = false;
        for (file, symbol, _) in declared.iter().filter(|(_, _, path)| *path == module) {
            if !work.charge() {
                return None;
            }
            if file.inactive(symbol.item_range, work) {
                continue;
            }
            match &symbol.module_path {
                Some(rift_syntax::RustModulePath::Literal { path, .. })
                    if symbol.container.is_none() && !file.conditional(symbol.item_range, work) =>
                {
                    let directory = file
                        .document
                        .path
                        .as_str()
                        .rsplit_once('/')
                        .map_or("", |(directory, _)| directory);
                    selected.insert(if directory.is_empty() {
                        path.as_str().to_owned()
                    } else {
                        format!("{directory}/{}", path.as_str())
                    });
                }
                _ => unknown = true,
            }
        }
        let admitted = paths
            .iter()
            .copied()
            .filter(|path| !unknown && selected.contains(*path))
            .collect::<Vec<_>>();
        if let [admitted] = admitted.as_slice() {
            unadmitted.extend(paths.iter().copied().filter(|path| path != admitted));
        } else {
            unadmitted.extend(paths);
        }
    }
    Some(unadmitted)
}

/// Files of modules a conditional item chain declares whose branch the configuration does
/// not admit or leaves unknown. They keep their declarations and contribute no resolved
/// facts or endpoints, as an unadmitted `#[path]` alternative does. A module an ordinary
/// `mod` item also declares keeps that item's admission.
fn unadmitted_chain_modules<'source>(
    files: &BTreeMap<&'source str, File<'source>>,
    work: &mut Work,
) -> Option<BTreeSet<&'source str>> {
    let mut unadmitted = BTreeSet::new();
    if files.values().all(|file| file.chain_modules.is_empty()) {
        return Some(unadmitted);
    }
    let mut bases = BTreeMap::<&'source str, &'source [String]>::new();
    let mut declared = BTreeSet::<Vec<String>>::new();
    for (&path, file) in files
        .iter()
        .filter(|(_, file)| file.document.facts.language().name == "rust")
    {
        let base = file.document.placement.module_path();
        if !base.is_empty() {
            bases.insert(path, base);
        }
        for symbol in file.document.facts.symbols() {
            if !work.charge() {
                return None;
            }
            let target = Target {
                file: file.document,
                symbol,
            };
            let Some(qualified) = target.admitted_path() else {
                continue;
            };
            if symbol.kind == "module" && symbol.body_range.is_none() {
                declared.insert(qualified);
            }
        }
    }
    for (&parent, file) in files {
        for (&name, &admitted) in &file.chain_modules {
            if admitted == Some(true) {
                continue;
            }
            let directories = [
                parent.strip_suffix("/mod.rs"),
                parent.strip_suffix(".rs"),
                Some(
                    parent
                        .rsplit_once('/')
                        .map_or("", |(directory, _)| directory),
                ),
            ];
            for directory in directories.into_iter().flatten() {
                let stem = if directory.is_empty() {
                    name.to_owned()
                } else {
                    format!("{directory}/{name}")
                };
                for candidate in [format!("{stem}.rs"), format!("{stem}/mod.rs")] {
                    if !work.charge() {
                        return None;
                    }
                    let Some((&path, base)) = bases.get_key_value(candidate.as_str()) else {
                        continue;
                    };
                    let Some((last, module)) = base.split_last() else {
                        continue;
                    };
                    if last != name
                        || declared.contains(*base)
                        || bases.get(parent).is_some_and(|parent| *parent != module)
                    {
                        continue;
                    }
                    unadmitted.insert(path);
                }
            }
        }
    }
    Some(unadmitted)
}

/// The final segment of a written trait path, without generic arguments.
fn trait_name(written: &str) -> Option<&str> {
    let path = written.split('<').next()?.trim();
    Some(path.rsplit("::").next()?.trim())
}

/// The operand of `&value`, `&mut value` or a parenthesized value, else the range itself.
fn reference_operand(file: &File<'_>, mut range: ByteRange) -> Option<ByteRange> {
    // Each step removes at least one byte of the bounded argument text.
    loop {
        let text = file.text(range)?;
        let (start, end) = if let Some(rest) = text.strip_prefix('&') {
            let rest = rest.trim_start();
            let rest = rest
                .strip_prefix("mut")
                .filter(|after| after.starts_with(char::is_whitespace))
                .unwrap_or(rest)
                .trim_start();
            (text.len() - rest.len(), text.len())
        } else if let Some(inner) = text
            .strip_prefix('(')
            .and_then(|text| text.strip_suffix(')'))
        {
            let leading = inner.len() - inner.trim_start().len();
            let trailing = inner.len() - inner.trim_end().len();
            (1 + leading, text.len() - 1 - trailing)
        } else {
            return Some(range);
        };
        if start >= end {
            return None;
        }
        range = ByteRange {
            start: range.start.checked_add(u64::try_from(start).ok()?)?,
            end: range.start.checked_add(u64::try_from(end).ok()?)?,
        };
    }
}

/// Whether a method belongs to a trait implementation rather than an inherent one.
fn trait_method(method: &Method<'_>) -> Option<bool> {
    Some(
        method
            .target
            .file
            .facts
            .static_facts()?
            .heritage_edges()
            .iter()
            .any(|edge| edge.from == method.implementation.type_path),
    )
}

/// How a method takes its receiver.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelfKind {
    /// `self` or `mut self`.
    Value,
    /// `&self`, with or without a lifetime.
    Shared,
    /// `&mut self`, with or without a lifetime.
    Unique,
    /// A typed receiver such as `self: Box<Self>`, or an unrecognized form.
    Other,
    /// No `self` parameter: not callable with method syntax.
    Absent,
}

/// The receiver form written on a method's `self` parameter.
fn self_kind(method: Target<'_>) -> Option<SelfKind> {
    let source = method.file.source;
    let facts = method.file.facts.static_facts()?;
    let Some(binding) = facts.bindings().iter().find(|binding| {
        matches!(binding.kind, StaticBindingKind::RustReceiver { .. })
            && method.symbol.item_range.encloses(binding.scope)
            && references::source_text(source, binding.name) == Some("self")
    }) else {
        return Some(SelfKind::Absent);
    };
    receiver_form(source, binding)
}

/// The form of one `self` parameter.
fn receiver_form(source: &str, binding: &rift_syntax::StaticBinding) -> Option<SelfKind> {
    let start = usize::try_from(binding.scope.start).ok()?;
    let name = usize::try_from(binding.name.start).ok()?;
    let end = usize::try_from(binding.name.end).ok()?;
    let before = source.get(start..name)?;
    let parameter = before.rsplit_once('(')?.1.trim();
    let after = source.get(end..)?.trim_start();
    if after.starts_with(':') {
        return Some(SelfKind::Other);
    }
    let (references, rest) = receivers::split_references(parameter);
    Some(match (references.as_slice(), rest) {
        ([], "" | "mut") => SelfKind::Value,
        ([receivers::Reference::Shared], "") => SelfKind::Shared,
        ([receivers::Reference::Unique], "") => SelfKind::Unique,
        _ => SelfKind::Other,
    })
}

fn callable_value(target: &Target<'_>) -> bool {
    target.symbol.kinds.contains(&SymbolKind::Callable)
        || matches!(target.symbol.kind, "struct" | "variant")
}

fn structural_owner(target: Target<'_>) -> bool {
    target.symbol.kind == "impl_item" && target.symbol.name.starts_with(['&', '[', '('])
}

fn insert<T>(index: &mut BTreeMap<Vec<String>, Option<T>>, path: Vec<String>, target: T) {
    index
        .entry(path)
        .and_modify(|entry| *entry = None)
        .or_insert(Some(target));
}

/// The segments of a written path. Generic arguments (`Holder::<A>::new`, `Holder<A>`) and
/// a qualified self type without a trait (`<Holder<A>>::new`) name the same declarations
/// as the plain path; a path through `<T as Trait>` or any other shape gives none.
fn path_segments<'text>(source: &'text str, work: &mut Work) -> Option<Vec<&'text str>> {
    let mut rest = source.trim();
    let mut path = Vec::new();
    if rest.starts_with('<') {
        let end = generic_end(rest, work)?;
        let inner = &rest[1..end];
        if inner.split_whitespace().any(|word| word == "as") {
            return None;
        }
        path.extend(path_segments(inner, work)?);
        rest = rest[end + 1..].trim_start();
        if rest.is_empty() {
            return Some(path);
        }
        rest = rest.strip_prefix("::")?;
    }
    let bytes = rest.as_bytes();
    let mut depth = 0_usize;
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        if !work.charge() {
            return None;
        }
        match bytes[index] {
            b'<' => depth = depth.checked_add(1)?,
            b'>' if index == 0 || bytes[index - 1] != b'-' => depth = depth.checked_sub(1)?,
            b':' if depth == 0 && bytes.get(index + 1) == Some(&b':') => {
                push_segment(&mut path, &rest[start..index])?;
                index += 2;
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    if depth != 0 {
        return None;
    }
    push_segment(&mut path, &rest[start..])?;
    (!path.is_empty()).then_some(path)
}

/// Adds one written segment without its generic arguments; a turbofish argument list
/// (`::<A>`) follows a named segment and adds none.
fn push_segment<'text>(path: &mut Vec<&'text str>, part: &'text str) -> Option<()> {
    let part = part.trim();
    let name = match part.find('<') {
        Some(0) => {
            path.last()?;
            return part.ends_with('>').then_some(());
        }
        Some(open) => part.ends_with('>').then(|| part[..open].trim())?,
        None => part,
    };
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_alphabetic() || character == '_')
        || !characters.all(|character| character.is_alphanumeric() || character == '_')
    {
        return None;
    }
    path.push(name);
    Some(())
}

/// The byte index of the `>` closing the generic group `text` opens with `<`.
fn generic_end(text: &str, work: &mut Work) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0_usize;
    for (index, byte) in bytes.iter().enumerate() {
        if !work.charge() {
            return None;
        }
        match byte {
            b'<' => depth = depth.checked_add(1)?,
            b'>' if index == 0 || bytes[index - 1] != b'-' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_type_binding(kind: StaticBindingKind) -> bool {
    // Value bindings do not shadow paths in Rust's type namespace.
    matches!(
        kind,
        StaticBindingKind::RustUse { .. } | StaticBindingKind::RustTypeParameter
    )
}

fn is_value_binding(kind: StaticBindingKind) -> bool {
    matches!(
        kind,
        StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::RustUse { .. }
            | StaticBindingKind::RustReceiver { .. }
            | StaticBindingKind::RustValue { .. }
    )
}

fn namespace_visible(source: &[String], namespace: &[String], visibility: Option<&str>) -> bool {
    match visibility {
        Some("pub") => true,
        Some("pub(crate)") => source.first() == namespace.first(),
        Some("pub(super)") => namespace
            .split_last()
            .is_some_and(|(_, parent)| source.starts_with(parent)),
        _ => source.starts_with(namespace),
    }
}
