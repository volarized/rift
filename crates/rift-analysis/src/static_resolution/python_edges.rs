//! Resolves Python calls through captured lexical names and module imports.

use std::collections::{BTreeMap, BTreeSet};

use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{Binding, File, Index, Target, Work};

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
            let path = file.document.path.as_str();
            let Some(stem) = path
                .strip_suffix(".py")
                .or_else(|| path.strip_suffix(".pyi"))
            else {
                continue;
            };
            let module = stem.strip_suffix("/__init__").unwrap_or(stem);
            for name in [Some(module), module.strip_prefix("src/")]
                .into_iter()
                .flatten()
            {
                modules
                    .entry(name.replace('/', "."))
                    .and_modify(|entry| *entry = None)
                    .or_insert_with(|| Some(path.to_owned()));
            }
        }
        Some(Self(modules))
    }

    fn edge_path(&self, from: &str, module: &str, work: &mut Work) -> Option<&str> {
        let dots = module.bytes().take_while(|byte| *byte == b'.').count();
        let name = if dots == 0 {
            module.to_owned()
        } else {
            let mut directory = from.rsplit_once('/').map_or("", |(directory, _)| directory);
            for _ in 1..dots {
                if !work.charge() {
                    return None;
                }
                directory = directory.rsplit_once('/')?.0;
            }
            let directory = directory
                .strip_prefix("src/")
                .unwrap_or(directory)
                .replace('/', ".");
            let suffix = module.get(dots..)?;
            if suffix.is_empty() {
                directory
            } else {
                format!("{directory}.{suffix}")
            }
        };
        self.0.get(&name)?.as_deref()
    }
}

impl<'source> Index<'source> {
    pub(super) fn edge_python_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match call.callee {
            StaticCallee::Direct => self
                .edge_python_name(file, call.callee_range, work)?
                .callable(),
            StaticCallee::Member { receiver, member } => {
                let binding =
                    edge_python_binding(file, file.text(receiver)?, call.range.start, work)?;
                let StaticBindingKind::PythonImport {
                    module,
                    export: None,
                } = binding.kind
                else {
                    return None;
                };
                let imported = self.edge_python_module(file, module, work)?;
                self.edge_python_export(imported, file.text(member)?, work)?
                    .callable()
            }
            _ => None,
        }
    }

    pub(super) fn edge_python_name(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let name = file.text(range)?;
        if let Some((receiver, member)) = name.split_once('.') {
            let binding = edge_python_binding(file, receiver, range.start, work)?;
            let StaticBindingKind::PythonImport {
                module,
                export: None,
            } = binding.kind
            else {
                return None;
            };
            let imported = self.edge_python_module(file, module, work)?;
            return self.edge_python_export(imported, member, work);
        }
        let binding = edge_python_binding(file, name, range.start, work)?;
        self.edge_python_binding_target(file, binding, work)
    }

    pub(super) fn edge_python_binding_target(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        match binding.kind {
            StaticBindingKind::Local => file.target_at(binding.name),
            StaticBindingKind::PythonImport {
                module,
                export: Some(export),
            } => {
                let imported = self.edge_python_module(file, module, work)?;
                self.edge_python_export(imported, file.text(export)?, work)
            }
            _ => None,
        }
    }

    fn edge_python_module<'index>(
        &'index self,
        file: &File<'source>,
        module: ByteRange,
        work: &mut Work,
    ) -> Option<&'index File<'source>> {
        let path =
            self.edge_python
                .edge_path(file.document.path.as_str(), file.text(module)?, work)?;
        self.files.get(path)
    }

    fn edge_python_export<'index>(
        &'index self,
        mut file: &'index File<'source>,
        mut name: &'source str,
        work: &mut Work,
    ) -> Option<Target<'source>> {
        let mut visited = BTreeSet::new();
        loop {
            if !work.charge() || !visited.insert((file.document.path.as_str(), name)) {
                return None;
            }
            let bindings = file.by_name.get(name)?;
            let mut root = bindings.iter().filter(|binding| {
                binding.scope.start == 0 && binding.scope.end == file.document.source.len() as u64
            });
            let binding = root.next()?;
            if root.next().is_some() {
                return None;
            }
            if let Some(target) = file.qualified_target(name) {
                let declared = binding.kind == StaticBindingKind::Local
                    && target.symbol.name_range == Some(binding.name);
                return (declared && !file.reassigned(name, None)).then_some(target);
            }
            let StaticBindingKind::PythonImport {
                module,
                export: Some(export),
            } = binding.kind
            else {
                return None;
            };
            name = file.text(export)?;
            file = self.edge_python_module(file, module, work)?;
        }
    }
}

fn edge_python_binding<'source>(
    file: &File<'source>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> Option<&'source StaticBinding> {
    if file.blocked(position, work) || file.reassigned(name, None) {
        return None;
    }
    match file.binding(name, position, |_| true, work) {
        Binding::Found(binding) => Some(binding),
        Binding::Missing | Binding::Blocked => None,
    }
}
