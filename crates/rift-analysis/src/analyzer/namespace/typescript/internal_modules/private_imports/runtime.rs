//! Runtime modules reached from an exact private import in the same archive.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rift_core::ReferenceRole;
use rift_syntax::{ByteRange, StaticBindingKind};

use super::super::super::{Loader, binding_name, observations};
use super::{
    NamespaceInput, PackageFiles, PackageImports, PackageMetadata, PackageScopes, ProjectPath,
    SelectedFile, is_foreign, nearest_scope, selected_source_is_valid,
};
use crate::{EnclosingDefinitions, static_resolution::commonjs_literal_require};

pub(super) fn modules(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    selected: &[SelectedFile<'_>],
    foreign: &BTreeSet<String>,
    imports: &PackageImports,
    scopes: &PackageScopes<'_>,
    metadata: &PackageMetadata,
) -> BTreeSet<ProjectPath> {
    let mut available = BTreeMap::new();
    let mut pending = BTreeSet::new();
    for file in selected.iter().filter(|file| checked(input, file)) {
        let path = file.path.as_str();
        match nearest_scope(path, scopes) {
            None if file.source.contains('#') => {
                let Some(loader) = loader(file, metadata.module_type.as_deref()) else {
                    continue;
                };
                pending.extend(
                    specifiers(file, loader)
                        .into_iter()
                        .filter(|specifier| imports.exact.contains_key(specifier.as_str()))
                        .filter_map(|specifier| {
                            imports.destination(
                                &specifier,
                                false,
                                loader == Loader::CommonJs,
                                || true,
                            )
                        }),
                );
            }
            Some((scope, Some(manifest))) if is_foreign(path, foreign) => {
                let module_type = match manifest.module_type.as_ref() {
                    None => None,
                    Some(value) => {
                        let Some(value) = value.as_str() else {
                            continue;
                        };
                        Some(value)
                    }
                };
                if let Some(loader) = loader(file, module_type) {
                    available.insert(file.path, (file, scope, loader));
                }
            }
            _ => {}
        }
    }
    let mut reached = BTreeSet::new();
    while let Some(path) = pending.pop_first() {
        let Some(&(file, scope, loader)) = available.get(&path) else {
            continue;
        };
        if !reached.insert(path.clone()) {
            continue;
        }
        for relative in specifiers(file, loader) {
            let Some(target) = files.relative_runtime(&path, &relative, loader) else {
                continue;
            };
            if available
                .get(&target)
                .is_some_and(|&(_, target_scope, _)| target_scope == scope)
                && !reached.contains(&target)
            {
                pending.insert(target);
            }
        }
    }
    reached
}

fn checked(input: &NamespaceInput<'_>, file: &SelectedFile<'_>) -> bool {
    file.syntax.language().name == "javascript"
        && file.syntax.declaration_syntax_complete()
        && selected_source_is_valid(input, file)
}

fn loader(file: &SelectedFile<'_>, module_type: Option<&str>) -> Option<Loader> {
    match Path::new(file.path.as_str()).extension()?.to_str()? {
        "cjs" => Some(Loader::CommonJs),
        "mjs" => Some(Loader::Module),
        "js" => match module_type {
            Some("module") => Some(Loader::Module),
            None if file.syntax.static_facts()?.has_javascript_module_syntax() => {
                Some(Loader::Module)
            }
            None | Some("commonjs") => Some(Loader::CommonJs),
            Some(_) => None,
        },
        _ => None,
    }
}

fn specifiers(file: &SelectedFile<'_>, loader: Loader) -> BTreeSet<String> {
    let Some(facts) = file.syntax.static_facts() else {
        return BTreeSet::new();
    };
    let mut modules = BTreeSet::new();
    match loader {
        Loader::Module => {
            let imports = facts.bindings().iter().filter_map(|binding| {
                let (StaticBindingKind::JavaScriptImport { module, .. }
                | StaticBindingKind::JavaScriptEdgeNamespace { module }) = binding.kind
                else {
                    return None;
                };
                binding_name(file.source, module)
            });
            let reexports = file
                .syntax
                .export_bindings()
                .unwrap_or_default()
                .iter()
                .filter(|binding| binding.container.is_none())
                .filter_map(|binding| {
                    observations::literal_module_name(file.source, binding.source?)
                });
            modules.extend(imports.chain(reexports));
        }
        Loader::CommonJs => {
            let is_require =
                |name: ByteRange| binding_name(file.source, name).as_deref() == Some("require");
            if facts.assignments().iter().copied().any(is_require) {
                return modules;
            }
            let scopes = EnclosingDefinitions::new(
                facts
                    .bindings()
                    .iter()
                    .filter(|binding| is_require(binding.name))
                    .map(|binding| binding.scope)
                    .chain(facts.unsupported_bindings().iter().copied())
                    .map(|range| (range.start, range.end, ())),
            );
            let available = |range: ByteRange| scopes.resolve(range.start, range.end).is_none();
            modules.extend(facts.bindings().iter().filter_map(|binding| {
                let StaticBindingKind::JavaScriptRequire {
                    require, module, ..
                } = binding.kind
                else {
                    return None;
                };
                available(require)
                    .then(|| binding_name(file.source, module))
                    .flatten()
            }));
            let arguments = facts
                .references()
                .iter()
                .filter(|reference| reference.role == ReferenceRole::Import)
                .map(|reference| reference.target)
                .collect::<BTreeSet<_>>();
            if !arguments.is_empty() {
                modules.extend(
                    facts
                        .calls()
                        .iter()
                        .filter(|call| {
                            arguments.contains(&call.range) && available(call.callee_range)
                        })
                        .filter_map(|call| commonjs_literal_require(file.source, call))
                        .filter_map(|module| binding_name(file.source, module)),
                );
            }
        }
    }
    modules
}
