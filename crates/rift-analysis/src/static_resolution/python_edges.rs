//! Resolves Python names through captured declarations, imports, and receivers.

mod aliases;
mod annotations;
mod classes;
mod elements;
mod names;
mod overloads;
mod rebinding;
mod receiver_guards;
mod references;
mod slots;

pub(super) use aliases::{ClassAliases, ModuleAliases};
pub(super) use classes::{ClassOrders, MemberOwners};
pub(super) use names::ReassignedLocalRead;
pub(super) use references::PythonNameSites;

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::index::python_identifier_is_valid;
use rift_protocol::read::SymbolKind;
use rift_syntax::{
    ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee, SyntaxSymbol,
};

use super::{Binding, File, Index, StaticOccurrence, Target, Work};
use crate::analyzer::namespace::python::python_import_target;

/// An established import terminal beside its written from-module token.
pub(super) struct PythonImportQualifier<'source> {
    pub(super) module: ByteRange,
    terminal: Target<'source>,
    from: rift_core::SymbolId,
}

impl<'source> PythonImportQualifier<'source> {
    pub(super) fn from_import(
        binding: &StaticBinding,
        terminal: Target<'source>,
        occurrence: &StaticOccurrence,
    ) -> Option<Self> {
        let StaticBindingKind::PythonImport {
            module,
            export: Some(_),
        } = binding.kind
        else {
            return None;
        };
        Some(Self {
            module,
            terminal,
            from: occurrence.edge_from.as_ref()?.clone(),
        })
    }
}

pub(super) struct EdgePythonModules {
    paths: BTreeMap<String, Option<String>>,
    stubs: BTreeMap<String, String>,
}

impl EdgePythonModules {
    pub(super) fn build_edge(files: &BTreeMap<&str, File<'_>>, work: &mut Work) -> Option<Self> {
        let mut paths = BTreeMap::<_, Option<String>>::new();
        let mut stubs = BTreeMap::new();
        for file in files
            .values()
            .filter(|file| file.document.facts.language().name == "python")
        {
            if !work.charge() {
                return None;
            }
            let Some(module) = file.qualified_target(".").and_then(Target::admitted_path) else {
                continue;
            };
            if !module.iter().all(|part| python_identifier_is_valid(part)) {
                continue;
            }
            let path = file.document.path.as_str();
            paths
                .entry(module.join("."))
                .and_modify(|entry| {
                    if let Some(implementation) = entry.as_deref() {
                        // Physical path order visits a .py file before its adjacent .pyi.
                        if path
                            .strip_suffix(".pyi")
                            .is_some_and(|stem| implementation.strip_suffix(".py") == Some(stem))
                        {
                            stubs.insert(implementation.to_owned(), path.to_owned());
                            return;
                        }
                        stubs.remove(implementation);
                    }
                    *entry = None;
                })
                .or_insert_with(|| Some(path.to_owned()));
        }
        Some(Self { paths, stubs })
    }

    fn path(&self, name: &str) -> Option<&str> {
        self.paths.get(name)?.as_deref()
    }

    fn same_module(&self, left: &str, right: &str) -> bool {
        left == right
            || self.stubs.get(left).is_some_and(|stub| stub == right)
            || self.stubs.get(right).is_some_and(|stub| stub == left)
    }

    fn name(from: &File<'_>, module: &str) -> Option<String> {
        let anchor = from.qualified_target(".")?.admitted_path()?;
        if module.starts_with('.') && !anchor.iter().all(|part| python_identifier_is_valid(part)) {
            return None;
        }
        let package = from
            .document
            .path
            .as_str()
            .rsplit('/')
            .next()
            .is_some_and(|name| matches!(name, "__init__.py" | "__init__.pyi"));
        Some(python_import_target(&anchor, package, module)?.join("."))
    }
}

/// A name a module binds, by the module's path.
type ModuleName<'source> = (&'source str, &'source str);

/// Where the imports of each name lead: the last name a chain of imports reaches, or `None`
/// where the chain is a cycle.
///
/// A name another file writes to is no link: whether its slot is available depends on the
/// writer.
#[derive(Default)]
pub(super) struct ImportTargets<'source>(
    BTreeMap<ModuleName<'source>, Option<ModuleName<'source>>>,
);

/// What covers a Python file beside its module.
#[derive(Clone, Copy)]
pub(super) enum WholeFile {
    /// Nothing else.
    Module,
    /// The one definition of the file, by its name.
    Definition(ByteRange),
    /// One comprehension, generator expression or lambda.
    Expression,
}

/// A scope that binds a name: its range, and whether it is the module.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Scope {
    range: ByteRange,
    module: bool,
}

impl Scope {
    fn of(file: &File<'_>, binding: &StaticBinding) -> Self {
        Self {
            range: binding.scope,
            module: file.python_module_binding(binding),
        }
    }

    /// Whether this scope holds `other`. A module holds every other scope of its file.
    fn holds(self, other: Self) -> bool {
        if other.module {
            self.module
        } else {
            self.module || self.range.encloses(other.range)
        }
    }
}

enum Value<'source> {
    Symbol(Target<'source>),
    Instance(Target<'source>),
    // A member retains its declaration separately from its receiver value.
    Member {
        declaration: Target<'source>,
        assigned: Target<'source>,
    },
    Module(String),
}

#[derive(Clone, Copy)]
enum ReceiverJoin {
    All,
    NonNone,
}

impl<'source> Index<'source> {
    pub(super) fn edge_python_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if matches!(call.callee, StaticCallee::Unsupported) {
            return None;
        }
        let target = match call.callee {
            StaticCallee::Member { receiver, member } if is_super(file.text(receiver)?) => {
                self.python_super_member(file, call, receiver, member, work)?
            }
            _ => self.edge_python_name(file, call.callee_range, work)?,
        };
        self.python_callable_alias(target, work)
    }

    /// A local name bound more than once in one callable scope, by assignments that all
    /// produce an instance of the same class, holds that instance wherever it is read: in
    /// every branch and on every loop iteration. Returns the earliest available binding.
    fn python_receiver_join(
        &self,
        file: &File<'source>,
        scope: Scope,
        candidates: impl Iterator<Item = &'source StaticBinding>,
        work: &mut Work,
    ) -> Option<&'source StaticBinding> {
        if scope.module || file.class_scopes.contains(&scope.range) {
            return None;
        }
        self.python_agreed_receiver(file, candidates, ReceiverJoin::All, work)
    }

    fn python_agreed_receiver(
        &self,
        file: &File<'source>,
        candidates: impl Iterator<Item = &'source StaticBinding>,
        mode: ReceiverJoin,
        work: &mut Work,
    ) -> Option<&'source StaticBinding> {
        let mut agreed = None;
        let mut selected: Option<&StaticBinding> = None;
        let mut count = 0_usize;
        let mut has_none = false;
        for binding in candidates {
            if !work.charge() {
                return None;
            }
            if matches!(mode, ReceiverJoin::NonNone)
                && binding.kind == StaticBindingKind::PythonNone
            {
                has_none = true;
                continue;
            }
            if !matches!(
                binding.kind,
                StaticBindingKind::PythonAnnotation { .. }
                    | StaticBindingKind::PythonInstance { .. }
                    | StaticBindingKind::PythonAlias { .. }
                    | StaticBindingKind::PythonElement { .. }
                    | StaticBindingKind::PythonUnpacked { .. }
            ) {
                return None;
            }
            let value = work.python_lookup(|work| {
                let value = self.binding_value(file, binding, work)?;
                self.python_receiver(value, work)
            })?;
            let Value::Instance(class) = value else {
                return None;
            };
            let class = (
                class.file.path.as_str(),
                class.symbol.qualified_name.as_str(),
            );
            if agreed.is_some_and(|agreed| agreed != class) {
                return None;
            }
            agreed = Some(class);
            count += 1;
            if selected.is_none_or(|earliest| binding.available_from < earliest.available_from) {
                selected = Some(binding);
            }
        }
        let has_agreement = match mode {
            ReceiverJoin::All => count > 1,
            ReceiverJoin::NonNone => has_none,
        };
        has_agreement.then_some(selected?)
    }

    /// A class base names a class, directly or through unchanged aliases; other values stay
    /// unresolved.
    pub(super) fn edge_python_base(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let target = self.edge_python_name(file, range, work)?;
        self.python_callable_alias(target, work)
            .filter(|target| target.symbol.kinds.contains(&SymbolKind::Constructible))
    }

    /// Follows unchanged variable aliases (`alias = target`) to the callable or class they name.
    fn python_callable_alias(
        &self,
        mut target: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut visited = BTreeSet::new();
        loop {
            if target.symbol.kinds.contains(&SymbolKind::Callable)
                || target.symbol.kinds.contains(&SymbolKind::Constructible)
            {
                return Some(target);
            }
            let name = target.symbol.name_range?;
            if !work.charge() || !visited.insert((target.file.path.as_str(), name.start)) {
                return None;
            }
            if let Some(class) = self.python_aliases.target(target) {
                return work.python_lookup(|_| Some(class));
            }
            let file = self.files.get(target.file.path.as_str())?;
            let StaticBindingKind::PythonAlias { value } = *file.python_values.get(&name.end)?
            else {
                return None;
            };
            target = self.edge_python_name(file, value, work)?;
        }
    }

    fn python_super_member(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        receiver: ByteRange,
        member: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        for binding in file.by_name.get("super").into_iter().flatten() {
            if !work.charge() || binding.scope.contains(receiver.start) {
                return None;
            }
        }
        let caller = file.enclosing_callable(call.range)?;
        let receivers = file.python_receivers.get(&caller.symbol.item_range)?;
        for &binding in receivers {
            if !work.charge() {
                return None;
            }
            if let StaticBindingKind::PythonReceiver { class_name } = binding.kind {
                let selected = edge_python_binding(
                    self,
                    file,
                    file.text(binding.name)?,
                    call.range.start,
                    work,
                )?;
                if selected != binding {
                    return None;
                }
                let class = file.target_at(class_name)?;
                // `super()` reads class slots; an attribute of the instance is none of them.
                return self.python_declared_base_member(class, file.text(member)?, work);
            }
        }
        None
    }

    pub(super) fn edge_python_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        work.python_lookup(|work| self.python_name(file, range, work))
            .and_then(|value| self.value_target(value))
    }

    fn python_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let (value, suffix) = self.python_name_start(file, range, work)?;
        let mut value = value;
        for member in suffix
            .split('.')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if !work.charge() {
                return None;
            }
            value = match value {
                Value::Module(name) => match self
                    .edge_python
                    .path(&name)
                    .and_then(|path| self.files.get(path))
                {
                    Some(file) => self.export_value(file, member, work)?,
                    None => self.module_value(&format!("{name}.{member}"))?,
                },
                value => self.python_member_value(value, member, work)?,
            };
        }
        Some(value)
    }

    fn python_name_start(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<(Value<'source>, &'source str)> {
        if !work.charge() {
            return None;
        }
        if let Some(call) = file.python_longest_call(range) {
            let target = self.edge_python_call(file, call, work)?;
            let class = self.python_call_result(target, work)?;
            let rest = file
                .text(ByteRange {
                    start: call.range.end,
                    end: range.end,
                })?
                .trim();
            return Some((Value::Instance(class), rest.strip_prefix('.')?));
        }
        let mut names = file.text(range)?.splitn(2, '.').map(str::trim);
        let name = names.next()?;
        let binding = edge_python_binding(self, file, name, range.start, work)?;
        let value = self.binding_value(file, binding, work)?;
        Some((value, names.next().unwrap_or("")))
    }

    pub(super) fn edge_python_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if !matches!(binding.kind, StaticBindingKind::PythonModule { .. })
            && let Some(target) = self
                .binding_value(file, binding, work)
                .and_then(|value| self.value_target(value))
        {
            return Some(target);
        }
        let module = match binding.kind {
            StaticBindingKind::PythonModule { module } => file.text(module)?.to_owned(),
            StaticBindingKind::PythonImport { module, .. } => {
                EdgePythonModules::name(file, file.text(module)?)?
            }
            _ => return self.value_target(self.binding_value(file, binding, work)?),
        };
        self.module_value(&module)
            .and_then(|value| self.value_target(value))
    }

    /// Keeps the written module when the named import resolves across a re-export.
    pub(super) fn python_written_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        terminal: Target<'source>,
        original: &StaticOccurrence,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let StaticBindingKind::PythonImport {
            module,
            export: Some(_),
        } = binding.kind
        else {
            return None;
        };
        if terminal.symbol.kind == "module" {
            return None;
        }
        let name = EdgePythonModules::name(file, file.text(module)?)?;
        let written = self.files.get(self.edge_python.path(&name)?)?;
        if self
            .edge_python
            .same_module(terminal.file.path.as_str(), written.document.path.as_str())
            || !work.charge()
        {
            return None;
        }
        let mut occurrence = self.occurrence(
            file,
            module,
            module,
            rift_core::ReferenceRole::Import,
            written.qualified_target(".")?,
        )?;
        occurrence.edge_from = Some(original.edge_from.as_ref()?.clone());
        Some(occurrence)
    }

    pub(super) fn python_import_qualifier(
        &self,
        file: &File<'source>,
        site: PythonImportQualifier<'source>,
    ) -> Option<StaticOccurrence> {
        let text = file.text(site.module)?;
        if text.bytes().all(|byte| byte == b'.') {
            return None;
        }
        let name = EdgePythonModules::name(file, text)?;
        let written = self.files.get(self.edge_python.path(&name)?)?;
        let target = written.qualified_target(".")?;
        if site.terminal.symbol.kind == "module" && site.terminal.file.path == target.file.path {
            return None;
        }
        let mut occurrence = self.occurrence(
            file,
            site.module,
            site.module,
            rift_core::ReferenceRole::Import,
            target,
        )?;
        occurrence.edge_from = Some(site.from);
        Some(occurrence)
    }

    fn value_target(&self, value: Value<'source>) -> Option<Target<'source>> {
        match value {
            Value::Symbol(target)
            | Value::Member {
                declaration: target,
                ..
            } => Some(target),
            Value::Instance(_) => None,
            Value::Module(name) => self
                .files
                .get(self.edge_python.path(&name)?)?
                .qualified_target("."),
        }
    }

    fn binding_value(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        if let StaticBindingKind::PythonAnnotation { annotation }
        | StaticBindingKind::PythonGuard { annotation } = binding.kind
        {
            return file.target_at(binding.name).map(Value::Symbol).or_else(|| {
                self.python_annotation(file, annotation, work)
                    .map(Value::Instance)
            });
        }
        self.declaration_value(file, binding, work)
    }

    fn declaration_value(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let declared = self.declared_value(file, binding, work);
        if declared.is_some() {
            return declared;
        }
        let element = match binding.kind {
            StaticBindingKind::PythonElement { iterable, position } => {
                self.python_element(file, iterable, position, work)?
            }
            StaticBindingKind::PythonUnpacked { value, position } => {
                self.python_unpacked(file, value, position, work)?
            }
            _ => return None,
        };
        Some(Value::Instance(element))
    }

    /// The value a binding declares or imports, without the container a name iterates or
    /// unpacks.
    fn declared_value(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        match binding.kind {
            StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::PythonNone
            | StaticBindingKind::PythonEmptyList
            | StaticBindingKind::PythonInstance { .. }
            | StaticBindingKind::PythonAlias { .. }
            | StaticBindingKind::PythonElement { .. }
            | StaticBindingKind::PythonUnpacked { .. }
            | StaticBindingKind::PythonFunction { .. } => {
                file.target_at(binding.name).map(Value::Symbol)
            }
            StaticBindingKind::PythonReceiver { class_name } => {
                let class = file.target_at(class_name)?;
                let method = file.enclosing_callable(binding.name)?;
                Some(if method.symbol.kinds.contains(&SymbolKind::Static) {
                    Value::Symbol(class)
                } else {
                    Value::Instance(class)
                })
            }
            StaticBindingKind::PythonImport { module, export } => {
                let name = EdgePythonModules::name(file, file.text(module)?)?;
                if let Some(export) = export {
                    let exported = file.text(export)?;
                    if let Some(path) = self.edge_python.path(&name) {
                        self.export_value(self.files.get(path)?, exported, work)
                    } else {
                        self.module_value(&format!("{name}.{exported}"))
                    }
                } else {
                    self.module_value(&name)
                }
            }
            StaticBindingKind::PythonModule { module } => {
                // Importing a dotted path binds its first component.
                let name = file.text(module)?.split('.').next()?;
                self.module_value(name)
            }
            _ => None,
        }
    }

    /// Follows every import of every Python module once.
    pub(super) fn python_import_targets(
        &self,
        work: &mut Work,
    ) -> Option<(ImportTargets<'source>, ModuleAliases<'source>)> {
        let mut targets = ImportTargets::default();
        let mut aliases = Vec::new();
        let collect_aliases = work.budget.remaining() > super::FILE_WORK_FLOOR;
        for file in self.python_files() {
            for binding in file.facts.bindings() {
                if !work.charge() {
                    return None;
                }
                if matches!(
                    binding.kind,
                    StaticBindingKind::PythonImport {
                        export: Some(_),
                        ..
                    }
                ) {
                    let name = (file.document.path.as_str(), file.text(binding.name)?);
                    self.follow_imports(name, &mut targets, work)?;
                } else if collect_aliases
                    && matches!(binding.kind, StaticBindingKind::PythonAlias { .. })
                    && file.python_module_binding(binding)
                {
                    aliases.push((file.document.path.as_str(), binding));
                }
            }
        }
        Some((targets, aliases))
    }

    /// Follows the imports of `name` up to a name already followed, the end of the chain, or
    /// a name of the chain itself.
    fn follow_imports(
        &self,
        mut name: ModuleName<'source>,
        targets: &mut ImportTargets<'source>,
        work: &mut Work,
    ) -> Option<()> {
        let mut chain = BTreeSet::new();
        let target = loop {
            if !work.charge() {
                return None;
            }
            if let Some(known) = targets.0.get(&name) {
                break *known;
            }
            if !chain.insert(name) {
                break None;
            }
            match self.imported_name(name, work) {
                Some(next) => name = next,
                None if work.exhausted => return None,
                None => break Some(name),
            }
        };
        targets
            .0
            .extend(chain.into_iter().map(|name| (name, target)));
        Some(())
    }

    /// The name of a selected module that the one binding of `name` imports.
    fn imported_name(
        &self,
        (path, name): ModuleName<'source>,
        work: &mut Work,
    ) -> Option<ModuleName<'source>> {
        if self.python_writes.contains_key(name) {
            return None;
        }
        let file = self.files.get(path)?;
        let bindings = file.by_name.get(name)?.iter().copied();
        let root = bindings.filter(|binding| file.python_module_binding(binding));
        let binding = self.python_binding_group(file, root, work)?;
        let StaticBindingKind::PythonImport {
            module,
            export: Some(export),
        } = binding.kind
        else {
            return None;
        };
        file.qualified_target(".")?.admitted_path()?;
        let module = EdgePythonModules::name(file, file.text(module)?)?;
        let exported = file.text(export)?;
        let target = self.edge_python.path(&module)?;
        // A package importing a name from itself binds its submodule.
        if self.edge_python.same_module(target, path) && exported == name {
            return None;
        }
        let target = self.python_export_file(self.files.get(target)?, exported, work)?;
        Some((target.document.path.as_str(), exported))
    }

    fn module_value(&self, name: &str) -> Option<Value<'source>> {
        let prefix = format!("{name}.");
        (self.edge_python.path(name).is_some()
            || self
                .edge_python
                .paths
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(module, _)| module.starts_with(&prefix)))
        .then(|| Value::Module(name.to_owned()))
    }

    /// Selects the physical file declaring a name of a module with an adjacent stub.
    fn python_export_file<'index>(
        &'index self,
        implementation: &'index File<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<&'index File<'source>> {
        let path = implementation.document.path.as_str();
        let Some(stub) = self.edge_python.stubs.get(path) else {
            return Some(implementation);
        };
        if implementation.document.facts.has_errors()
            || implementation.python_globals.contains_key(name)
            || implementation.blocked(0, work)
        {
            return Some(implementation);
        }
        for binding in implementation.by_name.get(name).into_iter().flatten() {
            if !work.charge() {
                return None;
            }
            if implementation.python_module_binding(binding)
                && !matches!(binding.kind, StaticBindingKind::PythonGuard { .. })
            {
                return Some(implementation);
            }
        }
        self.files.get(stub.as_str())
    }

    fn export_value<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut name: &'index str,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        file = self.python_export_file(file, name, work)?;
        if let Some(target) = self
            .python_imports
            .0
            .get(&(file.document.path.as_str(), name))
        {
            let (path, imported) = (*target)?;
            file = self.files.get(path)?;
            name = imported;
        }
        let mut visited = BTreeSet::new();
        loop {
            if !work.charge() || !visited.insert((file.document.path.as_str(), name)) {
                return None;
            }
            let module = file.qualified_target(".")?.admitted_path()?.join(".");
            let Some(bindings) = file.by_name.get(name) else {
                return self.module_value(&format!("{module}.{name}"));
            };
            let root = bindings
                .iter()
                .copied()
                .filter(|binding| file.python_module_binding(binding));
            let binding = self.python_binding_group(file, root, work)?;
            let callable = file.target_at(binding.name).is_some_and(|target| {
                target.symbol.kinds.contains(&SymbolKind::Callable)
                    || target.symbol.kinds.contains(&SymbolKind::Constructible)
            });
            if (callable
                || matches!(
                    binding.kind,
                    StaticBindingKind::PythonImport { .. } | StaticBindingKind::PythonModule { .. }
                ))
                && !self.python_module_slot_available(file, name, work)
            {
                return None;
            }
            match binding.kind {
                StaticBindingKind::Local
                | StaticBindingKind::Shadow
                | StaticBindingKind::PythonAnnotation { .. }
                | StaticBindingKind::PythonInstance { .. }
                | StaticBindingKind::PythonAlias { .. }
                | StaticBindingKind::PythonElement { .. }
                | StaticBindingKind::PythonUnpacked { .. }
                | StaticBindingKind::PythonEmptyList
                | StaticBindingKind::PythonFunction { .. } => {
                    return file.target_at(binding.name).map(Value::Symbol);
                }
                StaticBindingKind::PythonImport { module, export } => {
                    let module = EdgePythonModules::name(file, file.text(module)?)?;
                    let Some(export) = export else {
                        return self.module_value(&module);
                    };
                    let exported = file.text(export)?;
                    let Some(path) = self.edge_python.path(&module) else {
                        return self.module_value(&format!("{module}.{exported}"));
                    };
                    if self
                        .edge_python
                        .same_module(path, file.document.path.as_str())
                        && exported == name
                    {
                        // A package importing a name from itself binds its submodule.
                        return self.module_value(&format!("{module}.{exported}"));
                    }
                    name = exported;
                    file = self.python_export_file(self.files.get(path)?, name, work)?;
                }
                StaticBindingKind::PythonModule { module } => {
                    return self.module_value(file.text(module)?.split('.').next()?);
                }
                _ => return None,
            }
        }
    }
}

impl<'source> File<'source> {
    /// The longest call that starts where `range` starts and lies inside it.
    fn python_longest_call(&self, range: ByteRange) -> Option<&'source StaticCall> {
        let calls = self.python_calls.get(&range.start)?;
        let inside = calls.partition_point(|call| call.range.end <= range.end);
        calls.get(..inside)?.last().copied()
    }
}

fn is_super(text: &str) -> bool {
    text.strip_prefix("super")
        .and_then(|rest| rest.trim().strip_prefix('('))
        .and_then(|rest| rest.trim().strip_suffix(')'))
        .is_some_and(|arguments| arguments.trim().is_empty())
}

fn edge_python_binding<'source>(
    index: &Index<'source>,
    file: &File<'source>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    match index.python_binding(file, name, position, work) {
        Binding::Found(binding) => Some(binding),
        Binding::Missing | Binding::Blocked => None,
    }
}

/// The scope one position reads a name in.
struct NameScope {
    scope: Scope,
    /// The innermost scope that redirects this name with `global` or `nonlocal`.
    redirected: Option<Scope>,
    /// Whether a scope that declares the name `global` or `nonlocal` binds it as well.
    rebound: bool,
}

impl NameScope {
    /// A definite deletion in the body that redirects the name to an enclosing scope.
    fn redirected_deletion(
        &self,
        file: &File<'_>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> Option<(Scope, u64)> {
        let scope = self.redirected?;
        let end = file.python_deletion_before(name, scope, position, work)?;
        Some((scope, end))
    }
}

enum ScopeLookup {
    Found(NameScope),
    /// No scope the read reaches binds the name.
    Missing,
    /// The work ended or the declarations of the name conflict.
    Blocked,
}

impl<'source> Index<'source> {
    /// The binding a read of `name` at `position` selects; missing when no scope the read
    /// reaches binds the name.
    pub(super) fn python_binding(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> Binding<'source> {
        if file.blocked(position, work) {
            return Binding::Blocked;
        }
        let Some(end) = position.checked_add(1) else {
            return Binding::Blocked;
        };
        let around = ByteRange {
            start: position,
            end,
        };
        let bindings = &file.bindings_around(name, around).collect::<Vec<_>>();
        let selected = match file.python_scope(bindings, position, work) {
            ScopeLookup::Found(selected) => selected,
            ScopeLookup::Missing => return Binding::Missing,
            ScopeLookup::Blocked => return Binding::Blocked,
        };
        self.python_scope_binding(file, name, &selected, bindings, position, work)
            .map_or(Binding::Blocked, Binding::Found)
    }

    /// The binding of `scope` that a read at `position` selects.
    fn python_scope_binding(
        &self,
        file: &File<'source>,
        name: &str,
        selected: &NameScope,
        bindings: &[&'source StaticBinding],
        position: u64,
        work: &mut Work,
    ) -> Option<&'source StaticBinding> {
        // After a definite deletion, only assignments in the deleting body can restore
        // the binding before this use, even when it is stored in an enclosing scope.
        let (scope, deletion) = match selected.redirected_deletion(file, name, position, work) {
            Some((scope, end)) => (scope, Some(end)),
            None if selected.rebound => return None,
            None => (
                selected.scope,
                file.python_deletion_before(name, selected.scope, position, work),
            ),
        };
        if work.exhausted {
            return None;
        }
        let later = later_bindings(file, bindings, scope, position, work)?;
        let candidates = bindings.iter().copied().filter(|binding| {
            Scope::of(file, binding) == scope
                && !later.contains(&binding.name.start)
                && deletion.is_none_or(|end| {
                    end <= binding.name.start
                        && binding.name.end <= position
                        && match binding.kind {
                            // An assignment with a value extends past its annotation.
                            StaticBindingKind::PythonAnnotation { annotation } => {
                                file.target_at(binding.name).is_some_and(|target| {
                                    annotation.end < target.symbol.item_range.end
                                })
                            }
                            _ => true,
                        }
                })
        });
        if deletion.is_some() {
            // A guard can narrow a rebound value. It cannot give a deleted name a value.
            let rebound = candidates.clone().any(|binding| {
                !work.charge() || !matches!(binding.kind, StaticBindingKind::PythonGuard { .. })
            });
            if !rebound || work.exhausted {
                return None;
            }
        }
        let selected = match self.python_binding_group(file, candidates.clone(), work) {
            Some(selected) => selected,
            None => self
                .python_receiver_join(file, scope, candidates.clone(), work)
                .or_else(|| {
                    self.python_guarded_receiver(file, name, scope, position, candidates, work)
                })?,
        };
        (selected.available_from <= position
            && file.python_guard_available(selected, position, work))
        .then_some(selected)
    }
}

impl File<'_> {
    /// The last deletion before a read in the same function and block.
    fn python_deletion_before(
        &self,
        name: &str,
        scope: Scope,
        position: u64,
        work: &mut Work,
    ) -> Option<u64> {
        let deletions = self.python_deletions.get(&(name, scope.range))?;
        let end = position.checked_add(1)?;
        let callable = self.enclosing_callable(ByteRange {
            start: position,
            end,
        })?;
        if callable.symbol.item_range != scope.range
            || callable
                .symbol
                .body_range
                .is_none_or(|body| !body.contains(position))
            || self.python_expressions.resolve(position, end).is_some()
        {
            return None;
        }
        let mut latest = None;
        for deletion in deletions {
            if !work.charge() {
                return None;
            }
            if deletion.name.end <= position && deletion.block.contains(position) {
                latest = latest.max(Some(deletion.name.end));
            }
        }
        latest
    }

    /// The scope a read at `position` finds a name of `bindings` in.
    fn python_scope(
        &self,
        bindings: &[&StaticBinding],
        position: u64,
        work: &mut Work,
    ) -> ScopeLookup {
        let mut excluded = BTreeSet::new();
        let mut global = false;
        let mut nonlocal = false;
        let mut rebound = false;
        let mut redirected = None;
        loop {
            let mut best: Option<Scope> = None;
            for &binding in bindings {
                if !work.charge() {
                    return ScopeLookup::Blocked;
                }
                let scope = Scope::of(self, binding);
                if !scope.range.contains(position)
                    || excluded.contains(&scope)
                    || (global && !scope.module)
                    || (nonlocal && scope.module)
                    || (!scope.module
                        && (self.python_evaluated_outside(scope.range, position)
                            || (self.class_scopes.contains(&scope.range)
                                && self.python_class_hidden(scope.range, position))))
                {
                    continue;
                }
                if best.is_none_or(|best| best.holds(scope)) {
                    best = Some(scope);
                }
            }
            let Some(scope) = best else {
                return ScopeLookup::Missing;
            };
            let mut declaration = None;
            let mut selected = false;
            for &binding in bindings {
                if !work.charge() {
                    return ScopeLookup::Blocked;
                }
                if Scope::of(self, binding) != scope {
                    continue;
                }
                match binding.kind {
                    StaticBindingKind::PythonGlobal => declaration = Some(true),
                    StaticBindingKind::PythonNonlocal => declaration = Some(false),
                    _ => {
                        selected = true;
                    }
                }
            }
            let Some(is_global) = declaration else {
                return ScopeLookup::Found(NameScope {
                    scope,
                    redirected,
                    rebound,
                });
            };
            if global || (nonlocal && is_global) {
                return ScopeLookup::Blocked;
            }
            redirected.get_or_insert(scope);
            rebound |= selected;
            excluded.insert(scope);
            global = is_global;
            nonlocal = !is_global;
        }
    }

    /// Whether the code at `position` runs in a function, class, lambda, comprehension or
    /// generator expression inside `class`, whose names it does not read.
    fn python_class_hidden(&self, class: ByteRange, position: u64) -> bool {
        let end = position.saturating_add(1);
        let runs = |callable: &&SyntaxSymbol| {
            class.encloses(callable.item_range)
                && callable
                    .body_range
                    .is_some_and(|body| body.contains(position))
        };
        let callable = match self.callables.containing(position, end) {
            Some(mut around) => around.any(runs),
            None => self.callables.resolve(position, end).is_some_and(runs),
        };
        let nested = |inner: &&SyntaxSymbol| {
            let inner = inner.item_range;
            class != inner
                && class.encloses(inner)
                && inner.contains(position)
                && !self.python_evaluated_outside(inner, position)
        };
        callable
            || match self.classes.containing(position, end) {
                Some(mut around) => around.any(nested),
                None => self.classes.resolve(position, end).is_some_and(nested),
            }
            || self
                .python_expressions
                .containing(position, end)
                .into_iter()
                .flatten()
                .any(|expression| {
                    class.encloses(*expression)
                        && !self.python_evaluated_outside(*expression, position)
                })
    }
}

/// Bindings of one name that cannot have run before a read at `position`.
///
/// In the body of the callable whose scope holds the name, and in module code outside callable
/// bodies and annotations, a binding that starts after the read cannot have run yet unless a
/// loop holds both, or the read sits in a lambda, comprehension or generator expression.
fn later_bindings(
    file: &File<'_>,
    bindings: &[&StaticBinding],
    scope: Scope,
    position: u64,
    work: &mut Work,
) -> Option<BTreeSet<u64>> {
    let mut later = BTreeSet::new();
    let end = position.saturating_add(1);
    let callable = file.enclosing_callable(ByteRange {
        start: position,
        end,
    });
    let in_body = callable.is_some_and(|callable| {
        let body = callable.symbol.body_range;
        body.is_some_and(|body| body.contains(position))
    });
    let ordered = ((in_body
        && !scope.module
        && callable.is_some_and(|callable| callable.symbol.item_range == scope.range))
        || (scope.module && !in_body && !file.in_python_annotation(position)))
        && file.python_expressions.resolve(position, end).is_none();
    if !ordered {
        return Some(later);
    }
    for &binding in bindings {
        if !work.charge() {
            return None;
        }
        let repeated = file
            .facts
            .loops()
            .iter()
            .any(|range| range.contains(position) && range.contains(binding.name.start));
        if Scope::of(file, binding) == scope && binding.available_from > position && !repeated {
            later.insert(binding.name.start);
        }
    }
    Some(later)
}

impl File<'_> {
    /// Whether `position` is in a part of `scope` that the enclosing scope evaluates.
    fn python_evaluated_outside(&self, scope: ByteRange, position: u64) -> bool {
        // The parts of one scope are disjoint. Check the last one that starts by this position.
        self.python_evaluated_outside
            .range((scope, 0)..=(scope, position))
            .next_back()
            .is_some_and(|(_, &end)| position < end)
    }

    fn python_guard_available(
        &self,
        selected: &StaticBinding,
        position: u64,
        work: &mut Work,
    ) -> bool {
        if !matches!(selected.kind, StaticBindingKind::PythonGuard { .. }) {
            return true;
        }
        let shadowed = self
            .by_name
            .get("isinstance")
            .into_iter()
            .flatten()
            .any(|binding| !work.charge() || binding.scope.contains(selected.name.start));
        if shadowed {
            return false;
        }
        let owner = self
            .enclosing_callable(selected.name)
            .map(|target| target.symbol.name_range);
        let current = self
            .enclosing_callable(ByteRange {
                start: position,
                end: position.saturating_add(1),
            })
            .map(|target| target.symbol.name_range);
        if owner != current {
            return false;
        }
        let Some(name) = self.text(selected.name) else {
            return false;
        };
        self.by_name.get(name).into_iter().flatten().all(|binding| {
            let later =
                binding.name.start >= selected.available_from && binding.name.start < position;
            work.charge() && !later
        })
    }
}
