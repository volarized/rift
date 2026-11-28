//! Same-archive modules reached from an exact private package import.

mod runtime;

use std::collections::{BTreeMap, BTreeSet};

use rift_syntax::StaticBindingKind;

use super::super::PackageImports;
use super::{
    NamespaceInput, NestedManifest, PackageFiles, PackageMetadata, ProjectPath, SelectedFile,
    declaration_targets, is_declaration_file, is_foreign, is_script, selected_source_is_valid,
};

type PackageScopes<'a> = BTreeMap<&'a str, Option<NestedManifest>>;

/// Retains the reached modules within each target's nearest package scope.
pub(super) fn reached_modules(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    selected: &[SelectedFile<'_>],
    foreign: &BTreeSet<String>,
    metadata: &PackageMetadata,
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
            let manifest = (source.text().len() <= input.syntax().source_bytes_max())
                .then(|| serde_json::from_str::<NestedManifest>(source.text()).ok())
                .flatten();
            Some((scope, manifest))
        })
        .collect::<PackageScopes<'_>>();
    let mut admitted = declaration_modules(input, files, selected, foreign, &imports, &scopes);
    admitted.extend(runtime::modules(
        input, files, selected, foreign, &imports, &scopes, metadata,
    ));
    admitted
}

fn declaration_modules(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    selected: &[SelectedFile<'_>],
    foreign: &BTreeSet<String>,
    imports: &PackageImports,
    scopes: &PackageScopes<'_>,
) -> BTreeSet<ProjectPath> {
    let declarations = selected
        .iter()
        .filter_map(|file| {
            let path = file.path.as_str();
            let (scope, manifest) = nearest_scope(path, scopes)?;
            (file.syntax.language().name == "typescript"
                && manifest.is_some()
                && is_declaration_file(path)
                && is_foreign(path, foreign)
                && !is_script(file)
                && file.syntax.declaration_syntax_complete()
                && selected_source_is_valid(input, file))
            .then_some((file.path, (file, scope)))
        })
        .collect::<BTreeMap<_, _>>();
    let roots = import_specifiers(input, selected, scopes).filter_map(|name| {
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

fn nearest_scope<'a, 'manifest>(
    path: &str,
    scopes: &'manifest PackageScopes<'a>,
) -> Option<(&'a str, Option<&'manifest NestedManifest>)> {
    path.match_indices('/').rev().find_map(|(end, _)| {
        scopes
            .get_key_value(&path[..end])
            .map(|(&name, manifest)| (name, manifest.as_ref()))
    })
}

fn import_specifiers<'a, 'source>(
    input: &'a NamespaceInput<'_>,
    selected: &'a [SelectedFile<'source>],
    scopes: &'a PackageScopes<'source>,
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
