//! Python names read and written as values.

use rift_core::ReferenceRole;
use rift_protocol::read::EdgeKind;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, SyntaxSymbol};

use super::super::{File, Index, StaticResolution, Target, Work};
use super::{Scope, ScopeLookup};

/// What one read or written name names.
enum Name<'source> {
    /// A name without a declaration that another scope can read: a local, a parameter, a
    /// comprehension name, a builtin.
    Local,
    /// A name of a module, a class or an import whose bindings name no one declaration.
    Refused,
    Declared(Target<'source>),
}

impl<'source> Index<'source> {
    /// Records one read of a plain name.
    pub(super) fn python_name_read(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
        result: &mut StaticResolution,
    ) {
        let occurrence = match Self::python_shared_name(file, range, work) {
            Name::Local => return,
            Name::Refused => None,
            Name::Declared(target) => self.python_access(file, range, ReferenceRole::Read, target),
        };
        result.record_kind(occurrence, EdgeKind::Reads);
    }

    /// The declaration every binding of the read scope names.
    fn python_shared_name(
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
        let scope = match file.python_scope(&bindings, range.start, work) {
            ScopeLookup::Found(selected) => Some(selected.scope),
            ScopeLookup::Missing => None,
            ScopeLookup::Blocked => return Name::Refused,
        };
        // A `global` statement can bind a name that no module statement binds.
        let Some(scope) = scope.filter(|scope| !scope.module) else {
            return Self::python_module_name(file, name, range.start, work);
        };
        let held = bindings
            .into_iter()
            .filter(|binding| Scope::of(file, binding) == scope)
            .collect::<Vec<_>>();
        let class = file.class_scopes.contains(&scope.range);
        if !class && !held.iter().any(|binding| shares(binding.kind)) {
            return Name::Local;
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
                Self::python_one_declaration(file, &ran, work)
            } else {
                Name::Local
            };
        }
        // A class body reads the module's name until it binds its own.
        let module = Self::python_module_name(file, name, range.start, work);
        if ran.is_empty() {
            return module;
        }
        match module {
            Name::Local => Self::python_one_declaration(file, &ran, work),
            // Nothing tells whether the class bound its own under a condition.
            Name::Refused | Name::Declared(_) => Name::Refused,
        }
    }

    /// The declaration a read at `position` of a name of the module names.
    fn python_module_name(
        file: &File<'source>,
        name: &str,
        position: u64,
        work: &mut Work,
    ) -> Name<'source> {
        let bindings = file.by_name.get(name).into_iter().flatten().copied();
        let module = bindings
            .filter(|binding| {
                file.python_module_binding(binding)
                    && !matches!(binding.kind, StaticBindingKind::PythonGuard { .. })
            })
            .collect::<Vec<_>>();
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
        Self::python_one_declaration(file, &reached, work)
    }

    /// The one declaration `bindings` name. Bindings of one scope bind one name, so the ones
    /// that import nothing name the declaration of that name.
    fn python_one_declaration(
        file: &File<'source>,
        bindings: &[&'source StaticBinding],
        work: &mut Work,
    ) -> Name<'source> {
        let mut declared: Option<Target<'source>> = None;
        for binding in bindings {
            if !work.charge() {
                return Name::Refused;
            }
            if matches!(
                binding.kind,
                StaticBindingKind::PythonImport { .. } | StaticBindingKind::PythonModule { .. }
            ) {
                return Name::Local;
            }
            let Some(target) = file.target_at(binding.name) else {
                continue;
            };
            if declared
                .is_some_and(|known| known.symbol.qualified_name != target.symbol.qualified_name)
            {
                return Name::Refused;
            }
            declared.get_or_insert(target);
        }
        declared.map_or(Name::Local, Name::Declared)
    }
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
