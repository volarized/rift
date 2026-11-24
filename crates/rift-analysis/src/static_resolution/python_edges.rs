//! Resolves Python names through captured declarations, imports, and receivers.

mod annotations;
mod classes;
mod elements;
mod overloads;
mod rebinding;
mod receiver_guards;
mod references;
mod slots;

pub(super) use classes::ClassOrders;

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{File, Index, StaticOccurrence, Target, Work};
use crate::analyzer::namespace::python::python_import_target;

pub(super) struct EdgePythonModules(BTreeMap<String, Option<String>>);

impl EdgePythonModules {
    pub(super) fn build_edge(files: &BTreeMap<&str, File<'_>>, work: &mut Work) -> Option<Self> {
        let mut modules = BTreeMap::new();
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
            let path = file.document.path.as_str();
            modules
                .entry(module.join("."))
                .and_modify(|entry| *entry = None)
                .or_insert_with(|| Some(path.to_owned()));
        }
        Some(Self(modules))
    }

    fn path(&self, name: &str) -> Option<&str> {
        self.0.get(name)?.as_deref()
    }

    fn name(from: &File<'_>, module: &str) -> Option<String> {
        let anchor = from.qualified_target(".")?.admitted_path()?;
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
        for binding in file.facts.bindings() {
            if !work.charge() {
                return None;
            }
            if binding.scope != caller.symbol.item_range {
                continue;
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
                return self.python_base_member(class, file.text(member)?, work);
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
        let mut prefix = None;
        for call in file.python_calls.get(&range.start).into_iter().flatten() {
            if !work.charge() {
                return None;
            }
            if call.range.end <= range.end
                && prefix.is_none_or(|previous: &StaticCall| previous.range.end < call.range.end)
            {
                prefix = Some(*call);
            }
        }
        if let Some(call) = prefix {
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
        if terminal.file.path == written.document.path || !work.charge() {
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
        match binding.kind {
            StaticBindingKind::Local
            | StaticBindingKind::Shadow
            | StaticBindingKind::PythonNone
            | StaticBindingKind::PythonInstance { .. }
            | StaticBindingKind::PythonAlias { .. }
            | StaticBindingKind::PythonFunction { .. } => {
                file.target_at(binding.name).map(Value::Symbol)
            }
            StaticBindingKind::PythonElement { iterable } => {
                file.target_at(binding.name).map(Value::Symbol).or_else(|| {
                    self.python_element(file, iterable, work)
                        .map(Value::Instance)
                })
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

    fn module_value(&self, name: &str) -> Option<Value<'source>> {
        let prefix = format!("{name}.");
        (self.edge_python.path(name).is_some()
            || self
                .edge_python
                .0
                .range(prefix.clone()..)
                .next()
                .is_some_and(|(module, _)| module.starts_with(&prefix)))
        .then(|| Value::Module(name.to_owned()))
    }

    fn export_value<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut name: &'index str,
        work: &mut Work,
    ) -> Option<Value<'source>> {
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
                    if path == file.document.path.as_str() && exported == name {
                        // A package importing a name from itself binds its submodule.
                        return self.module_value(&format!("{module}.{exported}"));
                    }
                    name = exported;
                    file = self.files.get(path)?;
                }
                StaticBindingKind::PythonModule { module } => {
                    return self.module_value(file.text(module)?.split('.').next()?);
                }
                _ => return None,
            }
        }
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
    if file.blocked(position, work) {
        return None;
    }
    let callable = file.enclosing_callable(ByteRange {
        start: position,
        end: position.saturating_add(1),
    });
    let in_body = callable.is_some_and(|callable| {
        callable
            .symbol
            .body_range
            .is_some_and(|body| body.contains(position))
    });
    let bindings = file.by_name.get(name)?;
    let mut excluded = BTreeSet::new();
    let mut global = false;
    let mut nonlocal = false;
    loop {
        let mut best: Option<Scope> = None;
        for &binding in bindings {
            if !work.charge() {
                return None;
            }
            let scope = Scope::of(file, binding);
            if !scope.range.contains(position)
                || excluded.contains(&scope)
                || (global && !scope.module)
                || (nonlocal && scope.module)
                || (in_body && !scope.module && file.class_scopes.contains(&scope.range))
            {
                continue;
            }
            if best.is_none_or(|best| best.holds(scope)) {
                best = Some(scope);
            }
        }
        let scope = best?;
        let mut declaration = None;
        let mut selected = false;
        for &binding in bindings {
            if !work.charge() {
                return None;
            }
            if Scope::of(file, binding) != scope {
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
        if let Some(is_global) = declaration {
            if selected || global || (nonlocal && is_global) {
                return None;
            }
            excluded.insert(scope);
            global = is_global;
            nonlocal = !is_global;
        } else {
            let later = later_bindings(file, bindings, scope, position, callable, in_body, work)?;
            let candidates = bindings.iter().copied().filter(|binding| {
                Scope::of(file, binding) == scope && !later.contains(&binding.name.start)
            });
            let selected = match index.python_binding_group(file, candidates.clone(), work) {
                Some(selected) => selected,
                None => index
                    .python_receiver_join(file, scope, candidates.clone(), work)
                    .or_else(|| {
                        index.python_guarded_receiver(file, name, scope, position, candidates, work)
                    })?,
            };
            if selected.available_from > position {
                return None;
            }
            return file
                .python_guard_available(selected, position, work)
                .then_some(selected);
        }
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
    callable: Option<Target<'_>>,
    in_body: bool,
    work: &mut Work,
) -> Option<BTreeSet<u64>> {
    let mut later = BTreeSet::new();
    let ordered = ((in_body
        && !scope.module
        && callable.is_some_and(|callable| callable.symbol.item_range == scope.range))
        || (scope.module && !in_body && !file.in_python_annotation(position)))
        && !file
            .facts
            .expression_scopes()
            .iter()
            .any(|range| range.contains(position));
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
