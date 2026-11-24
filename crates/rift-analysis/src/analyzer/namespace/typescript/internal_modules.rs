//! Modules of npm package files that no entry names.

use super::{
    BTreeMap, BTreeSet, NamespaceInput, PackageMetadata, PreparedNamespace, ProjectPath,
    SelectedFile, SymbolId, SymbolIdentity, SymbolKind, SymbolOwner, declaration_anchors,
    prepare_file, selected_source_is_valid,
};
use serde::Deserialize;

/// Names every remaining selected source by its package-relative path.
///
/// An entry or observed file keeps its module. A source of a nested package takes none, and
/// a path another file's module already spells stays with that file. With an `exports` map
/// no consumer reaches these files by path, so their declarations are public only through an
/// alias of a file that a consumer reaches.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    metadata: &PackageMetadata,
    entries: &BTreeSet<(&str, ProjectPath)>,
    selected: &[SelectedFile<'_>],
    prepared: &mut PreparedNamespace,
) {
    if !matches!(input.owner(), SymbolOwner::Package { manager, .. } if manager == "npm") {
        return;
    }
    let foreign = foreign_directories(input, &metadata.name);
    let mut modules = held_modules(prepared);
    for file in selected {
        let language = file.syntax.language();
        let supported = matches!(language.name.as_str(), "javascript" | "typescript")
            && matches!(language.dialect.as_deref(), None | Some("tsx"));
        if !supported
            || entries.contains(&(language.name.as_str(), file.path.clone()))
            || !selected_source_is_valid(input, file)
            || is_foreign(file.path.as_str(), &foreign)
        {
            continue;
        }
        let path = file.path.as_str();
        // A declaration file's ambient modules name no module for the file itself.
        let ambient = declaration_anchors(input, file, None);
        if prepared
            .anchors
            .get(path)
            .is_some_and(|anchors| *anchors != ambient)
        {
            continue;
        }
        if is_script(file, &ambient) {
            continue;
        }
        let module = format!("{}/{path}", metadata.name);
        if modules
            .get(&(language.name.clone(), module.clone()))
            .is_some_and(|holder| holder.as_deref() != Some(path))
        {
            prepared.unresolved_exports = true;
            continue;
        }
        prepared.anchors.remove(path);
        if prepare_file(input, file, &module, prepared) {
            modules.insert((language.name.clone(), module), Some(path.to_owned()));
            if metadata.exports.is_some() {
                prepared
                    .unreached_modules
                    .insert(path.to_owned(), ambient.into_keys().collect());
            }
        }
    }
}

/// Whether a TypeScript file is no module: what it declares belongs to the global scope and
/// to the ambient modules it names.
///
/// A file that declares ambient modules is a module only when it exports a name itself, since
/// the exports written inside them are theirs.
fn is_script(file: &SelectedFile<'_>, ambient: &BTreeMap<String, SymbolId>) -> bool {
    let syntax = file.syntax;
    if syntax.language().name != "typescript" {
        return false;
    }
    if !syntax
        .static_facts()
        .is_some_and(rift_syntax::StaticFacts::has_javascript_module_syntax)
    {
        return true;
    }
    let exports = syntax
        .export_bindings()
        .unwrap_or_default()
        .iter()
        .any(|binding| binding.container.is_none())
        || syntax.symbols().iter().any(|symbol| {
            symbol.container.is_none()
                && symbol.kinds.contains(&SymbolKind::Public)
                && !ambient.contains_key(&symbol.qualified_name)
        });
    !ambient.is_empty() && !exports
}

/// The file that holds each module an established anchor names, or `None` for a module
/// several files spell.
fn held_modules(prepared: &PreparedNamespace) -> BTreeMap<(String, String), Option<String>> {
    let mut modules = BTreeMap::new();
    for (path, anchors) in &prepared.anchors {
        for anchor in anchors.values() {
            let Ok(identity) = SymbolIdentity::parse(anchor.as_str()) else {
                continue;
            };
            let Some(module) = identity.qualified_path().first() else {
                continue;
            };
            modules
                .entry((identity.language().name.clone(), module.clone()))
                .and_modify(|holder: &mut Option<String>| {
                    if holder.as_deref() != Some(path) {
                        *holder = None;
                    }
                })
                .or_insert_with(|| Some(path.clone()));
        }
    }
    modules
}

#[derive(Deserialize)]
struct NestedManifest {
    name: Option<String>,
}

/// Directories below the package root whose own manifest names another package, or cannot
/// be read.
fn foreign_directories(input: &NamespaceInput<'_>, package: &str) -> BTreeSet<String> {
    input
        .files()
        .iter()
        .chain(input.context_sources())
        .filter_map(|source| {
            let directory = source.path().as_str().strip_suffix("/package.json")?;
            let named = (source.text().len() <= input.syntax().source_bytes_max())
                .then(|| serde_json::from_str::<NestedManifest>(source.text()).ok())
                .flatten()
                .map(|manifest| manifest.name.is_none_or(|name| name == package));
            (named != Some(true)).then(|| directory.to_owned())
        })
        .collect()
}

fn is_foreign(path: &str, foreign: &BTreeSet<String>) -> bool {
    path.split('/').any(|segment| segment == "node_modules")
        || path
            .match_indices('/')
            .any(|(end, _)| foreign.contains(&path[..end]))
}
