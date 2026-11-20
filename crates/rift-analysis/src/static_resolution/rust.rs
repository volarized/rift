//! Resolves Rust paths admitted by the captured crate and module inventory.

mod references;

// Exhausting initializer lookup depth leaves the authored site unresolved.
const RECEIVER_DEPTH_MAX: usize = 64;

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBindingKind, StaticCall, StaticCallee, SyntaxSymbol};

use super::{Binding, File, Target, Work};

/// Qualified Rust declarations grouped by their admitted crate and module path.
#[derive(Default)]
pub(super) struct Namespaces<'source> {
    files: BTreeMap<&'source str, FileNamespace<'source>>,
    values: BTreeMap<Vec<String>, Option<Target<'source>>>,
    types: BTreeMap<Vec<String>, Option<Target<'source>>>,
    methods: BTreeMap<Vec<String>, Vec<Method<'source>>>,
    members: BTreeMap<Vec<String>, Option<Target<'source>>>,
    field_types: BTreeMap<Vec<String>, Target<'source>>,
    bounds: BTreeMap<(&'source str, ByteRange), Vec<ByteRange>>,
    unavailable: BTreeSet<(&'source str, ByteRange)>,
    return_owners: BTreeMap<(&'source str, ByteRange), Target<'source>>,
    initializers: BTreeMap<(&'source str, ByteRange), Option<&'source StaticCall>>,
    conditional_modules: BTreeSet<Vec<String>>,
    inactive_modules: BTreeSet<Vec<String>>,
    modules: BTreeMap<Vec<String>, Option<Target<'source>>>,
    crate_roots: BTreeMap<String, Option<&'source str>>,
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
        let parts = path_segments(file.text(range)?, work)?;
        if let [name] = parts.as_slice()
            && let Some(target) = Self::local_type(file, range.start, name, work)
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

    fn local_type(
        file: &File<'source>,
        position: u64,
        name: &str,
        work: &mut Work,
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
            if let Some(target) = file.qualified_target(&qualified)
                && target.symbol.kinds.contains(&SymbolKind::Type)
            {
                return Some(target);
            }
            container = container.rsplit_once("::")?.0;
        }
    }

    pub(super) fn new(
        files: &BTreeMap<&'source str, File<'source>>,
        work: &mut Work,
    ) -> Option<Self> {
        let mut namespaces = Self::default();
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "rust")
        {
            namespaces.index_file(file, work)?;
        }
        namespaces.index_aliases(files, work)?;
        namespaces.index_members(files, work)?;
        namespaces.index_methods(files, work)?;
        namespaces.index_field_types(files, work)?;
        namespaces.index_bounds(files, work)?;
        namespaces.index_returns(files, work)?;
        Some(namespaces)
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
                let Some(callable) = file.target_at(name).and_then(Target::callable) else {
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
                    self.return_owners.insert((path, name), owner);
                }
            }
        }
        Some(())
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
                if symbol.kind == "impl_item" || file.conditional(symbol.item_range, work) {
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
        for (path, target) in &self.members {
            if !work.charge() {
                return None;
            }
            let Some(target) = target.filter(|target| {
                target.symbol.kind == "field_declaration" && self.available(*target)
            }) else {
                continue;
            };
            let file = files.get(target.file.path.as_str())?;
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
            base: Vec::new(),
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
            if namespace.base.is_empty() {
                let depth = if structural_owner(target) {
                    symbol
                        .container
                        .as_deref()
                        .map_or(0, |container| container.split("::").count())
                        + 1
                } else {
                    symbol.qualified_name.split("::").count()
                };
                namespace.base = path.get(..path.len().checked_sub(depth)?)?.to_vec();
            }
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
            insert(&mut self.members, path.clone(), target);
        }
        if symbol.kind == "variant" {
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
        if file.conditional(range, work) || file.blocked(range.start, work) {
            return None;
        }
        let parts = file.use_parts(range, work)?;
        let parts = parts.iter().map(String::as_str).collect::<Vec<_>>();
        let path = self.absolute_path(file, range.start, &parts, work)?;
        self.lookup(file, range.start, &path, Namespace::Value, work)
            .or_else(|| self.lookup(file, range.start, &path, Namespace::Type, work))
    }

    fn direct_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = file.text(call.callee_range)?;
        let position = call.range.start;
        match file.binding(name, position, is_value_binding, work) {
            Binding::Found(binding) => match binding.kind {
                StaticBindingKind::Local => Self::local_callable(file, binding.name, work),
                StaticBindingKind::RustUse { path } => {
                    let imported = self.import(file, path, work)?;
                    (imported.symbol.kinds.contains(&SymbolKind::Callable)
                        || matches!(imported.symbol.kind, "struct" | "variant"))
                    .then_some(imported)
                }
                _ => None,
            },
            Binding::Missing => {
                let mut path = self.module(file, position, work)?.to_vec();
                path.push(name.to_owned());
                self.lookup(file, position, &path, Namespace::Value, work)
                    .or_else(|| self.glob_target(file, position, name, Namespace::Value, work))
            }
            Binding::Blocked => None,
        }
    }

    fn local_callable(
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let target = file.target_at(range)?.callable()?;
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
        if let ["Self", member] = parts.as_slice()
            && let Some(owner) = self.structural_self(file, position, work)
        {
            return self.inherent_method(file, position, owner, member, work);
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
        let owner = self.lookup(file, position, receiver, Namespace::Type, work)?;
        self.inherent_method(file, position, owner, member, work)
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
        let mut selected = None;
        let mut inherent = false;
        for method in candidates {
            if !work.charge() {
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
            if inherent && trait_impl {
                continue;
            }
            if !self.available(method.target) {
                return None;
            }
            if !trait_impl && !inherent {
                selected = None;
                inherent = true;
            }
            if selected.is_some() {
                return None;
            }
            selected = Some(method.target);
        }
        let target = selected?;
        (target.symbol.visibility.as_deref() == Some("pub")
            || self.accessible(file, position, &path, target, work)
            || target.symbol.visibility.is_none())
        .then_some(target)
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
        };
        let target = index
            .get(path)
            .copied()
            .flatten()
            .filter(|target| self.available(*target))?;
        self.accessible(file, position, path, target, work)
            .then_some(target)
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
        match file.binding(head, position, is_type_binding, work) {
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
                    && let Some(target) =
                        self.glob_target(file, position, head, Namespace::Type, work)
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

    fn glob_target(
        &self,
        file: &File<'source>,
        position: u64,
        name: &str,
        namespace: Namespace,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut selected: Option<Target<'source>> = None;
        for imported in file.facts.rust_uses() {
            if !work.charge() {
                return None;
            }
            if !imported.glob || !imported.scope.contains(position) {
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
            if selected.is_some_and(|previous| previous.symbol != target.symbol) {
                return None;
            }
            selected = Some(target);
        }
        selected
    }

    fn module(&self, file: &File<'source>, position: u64, work: &mut Work) -> Option<&[String]> {
        let namespace = self.files.get(file.document.path.as_str())?;
        let mut path = namespace.base.as_slice();
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
                && symbol
                    .body_range
                    .is_some_and(|body| body.contains(position))
            {
                path = inline;
            }
        }
        for length in 1..=path.len() {
            if !work.charge() || self.module_unavailable(&path[..length]) {
                return None;
            }
        }
        Some(path)
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
        self.conditions.iter().any(|(item, admitted)| {
            !work.charge() || item.contains(range.start) && *admitted != Some(true)
        })
    }

    pub(super) fn inactive(&self, range: ByteRange, work: &mut Work) -> bool {
        self.conditions.iter().any(|(item, admitted)| {
            !work.charge() || item.contains(range.start) && *admitted == Some(false)
        })
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

fn structural_owner(target: Target<'_>) -> bool {
    target.symbol.kind == "impl_item" && target.symbol.name.starts_with(['&', '['])
}

fn insert<T>(index: &mut BTreeMap<Vec<String>, Option<T>>, path: Vec<String>, target: T) {
    index
        .entry(path)
        .and_modify(|entry| *entry = None)
        .or_insert(Some(target));
}

fn path_segments<'text>(source: &'text str, work: &mut Work) -> Option<Vec<&'text str>> {
    let mut path = Vec::new();
    for part in source.split("::") {
        if !work.charge() {
            return None;
        }
        let part = part.trim();
        let mut characters = part.chars();
        if !characters
            .next()
            .is_some_and(|character| character.is_alphabetic() || character == '_')
            || !characters.all(|character| character.is_alphanumeric() || character == '_')
        {
            return None;
        }
        path.push(part);
    }
    Some(path)
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
