//! Resolves Python names through captured declarations, imports, and receivers.

mod classes;
mod references;

pub(super) use classes::ClassOrders;

use std::collections::{BTreeMap, BTreeSet};

use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{File, Index, Target, Work};
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

enum Value<'source> {
    Symbol(Target<'source>),
    Module(String),
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
        let target = self.edge_python_name(file, call.callee_range, work)?;
        (target.symbol.kinds.contains(&SymbolKind::Callable)
            || target.symbol.kinds.contains(&SymbolKind::Constructible))
        .then_some(target)
    }

    pub(super) fn edge_python_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut names = file.text(range)?.split('.').map(str::trim);
        let name = names.next()?;
        let binding = edge_python_binding(file, name, range.start, work)?;
        let mut value = self.binding_value(file, binding, work)?;
        for member in names {
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
                Value::Symbol(target) => Value::Symbol(self.python_member(target, member, work)?),
            };
        }
        self.value_target(value)
    }

    pub(super) fn edge_python_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        if let StaticBindingKind::PythonModule { module } = binding.kind {
            return self.value_target(self.module_value(file.text(module)?)?);
        }
        self.value_target(self.binding_value(file, binding, work)?)
    }

    fn value_target(&self, value: Value<'source>) -> Option<Target<'source>> {
        match value {
            Value::Symbol(target) => Some(target),
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
        match binding.kind {
            StaticBindingKind::Local | StaticBindingKind::Shadow => {
                file.target_at(binding.name).map(Value::Symbol)
            }
            StaticBindingKind::PythonReceiver { class_name } => {
                file.target_at(class_name).map(Value::Symbol)
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
        mut name: &'source str,
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
            let mut root = bindings.iter().filter(|binding| {
                binding.scope.start == 0 && binding.scope.end == file.document.source.len() as u64
            });
            let binding = root.next()?;
            if root.next().is_some() {
                return None;
            }
            match binding.kind {
                StaticBindingKind::Local => return file.target_at(binding.name).map(Value::Symbol),
                StaticBindingKind::Shadow => {
                    return file.target_at(binding.name).map(Value::Symbol);
                }
                StaticBindingKind::PythonImport { module, export } => {
                    let module = EdgePythonModules::name(file, file.text(module)?)?;
                    let Some(export) = export else {
                        return self.module_value(&module);
                    };
                    name = file.text(export)?;
                    let Some(path) = self.edge_python.path(&module) else {
                        return self.module_value(&format!("{module}.{name}"));
                    };
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

fn edge_python_binding<'source>(
    file: &File<'source>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work) {
        return None;
    }
    let in_body = file
        .enclosing_callable(ByteRange {
            start: position,
            end: position.saturating_add(1),
        })
        .is_some_and(|callable| {
            callable
                .symbol
                .body_range
                .is_some_and(|body| body.contains(position))
        });
    let bindings = file.by_name.get(name)?;
    let module = ByteRange {
        start: 0,
        end: file.document.source.len() as u64,
    };
    let mut excluded = BTreeSet::new();
    let mut global = false;
    let mut nonlocal = false;
    loop {
        let mut best: Option<ByteRange> = None;
        for &binding in bindings {
            if !work.charge() {
                return None;
            }
            let scope = binding.scope;
            if !scope.contains(position)
                || excluded.contains(&scope)
                || (global && scope != module)
                || (nonlocal && scope == module)
                || (in_body && file.class_scopes.contains(&scope))
            {
                continue;
            }
            if best.is_none_or(|best| best.encloses(scope)) {
                best = Some(scope);
            }
        }
        let scope = best?;
        let mut declaration = None;
        let mut selected = None;
        let mut ambiguous = false;
        for &binding in bindings {
            if !work.charge() {
                return None;
            }
            if binding.scope != scope {
                continue;
            }
            match binding.kind {
                StaticBindingKind::PythonGlobal => declaration = Some(true),
                StaticBindingKind::PythonNonlocal => declaration = Some(false),
                _ => {
                    ambiguous |= selected.is_some();
                    selected = Some(binding);
                }
            }
        }
        if let Some(is_global) = declaration {
            if selected.is_some() || global || (nonlocal && is_global) {
                return None;
            }
            excluded.insert(scope);
            global = is_global;
            nonlocal = !is_global;
        } else {
            return selected.filter(|binding| !ambiguous && binding.available_from <= position);
        }
    }
}
