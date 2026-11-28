//! Imports and overloads that bind one name in a Python scope.

use rift_syntax::{PythonOverload, StaticBinding, StaticBindingKind};

use super::super::{Binding, File, Index, Work};

impl<'source> Index<'source> {
    pub(super) fn python_binding_group(
        &self,
        file: &File<'source>,
        bindings: impl Iterator<Item = &'source StaticBinding>,
        work: &mut Work,
    ) -> Option<&'source StaticBinding> {
        let bindings = bindings.collect::<Vec<_>>();
        if let [first, rest @ ..] = bindings.as_slice()
            && !rest.is_empty()
            && let Some(root) = unaliased_import_root(file, first)
            && rest
                .iter()
                .all(|binding| work.charge() && unaliased_import_root(file, binding) == Some(root))
        {
            return Some(first);
        }
        if let Some(class) = super::rebinding::python_class_rebinding(file, &bindings, work) {
            return Some(class);
        }
        let mut bindings = bindings.into_iter();
        let first = bindings.next()?;
        let Some(second) = bindings.next() else {
            let signature = file.target_at(first.name).is_some_and(|target| {
                matches!(
                    target.symbol.python_overload,
                    Some(PythonOverload::Overload { .. })
                )
            });
            return (!signature).then_some(first);
        };
        let qualified = &file.target_at(first.name)?.symbol.qualified_name;
        let mut implementation = None;
        let mut signatures_end = 0;
        for binding in [first, second].into_iter().chain(bindings) {
            if !work.charge() {
                return None;
            }
            let target = file.target_at(binding.name)?.callable()?;
            if &target.symbol.qualified_name != qualified {
                return None;
            }
            match target.symbol.python_overload.as_ref()? {
                state @ PythonOverload::Overload { .. } => {
                    if !self.python_overload_import(file, state, work) {
                        return None;
                    }
                    signatures_end = signatures_end.max(target.symbol.item_range.end);
                }
                PythonOverload::Ordinary if implementation.is_none() => {
                    implementation = Some(binding);
                }
                PythonOverload::Ordinary
                | PythonOverload::GuardedImplementation
                | PythonOverload::Unknown => return None,
            }
        }
        let implementation = implementation?;
        (signatures_end <= file.target_at(implementation.name)?.symbol.item_range.start)
            .then_some(implementation)
    }

    fn python_overload_import(
        &self,
        file: &File<'source>,
        state: &PythonOverload,
        work: &mut Work,
    ) -> bool {
        let PythonOverload::Overload {
            module,
            imported,
            binding,
            decorator,
            ..
        } = state
        else {
            return false;
        };
        let Some(module_name) = file.text(*module) else {
            return false;
        };
        if file.blocked(decorator.start, work) {
            return false;
        }
        if self.edge_python.paths.contains_key(module_name) {
            return false;
        }
        let Some(name) = file.text(*binding) else {
            return false;
        };
        let Binding::Found(selected) = file.binding(name, decorator.start, |_| true, work) else {
            return false;
        };
        selected.name == *binding
            && selected.kind
                == StaticBindingKind::PythonImport {
                    module: *module,
                    export: *imported,
                }
    }
}

/// The first package component that an import without an alias binds.
fn unaliased_import_root<'source>(
    file: &File<'source>,
    binding: &StaticBinding,
) -> Option<&'source str> {
    let name = file.text(binding.name)?;
    match binding.kind {
        StaticBindingKind::PythonImport {
            module,
            export: None,
        } if module == binding.name => Some(name),
        StaticBindingKind::PythonModule { module }
            if file.text(module)?.strip_prefix(name)?.starts_with('.') =>
        {
            Some(name)
        }
        _ => None,
    }
}
