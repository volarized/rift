//! Captured Python class order and member lookup.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use rift_core::LoopBudget;
use rift_protocol::read::SymbolKind;

use super::super::{Binding, File, FileWork, Index, Target, Work};
use super::{Value, edge_python_binding};
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, SyntaxSymbol};

/// Classes the Python `builtins` module provides, which a module may name without binding.
const BUILTIN_CLASSES: [&str; 96] = [
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "EncodingWarning",
    "EnvironmentError",
    "Exception",
    "ExceptionGroup",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "NotADirectoryError",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "PythonFinalizationError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "ZeroDivisionError",
    "bool",
    "bytearray",
    "bytes",
    "classmethod",
    "complex",
    "dict",
    "enumerate",
    "filter",
    "float",
    "frozenset",
    "int",
    "list",
    "map",
    "memoryview",
    "object",
    "property",
    "range",
    "reversed",
    "set",
    "slice",
    "staticmethod",
    "str",
    "super",
    "tuple",
    "type",
    "zip",
];

/// Modules that provide the typing names.
pub(super) const TYPING_MODULES: [&str; 2] = ["typing", "typing_extensions"];

fn typing_name(qualified: &str, expected: &str) -> bool {
    qualified
        .rsplit_once('.')
        .is_some_and(|(module, name)| name == expected && TYPING_MODULES.contains(&module))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ClassKey<'source> {
    Declaration(&'source str, &'source str),
    Object,
    Generic,
    /// A base declared outside the selected sources, named by its written base expression.
    /// Its own ancestors are outside the selected sources too.
    External(&'source str, u64),
}

impl ClassKey<'_> {
    fn blocks_member(self, name: &str) -> bool {
        match self {
            // Generic's private and special members have no declaration in the selected sources.
            Self::Generic => name.starts_with('_'),
            // Any member may be declared by a class outside the selected sources.
            Self::External(..) => true,
            Self::Object | Self::Declaration(..) => false,
        }
    }
}

/// The order of every class of the selected sources, and whether the bound on reading them
/// ended before the last one.
#[derive(Default)]
pub(in crate::static_resolution) struct ClassOrders<'source> {
    orders: Orders<'source>,
    pub(in crate::static_resolution) ended: bool,
}

type Orders<'source> = BTreeMap<ClassKey<'source>, ClassOrder<'source>>;

struct ClassOrder<'source> {
    classes: OrderedClasses<'source>,
    complete: bool,
}

/// The classes of one order, the class itself first.
enum OrderedClasses<'source> {
    /// No order satisfies the bases of this class or one of its ancestors.
    Invalid,
    /// The class, then the order of its only base.
    Inherited(ClassKey<'source>),
    Merged(Vec<ClassKey<'source>>),
}

impl<'source> ClassOrder<'source> {
    fn inherited(base: ClassKey<'source>, complete: bool) -> Self {
        Self {
            classes: OrderedClasses::Inherited(base),
            complete,
        }
    }

    fn merged(classes: Vec<ClassKey<'source>>, complete: bool) -> Self {
        Self {
            classes: OrderedClasses::Merged(classes),
            complete,
        }
    }

    fn partial(class: ClassKey<'source>) -> Self {
        Self::merged(vec![class], false)
    }

    fn invalid() -> Self {
        Self {
            classes: OrderedClasses::Invalid,
            complete: false,
        }
    }
}

/// The classes of the order of `class`. An order inherits only orders established before it,
/// so the walk ends.
fn ordered<'orders, 'source>(
    orders: &'orders Orders<'source>,
    class: ClassKey<'source>,
) -> impl Iterator<Item = ClassKey<'source>> + 'orders {
    let mut inherited = Some(class);
    let mut merged: &[ClassKey<'source>] = &[];
    std::iter::from_fn(move || {
        if let Some((class, rest)) = merged.split_first() {
            merged = rest;
            return Some(*class);
        }
        let class = inherited.take()?;
        match &orders.get(&class)?.classes {
            OrderedClasses::Invalid => Some(class),
            OrderedClasses::Inherited(base) => {
                inherited = Some(*base);
                Some(class)
            }
            OrderedClasses::Merged(classes) => {
                let (class, rest) = classes.split_first()?;
                merged = rest;
                Some(*class)
            }
        }
    })
}

/// The classes that declare a member, by the member's name.
#[derive(Default)]
pub(in crate::static_resolution) struct MemberOwners<'source>(
    BTreeMap<&'source str, BTreeSet<ClassKey<'source>>>,
);

impl<'source> MemberOwners<'source> {
    /// The classes other than `class` that declare `name`.
    fn others(&self, class: ClassKey<'source>, name: &str) -> Option<&BTreeSet<ClassKey<'source>>> {
        let owners = self.0.get(name)?;
        owners.iter().any(|owner| *owner != class).then_some(owners)
    }
}

fn key(target: Target<'_>) -> ClassKey<'_> {
    ClassKey::Declaration(
        target.file.path.as_str(),
        target.symbol.qualified_name.as_str(),
    )
}

/// Where one binding of a name outside the selected sources leads.
enum ExternalBinding<'index, 'source> {
    /// A module outside the selected sources.
    Outside,
    /// The name a module of the selected sources re-exports.
    Reexport(&'index File<'source>, &'index str),
}

impl<'source> Index<'source> {
    /// Reads which classes of the Python files declare each member name.
    pub(in crate::static_resolution) fn python_member_owners(
        &self,
        work: &mut Work,
    ) -> Option<MemberOwners<'source>> {
        let mut owners = MemberOwners::default();
        for file in self.python_files() {
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return None;
                }
                if let Some(container) = symbol.container.as_deref() {
                    let class = ClassKey::Declaration(file.document.path.as_str(), container);
                    owners.0.entry(&symbol.name).or_default().insert(class);
                }
            }
        }
        Some(owners)
    }

    /// Reads the order of every Python class, under the shares of the files that declare
    /// them. A class past that bound has no order, and the others keep theirs.
    pub(in crate::static_resolution) fn python_class_orders(
        &self,
        shares: FileWork,
        work: &mut Work,
    ) -> ClassOrders<'source> {
        let bound = self
            .python_files()
            .map(|file| shares.max(file))
            .fold(0, usize::saturating_add)
            .min(work.budget.remaining());
        let mut apart = Work::new(bound);
        let mut orders = BTreeMap::from([
            (
                ClassKey::Object,
                ClassOrder::merged(vec![ClassKey::Object], true),
            ),
            (
                ClassKey::Generic,
                ClassOrder::inherited(ClassKey::Object, true),
            ),
        ]);
        let bases = self.python_bases(&mut apart);
        order_classes(bases, &mut orders, &mut apart);
        let used = bound.saturating_sub(apart.budget.remaining());
        work.budget = LoopBudget::new(work.budget.remaining().saturating_sub(used));
        ClassOrders {
            orders,
            ended: apart.exhausted,
        }
    }

    pub(super) fn python_files(&self) -> impl Iterator<Item = &File<'source>> {
        self.files
            .values()
            .filter(|file| file.document.facts.language().name == "python")
    }

    /// The bases each class of the selected sources names, up to the bound.
    fn python_bases(&self, work: &mut Work) -> Bases<'source> {
        let mut pending = BTreeMap::new();
        for file in self.python_files() {
            let mut heritage = BTreeMap::<ByteRange, Vec<ByteRange>>::new();
            for edge in file.facts.heritage_edges() {
                if !work.charge() {
                    return pending;
                }
                heritage.entry(edge.from).or_default().push(edge.target);
            }
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return pending;
                }
                if !symbol.kinds.contains(&SymbolKind::Constructible) {
                    continue;
                }
                let target = Target {
                    file: file.document,
                    symbol,
                };
                let written = symbol.name_range.and_then(|name| heritage.get(&name));
                let Some(bases) = self.python_class_bases(file, written, work) else {
                    return pending;
                };
                pending.insert(key(target), bases);
            }
        }
        pending
    }

    fn python_class_bases(
        &self,
        file: &File<'source>,
        written: Option<&Vec<ByteRange>>,
        work: &mut Work,
    ) -> Option<Vec<Option<ClassKey<'source>>>> {
        let mut bases = Vec::new();
        for &range in written.into_iter().flatten() {
            if !work.charge() {
                return None;
            }
            let library = self.python_library_name(file, range, work);
            let base = if library
                .as_deref()
                .is_some_and(|name| typing_name(name, "Generic"))
            {
                Some(ClassKey::Generic)
            } else if let Some(base) = self.edge_python_base(file, range, work) {
                Some(key(base))
            } else {
                self.python_external_base(file, range, library.as_deref(), work)
            };
            bases.push(base);
        }
        (!work.exhausted).then_some(bases)
    }

    /// The class an external base expression names. The builtin `object` has one shared key.
    fn python_external_base(
        &self,
        file: &File<'source>,
        range: ByteRange,
        library: Option<&str>,
        work: &mut Work,
    ) -> Option<ClassKey<'source>> {
        let text = file.text(range)?;
        // A subscripted or called base can supply other bases through `__mro_entries__`; the
        // captured base range holds only the name before its subscript or arguments.
        let applied = usize::try_from(range.end)
            .ok()
            .and_then(|end| file.document.source.get(end..))
            .is_none_or(|rest| rest.trim_start().starts_with(['[', '(']));
        let named = !applied
            && text.split('.').map(str::trim).all(|part| {
                !part.is_empty() && part.chars().all(|c| c == '_' || c.is_alphanumeric())
            });
        let mut parts = text.split('.').map(str::trim);
        let name = parts.next().filter(|_| named)?;
        if library == Some("builtins.object")
            || (text == "object"
                && matches!(
                    self.python_binding(file, name, range.start, work),
                    Binding::Missing
                ))
        {
            return Some(ClassKey::Object);
        }
        let dotted = parts.next().is_some();
        self.python_external_name(file, name, range.start, dotted, work)
            .then_some(ClassKey::External(file.document.path.as_str(), range.start))
    }

    /// Whether every binding the name reaches is one outside the selected sources. A name of
    /// one file is read once, and reaching it again establishes nothing.
    fn python_external_name<'index>(
        &'index self,
        file: &'index File<'source>,
        name: &'index str,
        position: u64,
        dotted: bool,
        work: &mut Work,
    ) -> bool {
        let mut visited = BTreeSet::new();
        let mut pending = vec![(file, name, position)];
        while let Some((file, name, position)) = pending.pop() {
            if !work.charge() || !visited.insert((file.document.path.as_str(), name)) {
                return false;
            }
            if file.blocked(position, work) {
                return false;
            }
            let Some(bindings) = file.by_name.get(name) else {
                // Python resolves a name no scope binds among the builtins.
                if dotted || !BUILTIN_CLASSES.contains(&name) {
                    return false;
                }
                continue;
            };
            if bindings.is_empty() {
                return false;
            }
            for binding in bindings {
                match self.python_external_binding(file, binding) {
                    Some(ExternalBinding::Outside) => {}
                    Some(ExternalBinding::Reexport(file, name)) => pending.push((file, name, 0)),
                    None => return false,
                }
            }
        }
        true
    }

    /// Where one binding of a name outside the selected sources leads. `None` for any other
    /// binding.
    fn python_external_binding<'index>(
        &'index self,
        file: &'index File<'source>,
        binding: &StaticBinding,
    ) -> Option<ExternalBinding<'index, 'source>> {
        let outside = |module: &str| {
            let absent = self.module_value(module).is_none();
            absent.then_some(ExternalBinding::Outside)
        };
        match binding.kind {
            StaticBindingKind::PythonModule { module } => {
                outside(file.text(module)?.split('.').next()?)
            }
            StaticBindingKind::PythonImport { module, export } => {
                let module = super::EdgePythonModules::name(file, file.text(module)?)?;
                let Some(export) = export.and_then(|export| file.text(export)) else {
                    return outside(&module);
                };
                let Some(path) = self.edge_python.path(&module) else {
                    outside(&module)?;
                    return outside(&format!("{module}.{export}"));
                };
                // A module of the selected sources re-exports a binding it holds.
                let source = self.files.get(path)?;
                source
                    .by_name
                    .contains_key(export)
                    .then_some(ExternalBinding::Reexport(source, export))
            }
            _ => None,
        }
    }

    /// Whether the expression at `range` names `expected` of `typing` or `typing_extensions`,
    /// imported by name or read through the module.
    pub(super) fn python_typing_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        expected: &str,
        work: &mut Work,
    ) -> bool {
        self.python_library_name(file, range, work)
            .is_some_and(|qualified| typing_name(&qualified, expected))
    }

    /// The module path and name the dotted expression at `range` reads from a module outside
    /// the selected sources, imported by name or read through the module.
    pub(super) fn python_library_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<String> {
        let text = file.text(range)?;
        let mut names = text.split('.').map(str::trim);
        let name = names.next()?;
        let binding = edge_python_binding(self, file, name, range.start, work)?;
        let mut qualified = match binding.kind {
            StaticBindingKind::PythonImport { module, export } => {
                let mut path = vec![file.text(module)?];
                if let Some(export) = export {
                    path.push(file.text(export)?);
                }
                path
            }
            // Importing a dotted path binds its first component.
            StaticBindingKind::PythonModule { .. } => vec![name],
            _ => return None,
        };
        qualified.extend(names);
        let qualified = qualified.join(".");
        let (module, _) = qualified.rsplit_once('.')?;
        let local = std::iter::successors(Some(module), |module| {
            module.rsplit_once('.').map(|(parent, _)| parent)
        })
        .any(|module| self.edge_python.0.contains_key(module));
        (!local && !qualified.starts_with('.')).then_some(qualified)
    }

    pub(super) fn python_member_value(
        &self,
        receiver: Value<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let receiver = self.python_receiver(receiver, work)?;
        let instance = matches!(receiver, Value::Instance(_));
        let class = match receiver {
            Value::Symbol(class) | Value::Instance(class) => class,
            Value::Member { .. } | Value::Module(_) => return None,
        };
        if class.symbol.kinds.contains(&SymbolKind::Module) {
            let file = self.files.get(class.file.path.as_str())?;
            return self.export_value(file, name, work);
        }
        let member = self
            .python_own_member(class, name, work)
            .or_else(|| self.python_base_member(class, name, work));
        let assigned = member.is_some_and(|target| self.python_assigned_member(target));
        if member.is_some() && !assigned {
            if !self.python_member_slot_available(class, instance, name, member?, work) {
                return None;
            }
            if instance
                && let Some(declaration) = member
                && let Some(assigned) = self.python_property_result(declaration, work)
            {
                return Some(Value::Member {
                    declaration,
                    assigned,
                });
            }
            return member.map(Value::Symbol);
        }
        let declaration = self
            .python_declared_base_member(class, name, work)
            .or(member)?;
        if !self.python_member_slot_available(class, instance, name, declaration, work) {
            return None;
        }
        match member.filter(|own| key(*own) != key(declaration)) {
            Some(assigned) => Some(Value::Member {
                declaration,
                assigned,
            }),
            None => Some(Value::Symbol(declaration)),
        }
    }

    fn python_assigned_member(&self, target: Target<'source>) -> bool {
        if target.symbol.kinds.contains(&SymbolKind::Callable) {
            return false;
        }
        let Some(file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        let Some(range) = target.symbol.name_range else {
            return false;
        };
        let Some(class) = target
            .symbol
            .container
            .as_deref()
            .and_then(|name| file.qualified_target(name))
            .filter(|container| container.symbol.kinds.contains(&SymbolKind::Constructible))
        else {
            return false;
        };
        file.enclosing_callable(range)
            .is_some_and(|caller| class.symbol.item_range.encloses(caller.symbol.item_range))
    }

    pub(super) fn python_slot_relation(
        &self,
        class: Target<'source>,
        written: Target<'source>,
        declaration: Target<'source>,
        instance: bool,
        work: &mut Work,
    ) -> Option<bool> {
        let complete = self.edge_python_classes.orders.get(&key(class))?.complete;
        let owner = declaration.symbol.container.as_deref()?;
        for ancestor in ordered(&self.edge_python_classes.orders, key(class)) {
            if !work.charge() {
                return None;
            }
            if ancestor == key(written) {
                return Some(true);
            }
            if !instance && ancestor == ClassKey::Declaration(declaration.file.path.as_str(), owner)
            {
                return Some(false);
            }
        }
        complete.then_some(false)
    }

    /// The member the class body of the nearest ancestor declares. An attribute a method
    /// assigns through its receiver belongs to the instance and declares none.
    pub(super) fn python_declared_base_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let owners = self.python_members.others(key(class), name)?;
        for ancestor in ordered(&self.edge_python_classes.orders, key(class)).skip(1) {
            if !work.charge() {
                return None;
            }
            if ancestor.blocks_member(name) {
                return None;
            }
            if !owners.contains(&ancestor) {
                continue;
            }
            let ClassKey::Declaration(path, qualified) = ancestor else {
                continue;
            };
            let base = self.files.get(path)?.qualified_target(qualified)?;
            let Some(target) = self.python_own_member(base, name, work) else {
                continue;
            };
            if !self.python_assigned_member(target) {
                return Some(target);
            }
            if let Some(method) = self.python_method_slot(base, name, work) {
                return Some(method);
            }
        }
        None
    }

    /// The callable a class body declares under a name its methods assign too.
    fn python_method_slot(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let file = self.files.get(class.file.path.as_str())?;
        let bindings = python_slot_bindings(file, class, name);
        let binding = self.python_binding_group(file, bindings, work)?;
        file.target_at(binding.name)?.callable()
    }

    /// The class or instance a value holds. An attribute that methods assign instances of
    /// different classes holds neither.
    pub(super) fn python_receiver(
        &self,
        value: Value<'source>,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let target = match value {
            Value::Symbol(target)
            | Value::Member {
                assigned: target, ..
            } => Some(target),
            Value::Instance(_) | Value::Module(_) => None,
        };
        let held = self.python_held_value(value, work)?;
        if let Value::Instance(class) = held
            && let Some(target) = target
            && self.python_assigned_member(target)
            && work.python_comparison(|work| self.python_assigned_otherwise(target, class, work))
        {
            return None;
        }
        Some(held)
    }

    /// Whether another assignment of an attribute without a written type holds an instance of
    /// a class other than `class`.
    fn python_assigned_otherwise(
        &self,
        target: Target<'source>,
        class: Target<'source>,
        work: &mut Work,
    ) -> bool {
        let Some(file) = self.files.get(target.file.path.as_str()) else {
            return false;
        };
        let facets = file
            .by_qualified_name
            .get(target.symbol.qualified_name.as_str())
            .into_iter()
            .flatten()
            .copied();
        let value_of = |symbol: &SyntaxSymbol| {
            symbol
                .name_range
                .and_then(|name| file.python_values.get(&name.end))
        };
        if facets.clone().any(|symbol| {
            matches!(
                value_of(symbol),
                Some(StaticBindingKind::PythonAnnotation { .. })
            )
        }) {
            return false;
        }
        facets
            .filter(|symbol| symbol.name_range != target.symbol.name_range)
            .any(|symbol| {
                if !work.charge() {
                    return true;
                }
                let facet = Target {
                    file: file.document,
                    symbol,
                };
                let other =
                    work.python_lookup(|work| self.python_held_value(Value::Symbol(facet), work));
                matches!(other, Some(Value::Instance(other)) if key(other) != key(class))
            })
    }

    fn python_held_value(
        &self,
        mut value: Value<'source>,
        work: &mut Work,
    ) -> Option<Value<'source>> {
        let mut visited = BTreeSet::new();
        loop {
            if !work.charge() {
                return None;
            }
            if let Value::Symbol(target) = value
                && let Some(class) = self.python_aliases.target(target)
            {
                return work.python_lookup(|_| Some(Value::Symbol(class)));
            }
            let target = match value {
                Value::Instance(_) => return Some(value),
                Value::Member { assigned, .. }
                    if assigned.symbol.kinds.contains(&SymbolKind::Constructible) =>
                {
                    return Some(Value::Instance(assigned));
                }
                Value::Symbol(target)
                | Value::Member {
                    assigned: target, ..
                } => target,
                Value::Module(_) => return self.value_target(value).map(Value::Symbol),
            };
            if !visited.insert(key(target)) {
                return None;
            }
            if target.symbol.kinds.contains(&SymbolKind::Constructible)
                || target.symbol.kinds.contains(&SymbolKind::Module)
            {
                return Some(Value::Symbol(target));
            }
            let file = self.files.get(target.file.path.as_str())?;
            value = match file.python_values.get(&target.symbol.name_range?.end)? {
                StaticBindingKind::PythonAnnotation { annotation } => {
                    match self.python_class_object_annotation(file, *annotation, work) {
                        Some(class) => return Some(Value::Symbol(class)),
                        None => Value::Instance(self.python_annotation(file, *annotation, work)?),
                    }
                }
                StaticBindingKind::PythonInstance { constructor } => {
                    let callable = self.edge_python_name(file, *constructor, work)?;
                    Value::Instance(self.python_call_result(callable, work)?)
                }
                StaticBindingKind::PythonAlias { value } => {
                    work.python_lookup(|work| self.python_name(file, *value, work))?
                }
                StaticBindingKind::PythonElement { iterable, position } => {
                    Value::Instance(self.python_element(file, *iterable, *position, work)?)
                }
                StaticBindingKind::PythonUnpacked { value, position } => {
                    Value::Instance(self.python_unpacked(file, *value, *position, work)?)
                }
                _ => return None,
            };
        }
    }

    /// The class object a value holds through unchanged aliases.
    ///
    /// A value that another value constructs, iterates, or unpacks holds an instance, so the
    /// values that construct each other need no lookup in turn.
    fn python_held_class(
        &self,
        mut target: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut visited = BTreeSet::new();
        loop {
            if target.symbol.kinds.contains(&SymbolKind::Constructible) {
                return Some(target);
            }
            if !work.charge() || !visited.insert(key(target)) {
                return None;
            }
            let file = self.files.get(target.file.path.as_str())?;
            if let Some(class) = self.python_aliases.target(target) {
                return work.python_lookup(|_| Some(class));
            }
            let value = match *file.python_values.get(&target.symbol.name_range?.end)? {
                StaticBindingKind::PythonAnnotation { annotation } => {
                    return self.python_class_object_annotation(file, annotation, work);
                }
                StaticBindingKind::PythonAlias { value } => {
                    work.python_lookup(|work| self.python_name(file, value, work))?
                }
                _ => return None,
            };
            target = match value {
                Value::Symbol(target) => target,
                // The value a class calls for an attribute is an instance of that class.
                Value::Member { assigned, .. }
                    if !assigned.symbol.kinds.contains(&SymbolKind::Constructible) =>
                {
                    assigned
                }
                Value::Member { .. } | Value::Instance(_) | Value::Module(_) => return None,
            };
        }
    }

    /// The class a `type[Class]` annotation names, when `type` is the builtin.
    fn python_class_object_annotation(
        &self,
        file: &File<'source>,
        annotation: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let written = file.text(annotation)?;
        let inner = written
            .trim()
            .strip_prefix("type[")?
            .strip_suffix(']')?
            .trim();
        if file.by_name.contains_key("type") || file.blocked(annotation.start, work) {
            return None;
        }
        let offset = inner.as_ptr() as usize - written.as_ptr() as usize;
        let start = annotation.start + u64::try_from(offset).ok()?;
        let range = ByteRange {
            start,
            end: start + u64::try_from(inner.len()).ok()?,
        };
        self.python_annotation(file, range, work)
    }

    fn python_property_result(
        &self,
        getter: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let (file, annotation) = self.python_property_annotation(getter, work)?;
        self.python_annotation(file, annotation, work)
    }

    /// The return annotation of a property whose class body holds no other definition of the
    /// name than its setter and deleter.
    pub(super) fn python_property_annotation(
        &self,
        getter: Target<'source>,
        work: &mut Work,
    ) -> Option<(&File<'source>, ByteRange)> {
        let file = self.files.get(getter.file.path.as_str())?;
        let range = getter.symbol.name_range?;
        let StaticBindingKind::PythonFunction {
            annotation,
            decorator: Some(decorator),
        } = *file.python_values.get(&range.end)?
        else {
            return None;
        };
        if file.text(decorator)? != "property" || file.blocked(decorator.start, work) {
            return None;
        }
        for binding in file.by_name.get("property").into_iter().flatten() {
            if !work.charge() || binding.scope.contains(decorator.start) {
                return None;
            }
        }
        let class = file.qualified_target(getter.symbol.container.as_deref()?)?;
        for binding in python_slot_bindings(file, class, &getter.symbol.name) {
            if !work.charge() {
                return None;
            }
            if binding.name != range && !python_property_accessor(file, getter, binding) {
                return None;
            }
        }
        Some((file, annotation))
    }

    pub(super) fn python_call_result(
        &self,
        callable: Target<'source>,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if callable.symbol.kinds.contains(&SymbolKind::Constructible) {
            return Some(callable);
        }
        if !callable.symbol.kinds.contains(&SymbolKind::Callable) {
            // A value that holds a class object constructs an instance of that class.
            return self.python_held_class(callable, work);
        }
        let file = self.files.get(callable.file.path.as_str())?;
        let StaticBindingKind::PythonFunction { annotation, .. } =
            file.python_values.get(&callable.symbol.name_range?.end)?
        else {
            return None;
        };
        self.python_annotation(file, *annotation, work)
    }

    pub(super) fn python_own_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let file = self.files.get(class.file.path.as_str())?;
        let target = file.qualified_target(&format!("{}.{name}", class.symbol.qualified_name))?;
        let bindings = python_slot_bindings(file, class, name);
        if !target.symbol.kinds.contains(&SymbolKind::Callable) {
            // An instance write through `self` before the property goes through its setter;
            // the class body declares only the property, so the slot is the property.
            return Some(python_property_slot(file, target, bindings, work).unwrap_or(target));
        }
        file.by_name.get(name)?;
        // Getter, setter and deleter facets of one property are one member slot.
        let mut property = true;
        for binding in bindings.clone() {
            if !work.charge() {
                return None;
            }
            property &= file.target_at(binding.name).is_some_and(|facet| {
                facet.symbol.kinds.contains(&SymbolKind::Property)
                    && facet.symbol.qualified_name == target.symbol.qualified_name
            });
        }
        if property {
            return Some(target);
        }
        let binding = self.python_binding_group(file, bindings, work)?;
        file.target_at(binding.name)
    }

    pub(super) fn python_base_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let owners = self.python_members.others(key(class), name)?;
        for ancestor in ordered(&self.edge_python_classes.orders, key(class)).skip(1) {
            if !work.charge() {
                return None;
            }
            if ancestor.blocks_member(name) {
                return None;
            }
            if !owners.contains(&ancestor) {
                continue;
            }
            let ClassKey::Declaration(path, qualified) = ancestor else {
                continue;
            };
            let base = self.files.get(path)?.qualified_target(qualified)?;
            if let Some(target) = self.python_own_member(base, name, work) {
                return Some(target);
            }
        }
        None
    }
}

/// The bindings of a class body that give `name` a value. An annotation without a value
/// declares a type alone.
fn python_slot_bindings<'index, 'source>(
    file: &'index File<'source>,
    class: Target<'source>,
    name: &str,
) -> impl Iterator<Item = &'source StaticBinding> + Clone + use<'index, 'source> {
    file.bindings_around(name, class.symbol.item_range)
        .filter(move |binding| {
            binding.scope == class.symbol.item_range
                && !file.python_module_binding(binding)
                && match binding.kind {
                    StaticBindingKind::PythonAnnotation { annotation } => {
                        usize::try_from(annotation.end)
                            .ok()
                            .and_then(|end| file.document.source.get(end..))
                            .is_some_and(|rest| {
                                rest.trim_start_matches([' ', '\t', '\\', '\r', '\n'])
                                    .starts_with('=')
                            })
                    }
                    _ => true,
                }
        })
}

/// Whether `binding` is the setter or deleter that `getter` decorates.
fn python_property_accessor(file: &File<'_>, getter: Target<'_>, binding: &StaticBinding) -> bool {
    let Some(facet) = file.target_at(binding.name) else {
        return false;
    };
    let Some(decorators) = file.text(ByteRange {
        start: facet.symbol.range.start,
        end: binding.name.start,
    }) else {
        return false;
    };
    let name = getter.symbol.name.as_str();
    facet.symbol.kinds.contains(&SymbolKind::Property)
        && facet.symbol.qualified_name == getter.symbol.qualified_name
        && ["setter", "deleter"].iter().any(|accessor| {
            decorators
                .strip_prefix('@')
                .and_then(|rest| rest.strip_prefix(name))
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|rest| rest.strip_prefix(accessor))
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        })
}

/// The first property facet of `target` when every binding the class body holds for the
/// name is a facet of that property.
fn python_property_slot<'source>(
    file: &File<'source>,
    target: Target<'source>,
    bindings: impl Iterator<Item = &'source StaticBinding>,
    work: &mut Work,
) -> Option<Target<'source>> {
    let mut first = None;
    for binding in bindings {
        if !work.charge() {
            return None;
        }
        let facet = file.target_at(binding.name)?;
        if !facet.symbol.kinds.contains(&SymbolKind::Property)
            || facet.symbol.qualified_name != target.symbol.qualified_name
        {
            return None;
        }
        first.get_or_insert(facet);
    }
    first
}

type Bases<'source> = BTreeMap<ClassKey<'source>, Vec<Option<ClassKey<'source>>>>;

/// Orders every class after its bases, each class and base read once. A class whose bases
/// never get an order has a partial one.
fn order_classes<'source>(
    mut pending: Bases<'source>,
    orders: &mut Orders<'source>,
    work: &mut Work,
) {
    let mut waiting = BTreeMap::<ClassKey<'source>, usize>::new();
    let mut derived = BTreeMap::<ClassKey<'source>, Vec<ClassKey<'source>>>::new();
    let mut ready = VecDeque::new();
    for (class, bases) in &pending {
        let known = bases.iter().flatten().collect::<BTreeSet<_>>();
        let mut missing = 0;
        for base in known {
            if !work.charge() {
                return;
            }
            if let ClassKey::External(..) = base {
                orders.insert(*base, ClassOrder::inherited(ClassKey::Object, true));
            } else if !orders.contains_key(base) {
                missing += 1;
                derived.entry(*base).or_default().push(*class);
            }
        }
        if missing == 0 {
            ready.push_back(*class);
        } else {
            waiting.insert(*class, missing);
        }
    }
    while let Some(class) = ready.pop_front() {
        let Some(bases) = pending.remove(&class) else {
            continue;
        };
        let order = merge_order(class, &bases, orders, work);
        if work.exhausted {
            return;
        }
        orders.insert(class, order.unwrap_or_else(ClassOrder::invalid));
        for derived in derived.remove(&class).into_iter().flatten() {
            if !work.charge() {
                return;
            }
            if let Some(missing) = waiting.get_mut(&derived) {
                *missing -= 1;
                if *missing == 0 {
                    ready.push_back(derived);
                }
            }
        }
    }
    for class in pending.into_keys() {
        if !work.charge() {
            return;
        }
        orders.insert(class, ClassOrder::partial(class));
    }
}

fn merge_order<'source>(
    class: ClassKey<'source>,
    bases: &[Option<ClassKey<'source>>],
    orders: &Orders<'source>,
    work: &mut Work,
) -> Option<ClassOrder<'source>> {
    if !work.charge() {
        return None;
    }
    if bases.is_empty() {
        return Some(ClassOrder::inherited(ClassKey::Object, true));
    }
    if let [base] = bases {
        let Some(base) = base else {
            return Some(ClassOrder::partial(class));
        };
        let order = orders.get(base)?;
        return Some(match order.classes {
            OrderedClasses::Invalid => ClassOrder::invalid(),
            _ => ClassOrder::inherited(*base, order.complete),
        });
    }
    let mut known = BTreeSet::new();
    let mut complete = true;
    for base in bases {
        if !work.charge() {
            return None;
        }
        match base {
            Some(base) => {
                if !known.insert(*base) {
                    return None;
                }
                let order = orders.get(base)?;
                if matches!(order.classes, OrderedClasses::Invalid) {
                    return Some(ClassOrder::invalid());
                }
                complete &= order.complete;
            }
            None => complete = false,
        }
    }
    if !complete {
        // The first direct base precedes its own bases in every valid class order.
        // Unknown ancestors of another base can change the order after that point.
        let first = bases.first().copied().flatten();
        let classes = std::iter::once(class).chain(first).collect();
        return Some(ClassOrder::merged(classes, false));
    }
    let direct = bases.iter().copied().collect::<Option<Vec<_>>>()?;
    let mut sequences = Vec::with_capacity(direct.len() + 1);
    for base in &direct {
        let mut sequence = VecDeque::new();
        for class in ordered(orders, *base) {
            if !work.charge() {
                return None;
            }
            sequence.push_back(class);
        }
        sequences.push(sequence);
    }
    sequences.push(direct.into());
    merge_sequences(class, sequences, work)
}

/// Merges the orders of the bases of `class`, with its bases as the last sequence, by the C3
/// rule: the next class is the first head that no sequence holds behind its own head.
///
/// Each class leaves each sequence once, and the next head is read from an index, so the work
/// is the length of the sequences and does not grow with their number.
fn merge_sequences<'source>(
    class: ClassKey<'source>,
    sequences: Vec<VecDeque<ClassKey<'source>>>,
    work: &mut Work,
) -> Option<ClassOrder<'source>> {
    let mut merge = Merge::new(sequences);
    let mut order = vec![class];
    while !merge.heads.is_empty() {
        order.push(merge.take_next(work)?);
    }
    Some(ClassOrder::merged(order, true))
}

/// Sequences under a C3 merge, with their heads indexed.
struct Merge<'source> {
    sequences: Vec<VecDeque<ClassKey<'source>>>,
    /// How many sequences hold a class behind their head, for the classes one holds.
    behind: BTreeMap<ClassKey<'source>, usize>,
    /// The sequences each class heads.
    heads: BTreeMap<ClassKey<'source>, Vec<usize>>,
    /// The sequences whose head no sequence holds behind its own.
    ready: BTreeSet<usize>,
}

impl<'source> Merge<'source> {
    fn new(sequences: Vec<VecDeque<ClassKey<'source>>>) -> Self {
        let mut behind = BTreeMap::<_, usize>::new();
        let mut heads = BTreeMap::<_, Vec<_>>::new();
        for (index, sequence) in sequences.iter().enumerate() {
            for class in sequence.iter().skip(1) {
                *behind.entry(*class).or_default() += 1;
            }
            if let Some(head) = sequence.front() {
                heads.entry(*head).or_default().push(index);
            }
        }
        let ready = heads
            .iter()
            .filter(|(head, _)| !behind.contains_key(*head))
            .flat_map(|(_, sequences)| sequences.iter().copied())
            .collect();
        Self {
            sequences,
            behind,
            heads,
            ready,
        }
    }

    /// The next class of the merged order, removed from every sequence it heads. `None` where
    /// no order satisfies every sequence.
    fn take_next(&mut self, work: &mut Work) -> Option<ClassKey<'source>> {
        let first = *self.ready.first()?;
        let selected = *self.sequences.get(first)?.front()?;
        for index in self.heads.remove(&selected)? {
            if !work.charge() {
                return None;
            }
            self.ready.remove(&index);
            let sequence = self.sequences.get_mut(index)?;
            sequence.pop_front();
            let Some(&head) = sequence.front() else {
                continue;
            };
            let sequences = self.heads.entry(head).or_default();
            sequences.push(index);
            // The sequences a class already heads become ready when the last one behind ends.
            match self.behind.get_mut(&head) {
                Some(count) if *count > 1 => *count -= 1,
                Some(_) => {
                    self.behind.remove(&head);
                    self.ready.extend(sequences.iter().copied());
                }
                None => {
                    self.ready.insert(index);
                }
            }
        }
        Some(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];

    fn class(index: usize) -> ClassKey<'static> {
        ClassKey::Declaration(NAMES[index], "C")
    }

    /// The C3 rule as Python states it: scan every sequence for a head in no tail.
    fn scanned(
        class: ClassKey<'static>,
        mut sequences: Vec<VecDeque<ClassKey<'static>>>,
    ) -> Option<Vec<ClassKey<'static>>> {
        let mut order = vec![class];
        loop {
            sequences.retain(|sequence| !sequence.is_empty());
            if sequences.is_empty() {
                return Some(order);
            }
            let head = sequences.iter().map(|sequence| sequence[0]).find(|head| {
                !sequences
                    .iter()
                    .any(|sequence| sequence.iter().skip(1).any(|class| class == head))
            })?;
            order.push(head);
            for sequence in &mut sequences {
                if sequence[0] == head {
                    sequence.pop_front();
                }
            }
        }
    }

    #[test]
    fn merged_orders_equal_a_scan_over_generated_hierarchies() {
        let mut state = 1_u64;
        let mut next = |bound: usize| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            usize::try_from(state >> 33).expect("31 bits") % bound
        };
        let (mut merged, mut refused) = (0, 0);
        for _ in 0..2_000 {
            let mut orders: Vec<Option<Vec<ClassKey<'static>>>> = Vec::new();
            for index in 0..NAMES.len() {
                let mut bases = Vec::new();
                for _ in 0..next(4) {
                    let base = next(index.max(1));
                    if base < index && !bases.contains(&base) {
                        bases.push(base);
                    }
                }
                let sequences = bases
                    .iter()
                    .map(|base| orders[*base].clone().map(VecDeque::from))
                    .chain([Some(bases.iter().map(|base| class(*base)).collect())])
                    .collect::<Option<Vec<_>>>();
                let Some(sequences) = sequences else {
                    orders.push(None);
                    continue;
                };
                let expected = scanned(class(index), sequences.clone());
                let mut work = Work::new(10_000);
                let found = merge_sequences(class(index), sequences, &mut work).map(|order| {
                    let OrderedClasses::Merged(classes) = order.classes else {
                        panic!("a merged order");
                    };
                    classes
                });
                assert_eq!(found.is_some(), expected.is_some());
                assert!(found == expected, "class {index}");
                match expected {
                    Some(_) => merged += 1,
                    None => refused += 1,
                }
                orders.push(expected);
            }
        }
        assert!(merged > 1_000 && refused > 100, "{merged} {refused}");
    }
}
