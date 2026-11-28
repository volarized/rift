//! Declaration modules reached from an exact private package import.

use std::collections::{BTreeMap, BTreeSet};

use rift_syntax::StaticBindingKind;

use super::super::PackageImports;
use super::{
    NamespaceInput, NestedManifest, PackageFiles, ProjectPath, SelectedFile, declaration_targets,
    is_declaration_file, is_foreign, is_script, selected_source_is_valid,
};

/// Retains the selected declaration closure within the target's nearest package scope.
pub(super) fn declaration_modules(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    selected: &[SelectedFile<'_>],
    foreign: &BTreeSet<String>,
) -> BTreeSet<ProjectPath> {
    let Some(imports) = (!foreign.is_empty())
        .then(|| PackageImports::captured(input))
        .flatten()
        .filter(|imports| !imports.exact.is_empty())
    else {
        return BTreeSet::new();
    };
    let scopes = input
        .files()
        .iter()
        .chain(input.context_sources())
        .filter_map(|source| {
            let scope = source.path().as_str().strip_suffix("/package.json")?;
            let valid = source.text().len() <= input.syntax().source_bytes_max()
                && serde_json::from_str::<NestedManifest>(source.text()).is_ok();
            Some((scope, valid))
        })
        .collect::<BTreeMap<_, _>>();
    let declarations = selected
        .iter()
        .filter_map(|file| {
            let path = file.path.as_str();
            let (scope, valid) = nearest_scope(path, &scopes)?;
            (file.syntax.language().name == "typescript"
                && valid
                && is_declaration_file(path)
                && is_foreign(path, foreign)
                && !is_script(file)
                && file.syntax.declaration_syntax_complete()
                && selected_source_is_valid(input, file))
            .then_some((file.path, (file, scope)))
        })
        .collect::<BTreeMap<_, _>>();
    let roots = import_specifiers(input, selected, &scopes).filter_map(|name| {
        if !imports.exact.contains_key(name) {
            return None;
        }
        let target = imports.destination(name, true, false, || true)?;
        if is_declaration_file(target.as_str()) {
            Some(target)
        } else {
            files.runtime_declaration(target.as_str())
        }
    });
    let mut pending = roots
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut admitted = BTreeSet::new();
    while let Some(path) = pending.pop() {
        let Some(&(file, scope)) = declarations.get(&path) else {
            continue;
        };
        if !admitted.insert(path) {
            continue;
        }
        pending.extend(declaration_targets(file, files).filter(|target| {
            declarations
                .get(target)
                .is_some_and(|&(_, target_scope)| target_scope == scope)
                && !admitted.contains(target)
        }));
    }
    admitted
}

fn nearest_scope<'a>(path: &str, scopes: &BTreeMap<&'a str, bool>) -> Option<(&'a str, bool)> {
    path.match_indices('/').rev().find_map(|(end, _)| {
        scopes
            .get_key_value(&path[..end])
            .map(|(&name, &valid)| (name, valid))
    })
}

fn import_specifiers<'a, 'source>(
    input: &'a NamespaceInput<'_>,
    selected: &'a [SelectedFile<'source>],
    scopes: &'a BTreeMap<&'source str, bool>,
) -> impl Iterator<Item = &'source str> + 'a {
    selected
        .iter()
        .filter(move |file| {
            nearest_scope(file.path.as_str(), scopes).is_none()
                && file.source.contains('#')
                && file.syntax.declaration_syntax_complete()
                && selected_source_is_valid(input, file)
        })
        .flat_map(|file| {
            file.syntax
                .static_facts()
                .into_iter()
                .flat_map(rift_syntax::StaticFacts::bindings)
                .filter_map(|binding| {
                    let (StaticBindingKind::JavaScriptImport { module, .. }
                    | StaticBindingKind::JavaScriptEdgeNamespace { module }) = binding.kind
                    else {
                        return None;
                    };
                    let start = usize::try_from(module.start).ok()?;
                    let end = usize::try_from(module.end).ok()?;
                    file.source
                        .get(start..end)
                        .filter(|name| name.starts_with('#'))
                })
        })
}
