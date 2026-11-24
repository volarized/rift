//! Captured Python class order and member lookup.

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;

use super::super::{File, Index, Target, Work};
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ClassKey<'source> {
    Declaration(&'source str, &'source str),
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
            Self::Declaration(..) => false,
        }
    }
}

#[derive(Default)]
pub(in crate::static_resolution) struct ClassOrders<'source>(
    BTreeMap<ClassKey<'source>, ClassOrder<'source>>,
);

struct ClassOrder<'source> {
    classes: Vec<ClassKey<'source>>,
    complete: bool,
}

impl<'source> ClassOrder<'source> {
    fn partial(class: ClassKey<'source>) -> Self {
        Self {
            classes: vec![class],
            complete: false,
        }
    }
}

fn key(target: Target<'_>) -> ClassKey<'_> {
    ClassKey::Declaration(
        target.file.path.as_str(),
        target.symbol.qualified_name.as_str(),
    )
}

impl<'source> Index<'source> {
    pub(in crate::static_resolution) fn python_class_orders(
        &self,
        work: &mut Work,
    ) -> Option<ClassOrders<'source>> {
        let mut pending = BTreeMap::new();
        for file in self
            .files
            .values()
            .filter(|file| file.document.facts.language().name == "python")
        {
            for symbol in file.document.facts.symbols() {
                if !work.charge() {
                    return None;
                }
                if !symbol.kinds.contains(&SymbolKind::Constructible) {
                    continue;
                }
                let target = Target {
                    file: file.document,
                    symbol,
                };
                let bases = self.python_class_bases(file, target, work)?;
                pending.insert(key(target), bases);
            }
        }
        let mut orders = BTreeMap::from([(
            ClassKey::Generic,
            ClassOrder {
                classes: vec![ClassKey::Generic],
                complete: true,
            },
        )]);
        for base in pending.values().flatten().flatten() {
            if let ClassKey::External(..) = base {
                orders.insert(
                    *base,
                    ClassOrder {
                        classes: vec![*base],
                        complete: true,
                    },
                );
            }
        }
        while !pending.is_empty() {
            let mut ready = Vec::new();
            for (class, bases) in &pending {
                if !work.charge() {
                    return None;
                }
                let mut available = true;
                for base in bases.iter().flatten() {
                    if !work.charge() {
                        return None;
                    }
                    available &= orders.contains_key(base);
                }
                if available {
                    ready.push(*class);
                }
            }
            if ready.is_empty() {
                break;
            }
            for class in ready {
                let bases = pending.remove(&class)?;
                let order = merge_order(class, &bases, &orders, work);
                if work.exhausted {
                    return None;
                }
                orders.insert(class, order.unwrap_or_else(|| ClassOrder::partial(class)));
            }
        }
        for class in pending.into_keys() {
            if !work.charge() {
                return None;
            }
            orders.insert(class, ClassOrder::partial(class));
        }
        Some(ClassOrders(orders))
    }

    fn python_class_bases(
        &self,
        file: &File<'source>,
        class: Target<'source>,
        work: &mut Work,
    ) -> Option<Vec<Option<ClassKey<'source>>>> {
        let mut bases = Vec::new();
        for edge in file.facts.heritage_edges() {
            if !work.charge() {
                return None;
            }
            if Some(edge.from) != class.symbol.name_range {
                continue;
            }
            let base = if self.python_generic_marker(file, edge.target, work) {
                Some(ClassKey::Generic)
            } else if let Some(base) = self.edge_python_base(file, edge.target, work) {
                Some(key(base))
            } else {
                self.python_external_class(file, edge.target, work)
                    .then_some(ClassKey::External(
                        file.document.path.as_str(),
                        edge.target.start,
                    ))
            };
            bases.push(base);
        }
        Some(bases)
    }

    /// A base expression names a class outside the selected sources: an unbound name (a
    /// builtin class), or a name every binding of which imports it from a module outside them,
    /// directly or through modules of the selected sources.
    fn python_external_class(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> bool {
        let Some(text) = file.text(range) else {
            return false;
        };
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
        let Some(name) = parts.next().filter(|_| named) else {
            return false;
        };
        let dotted = parts.next().is_some();
        let mut visited = BTreeSet::new();
        self.python_external_name(file, name, range.start, dotted, &mut visited, work)
    }

    fn python_external_name<'index>(
        &'index self,
        file: &'index File<'source>,
        name: &'index str,
        position: u64,
        dotted: bool,
        visited: &mut BTreeSet<(&'index str, &'index str)>,
        work: &mut Work,
    ) -> bool {
        if !work.charge() || !visited.insert((file.document.path.as_str(), name)) {
            return false;
        }
        if file.blocked(position, work) {
            return false;
        }
        let Some(bindings) = file.by_name.get(name) else {
            // Python resolves a name no scope binds among the builtins.
            return !dotted && BUILTIN_CLASSES.contains(&name);
        };
        !bindings.is_empty()
            && bindings.iter().all(|binding| match binding.kind {
                StaticBindingKind::PythonModule { module } => file
                    .text(module)
                    .and_then(|module| module.split('.').next())
                    .is_some_and(|module| self.module_value(module).is_none()),
                StaticBindingKind::PythonImport { module, export } => {
                    let Some(module) = file
                        .text(module)
                        .and_then(|module| super::EdgePythonModules::name(file, module))
                    else {
                        return false;
                    };
                    match export.and_then(|export| file.text(export)) {
                        Some(export) => match self.edge_python.path(&module) {
                            // A module of the selected sources re-exports a binding it holds.
                            Some(path) => self.files.get(path).is_some_and(|source| {
                                source.by_name.contains_key(export)
                                    && self.python_external_name(
                                        source, export, 0, dotted, visited, work,
                                    )
                            }),
                            None => {
                                self.module_value(&module).is_none()
                                    && self.module_value(&format!("{module}.{export}")).is_none()
                            }
                        },
                        None => self.module_value(&module).is_none(),
                    }
                }
                _ => false,
            })
    }

    fn python_generic_marker(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> bool {
        self.python_typing_name(file, range, "Generic", work)
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
            .is_some_and(|qualified| {
                qualified.rsplit_once('.').is_some_and(|(module, name)| {
                    name == expected && TYPING_MODULES.contains(&module)
                })
            })
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
        let order = self.edge_python_classes.0.get(&key(class))?;
        let owner = declaration.symbol.container.as_deref()?;
        for &ancestor in &order.classes {
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
        order.complete.then_some(false)
    }

    /// The member the class body of the nearest ancestor declares. An attribute a method
    /// assigns through its receiver belongs to the instance and declares none.
    pub(super) fn python_declared_base_member(
        &self,
        class: Target<'source>,
        name: &str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let order = self.edge_python_classes.0.get(&key(class))?;
        for &ancestor in order.classes.iter().skip(1) {
            if !work.charge() {
                return None;
            }
            if ancestor.blocks_member(name) {
                return None;
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
            return match self.python_receiver(Value::Symbol(callable), work)? {
                Value::Symbol(class) if class.symbol.kinds.contains(&SymbolKind::Constructible) => {
                    Some(class)
                }
                _ => None,
            };
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
        let order = self.edge_python_classes.0.get(&key(class))?;
        for &ancestor in order.classes.iter().skip(1) {
            if !work.charge() {
                return None;
            }
            if ancestor.blocks_member(name) {
                return None;
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
    file.by_name
        .get(name)
        .into_iter()
        .flatten()
        .copied()
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

fn merge_order<'source>(
    class: ClassKey<'source>,
    bases: &[Option<ClassKey<'source>>],
    orders: &BTreeMap<ClassKey<'source>, ClassOrder<'source>>,
    work: &mut Work,
) -> Option<ClassOrder<'source>> {
    let mut partial = ClassOrder::partial(class);
    if let [base] = bases {
        let Some(base) = base else {
            return Some(partial);
        };
        let parent = orders.get(base)?;
        for &class in &parent.classes {
            if !work.charge() {
                return None;
            }
            partial.classes.push(class);
        }
        partial.complete = parent.complete;
        return Some(partial);
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
                complete &= orders.get(base)?.complete;
            }
            None => complete = false,
        }
    }
    if !complete {
        // The first direct base precedes its own bases in every valid class order.
        // Unknown ancestors of another base can change the order after that point.
        if let Some(Some(base)) = bases.first() {
            partial.classes.push(*base);
        }
        return Some(partial);
    }
    let mut sequences = Vec::new();
    let mut direct = Vec::new();
    for base in bases {
        if !work.charge() {
            return None;
        }
        let base = (*base)?;
        direct.push(base);
        sequences.push(orders.get(&base)?.classes.as_slice());
    }
    sequences.push(direct.as_slice());
    let mut order = vec![class];
    while sequences.iter().any(|sequence| !sequence.is_empty()) {
        let mut selected = None;
        for sequence in &sequences {
            if !work.charge() {
                return None;
            }
            let Some(candidate) = sequence.first() else {
                continue;
            };
            let mut in_tail = false;
            for other in &sequences {
                for item in other.iter().skip(1) {
                    if !work.charge() {
                        return None;
                    }
                    in_tail |= item == candidate;
                }
            }
            if !in_tail {
                selected = Some(*candidate);
                break;
            }
        }
        let selected = selected?;
        order.push(selected);
        for sequence in &mut sequences {
            if !work.charge() {
                return None;
            }
            if sequence.first() == Some(&selected) {
                *sequence = &sequence[1..];
            }
        }
    }
    Some(ClassOrder {
        classes: order,
        complete: true,
    })
}
