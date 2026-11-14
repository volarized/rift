//! Resolves Rust paths admitted by the captured crate and module inventory.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolFacet;
use rift_syntax::{ByteRange, StaticBindingKind, StaticCall, StaticCallee, SyntaxSymbol};

use super::{Binding, File, Target, Work};

/// Qualified Rust declarations grouped by their admitted crate and module path.
#[derive(Default)]
pub(super) struct Namespaces<'source> {
    files: BTreeMap<&'source str, FileNamespace<'source>>,
    values: BTreeMap<Vec<String>, Option<Target<'source>>>,
    types: BTreeMap<Vec<String>, Option<Target<'source>>>,
    methods: BTreeMap<Vec<String>, Option<Method<'source>>>,
    conditional_modules: BTreeSet<Vec<String>>,
    modules: BTreeMap<Vec<String>, bool>,
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
        Some(namespaces)
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
            if symbol.container.is_none() && path.len() > 1 {
                namespace.base = path[..path.len() - 1].to_vec();
            }
            if symbol.kind == "module" {
                if file.conditional(symbol.item_range, work) {
                    self.conditional_modules.insert(path.clone());
                }
                if symbol.body_range.is_some() {
                    namespace.inline.push((symbol, path.clone()));
                }
                let public = symbol.visibility.as_deref() == Some("pub");
                self.modules
                    .entry(path)
                    .and_modify(|admitted| *admitted = false)
                    .or_insert(public);
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
        if file.conditional(symbol.item_range, work) {
            return;
        }
        let target = Target {
            file: file.document,
            symbol,
        };
        if symbol.facets.contains(&SymbolFacet::Type) && file.is_module_item(symbol) {
            insert(&mut self.types, path, target);
        } else if symbol.facets.contains(&SymbolFacet::Callable) {
            if let Some(implementation) = inherent_member(file, symbol, work) {
                insert(
                    &mut self.methods,
                    path,
                    Method {
                        target,
                        implementation,
                    },
                );
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
        if file.conditional(call.range, work) {
            return None;
        }
        match call.callee {
            StaticCallee::Direct => self.direct_call(file, call, work),
            StaticCallee::Qualified => self.qualified_call(file, call, work),
            StaticCallee::Member { receiver, member } => {
                let name = file.text(receiver)?;
                let binding = file.binding(name, call.range.start, is_value_binding, work);
                let Binding::Found(binding) = binding else {
                    return None;
                };
                let StaticBindingKind::RustReceiver { type_path } = binding.kind else {
                    return None;
                };
                if file.reassigned(name, None) {
                    return None;
                }
                self.method(file, type_path, file.text(member)?, work)
            }
            StaticCallee::Unsupported => None,
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
        let parts = path_segments(file.text(range)?, work)?;
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
                    imported.callable()
                }
                _ => None,
            },
            Binding::Missing => {
                let mut path = self.module(file, position, work)?.to_vec();
                path.push(name.to_owned());
                self.lookup(file, position, &path, Namespace::Value, work)
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
        let path = self.resolved_path(file, position, &parts, work)?;
        if let Some(target) = self.lookup(file, position, &path, Namespace::Value, work) {
            return Some(target);
        }
        let (member, receiver) = path.split_last()?;
        let owner = self.lookup(file, position, receiver, Namespace::Type, work)?;
        self.inherent_method(file, position, owner, member, work)
    }

    fn method(
        &self,
        file: &File<'source>,
        type_path: ByteRange,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let parts = path_segments(file.text(type_path)?, work)?;
        let path = self.resolved_path(file, type_path.start, &parts, work)?;
        let owner = self.lookup(file, type_path.start, &path, Namespace::Type, work)?;
        self.inherent_method(file, type_path.start, owner, member, work)
    }

    fn inherent_method(
        &self,
        file: &File<'source>,
        position: u64,
        owner: Target<'source>,
        member: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !matches!(owner.symbol.kind, "struct" | "enum") {
            return None;
        }
        let mut path = owner.admitted_path()?;
        path.push(member.to_owned());
        let method = self.methods.get(&path).copied().flatten()?;
        if file.document.path != method.target.file.path {
            return None;
        }
        let range = method.implementation.type_path;
        let type_path = path_segments(file.text(range)?, work)?;
        let implemented = self.resolved_path(file, range.start, &type_path, work)?;
        if implemented != path[..path.len() - 1] {
            return None;
        }
        self.accessible(file, position, &path, method.target, work)
            .then_some(method.target)
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
        let target = index.get(path).copied().flatten()?;
        self.accessible(file, position, path, target, work)
            .then_some(target)
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
            if !work.charge() || self.conditional_modules.contains(module) {
                return false;
            }
            if self.modules.get(module) == Some(&false)
                && !source.starts_with(&module[..length - 1])
            {
                return false;
            }
        }
        target.symbol.visibility.as_deref() == Some("pub")
            || source == target_namespace
            || (target.symbol.facets.contains(&SymbolFacet::Callable)
                && self.methods.get(path).copied().flatten().is_some()
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
        if matches!(*head, "crate" | "self" | "super") {
            return self.absolute_path(file, position, parts, work);
        }
        match file.binding(head, position, is_type_binding, work) {
            Binding::Found(binding) => match binding.kind {
                StaticBindingKind::RustUse { path } => {
                    let imported = path_segments(file.text(path)?, work)?;
                    let mut prefix = self.absolute_path(file, path.start, &imported, work)?;
                    prefix.extend(tail.iter().map(|part| (*part).to_owned()));
                    Some(prefix)
                }
                _ => None,
            },
            Binding::Missing => {
                let mut path = self.module(file, position, work)?.to_vec();
                path.extend(parts.iter().map(|part| (*part).to_owned()));
                Some(path)
            }
            Binding::Blocked => None,
        }
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
            _ => return None,
        };
        let mut path = base.to_vec();
        path.extend(tail.iter().map(|part| (*part).to_owned()));
        Some(path)
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
        Some(path)
    }
}

impl File<'_> {
    pub(super) fn conditional(&self, range: ByteRange, work: &mut Work) -> bool {
        self.facts
            .conditional_items()
            .iter()
            .any(|item| !work.charge() || item.contains(range.start) || range.contains(item.start))
    }

    fn in_impl_or_trait(&self, symbol: &SyntaxSymbol, work: &mut Work) -> bool {
        let Some(container) = symbol.container.as_deref() else {
            return false;
        };
        let Some(target) = self.qualified_target(container) else {
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
    let name = container.rsplit("::").next().unwrap_or(container);
    file.facts.inherent_impls().iter().find(|implementation| {
        work.charge()
            && implementation.range.encloses(symbol.item_range)
            && file.text(implementation.type_path) == Some(name)
    })
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
    matches!(kind, StaticBindingKind::RustUse { .. })
}

fn is_value_binding(kind: StaticBindingKind) -> bool {
    matches!(
        kind,
        StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::RustUse { .. }
            | StaticBindingKind::RustReceiver { .. }
    )
}
