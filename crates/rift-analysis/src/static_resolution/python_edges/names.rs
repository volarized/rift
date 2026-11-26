//! Python names read and written as values.

use std::collections::BTreeMap;

use rift_core::ReferenceRole;
use rift_protocol::read::{EdgeKind, SymbolKind};
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, SyntaxSymbol};

use super::super::{
    ExternalOccurrence, File, Index, StaticOccurrence, StaticResolution, Target, Work,
};
use super::{EdgePythonModules, ModuleName, Scope, ScopeLookup};

/// What one read or written name names.
#[derive(Clone)]
enum Name<'source> {
    /// A name without a declaration that another scope can read: a local, a parameter, a
    /// comprehension name, a builtin.
    Local,
    /// A name of a module, a class or an import whose bindings name no one declaration.
    Refused,
    Declared(Target<'source>),
    /// A declaration of a dependency or the standard library.
    External(rift_core::SymbolId),
}

impl Name<'_> {
    /// Whether both name one declaration.
    fn agrees(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Declared(left), Self::Declared(right)) => {
                left.file.path == right.file.path
                    && left.symbol.qualified_name == right.symbol.qualified_name
            }
            (Self::External(left), Self::External(right)) => left == right,
            _ => false,
        }
    }
}

/// What the names of modules that one lookup passed through name. A name whose lookup has not
/// ended is refused, so an import cycle ends.
type ImportedNames<'source> = BTreeMap<ModuleName<'source>, Name<'source>>;

impl<'source> Index<'source> {
    /// Records direct function-local reads after all earlier reference work.
    pub(in crate::static_resolution) fn python_local_reads(
        &self,
        file: &File<'source>,
        work: &mut Work,
        result: &mut StaticResolution,
        parameters: bool,
    ) {
        for &(read, name) in file.facts.python_local_reads() {
            let target = match file.python_parameters.get(&name) {
                Some(&Some(symbol)) if parameters => Some(Target {
                    file: file.document,
                    symbol,
                }),
                None if !parameters => file.target_at(name),
                _ => continue,
            };
            if !work.charge() {
                return;
            }
            if let Some(occurrence) = target
                .and_then(|target| file.python_local_target(read, name, target, work))
                .and_then(|target| self.python_access(file, read, ReferenceRole::Read, target))
                .filter(|site| site.edge_from.is_some())
            {
                result.record(Some(occurrence));
            }
        }
    }

    /// Records one read of a plain name.
    pub(super) fn python_name_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let occurrence = match self.python_shared_name(file, range, work) {
            Name::Local => return,
            Name::Refused => None,
            Name::Declared(target) => self.python_access(file, range, ReferenceRole::Read, target),
            Name::External(target) => {
                if let Some(external) = self.python_external_access(file, range, target) {
                    result.external.push(external);
                    return;
                }
                None
            }
        };
        result.record_kind(occurrence, EdgeKind::Reads);
    }

    /// The written root name of an already resolved path, when it names a module.
    pub(super) fn python_member_module_qualifier(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<StaticOccurrence> {
        let Name::Declared(target) = self.python_shared_name(file, range, work) else {
            return None;
        };
        if !target.symbol.kinds.contains(&SymbolKind::Module) {
            return None;
        }
        self.python_access(file, range, ReferenceRole::Read, target)
    }

    /// Records one write of a name that the module or a class holds.
    pub(super) fn python_name_write(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let occurrence = match self.python_written_name(file, range, work) {
            Name::Local => return,
            Name::Declared(target) => self.python_access(file, range, ReferenceRole::Write, target),
            Name::Refused | Name::External(_) => None,
        };
        result.record_kind(occurrence, EdgeKind::Writes);
    }

    /// The declaration of the name that a module or class holds. An import declares nothing
    /// here, so a write that replaces it names no declaration.
    fn python_written_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Name<'source> {
        let Some(name) = file.text(range) else {
            return Name::Refused;
        };
        if file.blocked(range.start, work) {
            return Name::Refused;
        }
        let around = file.bindings_around(name, range).collect::<Vec<_>>();
        let scope = match file.python_scope(&around, range.start, work) {
            ScopeLookup::Found(selected) => Some(selected.scope),
            ScopeLookup::Missing => None,
            ScopeLookup::Blocked => return Name::Refused,
        };
        let held: Vec<_> = match scope.filter(|scope| !scope.module) {
            Some(scope) => around
                .into_iter()
                .filter(|binding| Scope::of(file, binding) == scope)
                .collect(),
            None => file.python_held_bindings(name),
        };
        if held.iter().copied().any(imports) {
            return Name::Refused;
        }
        self.python_one_declaration(file, &held, work)
    }

    /// The declaration every binding of the read scope names.
    fn python_shared_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Name<'source> {
        let Some(name) = file.text(range) else {
            return Name::Refused;
        };
        let bindings = file
            .bindings_around(name, range)
            .filter(|binding| !matches!(binding.kind, StaticBindingKind::PythonGuard { .. }))
            .collect::<Vec<_>>();
        let selected = match file.python_scope(&bindings, range.start, work) {
            ScopeLookup::Found(selected) => selected,
            ScopeLookup::Missing => return self.python_module_name(file, name, range.start, work),
            ScopeLookup::Blocked => return Name::Refused,
        };
        let scope = selected.scope;
        let rebound = |work: &mut Work| {
            self.edge_python_name(file, range, work)
                .map_or(Name::Refused, Name::Declared)
        };
        let redirected = selected.redirected_deletion(file, name, range.start, work);
        if scope.module {
            if redirected.is_some() {
                return rebound(work);
            }
            return self.python_module_name(file, name, range.start, work);
        }
        let held = bindings
            .into_iter()
            .filter(|binding| Scope::of(file, binding) == scope)
            .collect::<Vec<_>>();
        let class = file.class_scopes.contains(&scope.range);
        if !class && !held.iter().any(|binding| shares(binding.kind)) {
            return Name::Local;
        }
        if redirected.is_some()
            || file
                .python_deletion_before(name, scope, range.start, work)
                .is_some()
        {
            return rebound(work);
        }
        // A parameter beside a declaration is a value of its own.
        let parameter = !class
            && held
                .iter()
                .any(|binding| !shares(binding.kind) && file.target_at(binding.name).is_none());
        if parameter || file.blocked(range.start, work) {
            return Name::Refused;
        }
        let Some(ran) = file.python_ran(held, scope, range.start, work) else {
            return Name::Refused;
        };
        if !class {
            // The declaration has not run where the variables of its name alone have.
            return if ran.is_empty() {
                Name::Refused
            } else if ran.iter().any(|binding| shares(binding.kind)) {
                self.python_one_declaration(file, &ran, work)
            } else {
                Name::Local
            };
        }
        // A class body reads the module's name until it binds its own.
        let module = self.python_module_name(file, name, range.start, work);
        if ran.is_empty() {
            return module;
        }
        match module {
            Name::Local => self.python_one_declaration(file, &ran, work),
            // Nothing tells whether the class bound its own under a condition.
            Name::Refused | Name::Declared(_) | Name::External(_) => Name::Refused,
        }
    }

    /// The declaration a read at `position` of a name of the module names.
    fn python_module_name(
        &self,
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> Name<'source> {
        let module = file.python_module_bindings(name);
        let global = file.python_globals.get(name).into_iter().flatten();
        let mut reached = global.copied().collect::<Vec<_>>();
        if module.is_empty() && reached.is_empty() {
            return Name::Local;
        }
        if file.blocked(position, work) {
            return Name::Refused;
        }
        if let Some(first) = module.first() {
            let scope = Scope::of(file, first);
            let Some(ran) = file.python_ran(module, scope, position, work) else {
                return Name::Refused;
            };
            reached.splice(..0, ran);
        }
        if reached.is_empty() {
            return Name::Refused;
        }
        self.python_one_declaration(file, &reached, work)
    }

    fn python_one_declaration(
        &self,
        file: &File<'source>,
        bindings: &[&'source StaticBinding],
        work: &mut Work,
    ) -> Name<'source> {
        self.python_agreed_name(file, bindings, &mut ImportedNames::new(), work)
    }

    /// The one declaration `bindings` name. Bindings of one scope bind one name, so the ones
    /// that import nothing name the declaration of that name.
    fn python_agreed_name(
        &self,
        file: &File<'source>,
        bindings: &[&'source StaticBinding],
        followed: &mut ImportedNames<'source>,
        work: &mut Work,
    ) -> Name<'source> {
        let mut agreed = Name::Local;
        let mut imported = false;
        let mut undeclared = false;
        for binding in bindings {
            if !work.charge() {
                return Name::Refused;
            }
            let import = imports(binding);
            imported |= import;
            let named = if import {
                work.python_lookup(|work| {
                    Some(self.python_imported_name(file, binding, followed, work))
                })
                .unwrap_or(Name::Refused)
            } else if let Some(target) = file.target_at(binding.name) {
                Name::Declared(target)
            } else {
                undeclared = true;
                continue;
            };
            if matches!(named, Name::Refused)
                || !(matches!(agreed, Name::Local) || agreed.agrees(&named))
            {
                return Name::Refused;
            }
            agreed = named;
        }
        if imported && undeclared {
            return Name::Refused;
        }
        agreed
    }

    /// What one import binds: the declaration its chain ends at.
    fn python_imported_name(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        followed: &mut ImportedNames<'source>,
        work: &mut Work,
    ) -> Name<'source> {
        let local = match binding.kind {
            StaticBindingKind::PythonImport {
                module,
                export: Some(export),
            } => (|| {
                let module = EdgePythonModules::name(file, file.text(module)?)?;
                let exported = file.text(export)?;
                match self.edge_python.path(&module) {
                    // A package importing a name from itself binds its submodule.
                    Some(path)
                        if !self
                            .edge_python
                            .same_module(path, file.document.path.as_str())
                            || Some(exported) != file.text(binding.name) =>
                    {
                        let selected =
                            self.python_export_file(self.files.get(path)?, exported, work)?;
                        Some(self.python_exported_name(
                            (selected.document.path.as_str(), exported),
                            followed,
                            work,
                        ))
                    }
                    _ => self.python_module_target(&format!("{module}.{exported}")),
                }
            })(),
            StaticBindingKind::PythonImport {
                module,
                export: None,
            } => file
                .text(module)
                .and_then(|module| EdgePythonModules::name(file, module))
                .and_then(|module| self.python_module_target(&module)),
            // Importing a dotted path binds its first component.
            StaticBindingKind::PythonModule { module } => file
                .text(module)
                .and_then(|module| self.python_module_target(module.split('.').next()?)),
            _ => None,
        };
        local.unwrap_or_else(|| {
            self.external_import(file, binding, work)
                .map_or(Name::Refused, Name::External)
        })
    }

    fn python_module_target(&self, module: &str) -> Option<Name<'source>> {
        let target = self.value_target(self.module_value(module)?)?;
        Some(Name::Declared(target))
    }

    /// What every binding of `name` in a module of the selection names.
    fn python_exported_name(
        &self,
        mut name: ModuleName<'source>,
        followed: &mut ImportedNames<'source>,
        work: &mut Work,
    ) -> Name<'source> {
        match self.python_imports.0.get(&name) {
            Some(Some(last)) => name = *last,
            Some(None) => return Name::Refused,
            None => {}
        }
        if let Some(known) = followed.get(&name) {
            return known.clone();
        }
        followed.insert(name, Name::Refused);
        let named = self
            .python_module_export(name, followed, work)
            .unwrap_or(Name::Refused);
        followed.insert(name, named.clone());
        named
    }

    fn python_module_export(
        &self,
        (path, name): ModuleName<'source>,
        followed: &mut ImportedNames<'source>,
        work: &mut Work,
    ) -> Option<Name<'source>> {
        let file = self.files.get(path)?;
        let bindings = file.python_held_bindings(name);
        if bindings.is_empty() {
            let module = file.qualified_target(".")?.admitted_path()?.join(".");
            return self.python_module_target(&format!("{module}.{name}"));
        }
        let imported = bindings.iter().copied().any(imports);
        if imported && !self.python_module_slot_available(file, name, work) {
            return None;
        }
        match self.python_agreed_name(file, &bindings, followed, work) {
            Name::Local => None,
            named => Some(named),
        }
    }

    /// An access of a declaration that a dependency or the standard library owns, from the
    /// same place as [`Self::python_access`].
    fn python_external_access(
        &self,
        file: &File<'source>,
        range: ByteRange,
        target: rift_core::SymbolId,
    ) -> Option<ExternalOccurrence> {
        let mut external =
            Self::external_occurrence(file, range, range, ReferenceRole::Read, target)?;
        external.edge_from = file
            .enclosing_callable(range)
            .and_then(|caller| self.python_identity(file, range, caller));
        Some(external)
    }
}

fn imports(binding: &StaticBinding) -> bool {
    matches!(
        binding.kind,
        StaticBindingKind::PythonImport { .. } | StaticBindingKind::PythonModule { .. }
    )
}

/// Whether a binding of a function scope declares or imports what it names.
fn shares(kind: StaticBindingKind) -> bool {
    matches!(
        kind,
        StaticBindingKind::Local
            | StaticBindingKind::PythonFunction { .. }
            | StaticBindingKind::PythonImport { .. }
            | StaticBindingKind::PythonModule { .. }
    )
}

impl<'source> File<'source> {
    /// The one authored local declaration available to this read in its own function.
    fn python_local_target(
        &self,
        read: ByteRange,
        name: ByteRange,
        target: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let caller = self.enclosing_callable(read)?;
        let body = caller.symbol.body_range?;
        let item = target.symbol.item_range;
        let in_scope = if target.symbol.is_python_parameter() {
            caller.symbol.item_range.encloses(item) && item.end <= body.start
        } else {
            target.symbol.kind == "variable" && body.encloses(item)
        };
        if !in_scope
            || target.symbol.container.as_deref() != Some(caller.symbol.qualified_name.as_str())
            || !body.encloses(read)
            || item.end > read.start
            || self.in_python_annotation(read.start)
            || self
                .python_expressions
                .resolve(read.start, read.end)
                .is_some()
            || self.blocked(read.start, work)
        {
            return None;
        }
        let spelling = self.text(read)?;
        let bindings = self
            .bindings_around(spelling, read)
            .filter(|binding| {
                !matches!(
                    binding.kind,
                    StaticBindingKind::PythonGuard { .. }
                        | StaticBindingKind::PythonMemberWrite { .. }
                )
            })
            .collect::<Vec<_>>();
        let ScopeLookup::Found(selected) = self.python_scope(&bindings, read.start, work) else {
            return None;
        };
        let scope = selected.scope;
        if scope.module || scope.range != caller.symbol.item_range || selected.redirected.is_some()
        {
            return None;
        }
        let mut local = bindings
            .into_iter()
            .filter(|binding| Scope::of(self, binding) == scope);
        let binding = local.next()?;
        if binding.name != name || binding.available_from > read.start || local.next().is_some() {
            return None;
        }
        let mut deletions = self
            .python_deletions
            .get(&(spelling, scope.range))
            .into_iter()
            .flatten();
        if deletions.any(|deletion| !work.charge() || deletion.name.end <= read.start) {
            return None;
        }
        Some(target)
    }

    /// The bindings of a name in module statements.
    fn python_module_bindings(&self, name: &str) -> Vec<&'source StaticBinding> {
        let bindings = self.by_name.get(name).into_iter().flatten().copied();
        bindings
            .filter(|binding| {
                self.python_module_binding(binding)
                    && !matches!(binding.kind, StaticBindingKind::PythonGuard { .. })
            })
            .collect()
    }

    /// The bindings of a name in module statements, then under `global`.
    fn python_held_bindings(&self, name: &str) -> Vec<&'source StaticBinding> {
        let mut bindings = self.python_module_bindings(name);
        let global = self.python_globals.get(name).into_iter().flatten();
        bindings.extend(global.copied());
        bindings
    }

    /// The bindings of `scope` that can have run before a read at `position`.
    fn python_ran(
        &self,
        bindings: Vec<&'source StaticBinding>,
        scope: Scope,
        position: u64,
        work: &mut Work,
    ) -> Option<Vec<&'source StaticBinding>> {
        if !self.python_ordered(scope, position) {
            return Some(bindings);
        }
        let mut ran = Vec::new();
        for binding in bindings {
            if !work.charge() {
                return None;
            }
            let declared = self.target_at(binding.name);
            let end = declared.map_or(0, |target| target.symbol.item_range.end);
            let end = end.max(binding.available_from).max(binding.name.end);
            if end <= position || self.python_repeated(binding.name.start, scope, position) {
                ran.push(binding);
            }
        }
        Some(ran)
    }

    /// Whether the code at `position` runs in the order of the statements of `scope`: no
    /// function, lambda, comprehension or generator expression of the scope holds it, and no
    /// annotation, which can be evaluated later.
    fn python_ordered(&self, scope: Scope, position: u64) -> bool {
        let end = position.saturating_add(1);
        let inside = |range: ByteRange| {
            scope.module || (scope.range != range && scope.range.encloses(range))
        };
        let runs = |callable: &&SyntaxSymbol| {
            inside(callable.item_range)
                && callable
                    .body_range
                    .is_some_and(|body| body.contains(position))
        };
        let deferred = match self.callables.containing(position, end) {
            Some(mut around) => around.any(runs),
            None => self.callables.resolve(position, end).is_some_and(runs),
        };
        let mut expressions = self.python_expressions.containing(position, end);
        !deferred
            && !self.in_python_annotation(position)
            && !expressions.iter_mut().flatten().any(|expression| {
                inside(*expression) && !self.python_evaluated_outside(*expression, position)
            })
    }

    /// Whether a loop of `scope` holds both positions.
    fn python_repeated(&self, bound: u64, scope: Scope, position: u64) -> bool {
        let end = position.saturating_add(1);
        let mut loops = self.python_loops.containing(position, end);
        loops
            .iter_mut()
            .flatten()
            .any(|range| range.contains(bound) && (scope.module || scope.range.encloses(*range)))
    }
}
