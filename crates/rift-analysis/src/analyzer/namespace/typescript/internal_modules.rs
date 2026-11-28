//! Modules of npm package files that no entry names.

use super::entry::{Entry, PackageFiles};
use super::{
    BTreeMap, BTreeSet, Exports, NamespaceInput, PackageMetadata, PreparedNamespace, ProjectPath,
    SelectedFile, SymbolId, SymbolIdentity, SymbolOwner, binding_name, declaration_anchors,
    observations, prepare_anchors, selected_source_is_valid,
};
use crate::static_resolution::is_declaration_file;
use rift_syntax::ScopedIdentityAnchors;
use serde::Deserialize;

mod scopes;
pub(super) use scopes::private_local_identities;

/// Names every remaining selected source by its package-relative path.
///
/// An entry or observed file keeps its module. A source of a nested package takes none, and
/// a path that another file's module or an `exports` subpath spells stays with that one.
/// With an `exports` map no consumer reaches these files by path, so their declarations are
/// public only through an alias of a file that a consumer reaches. A name the file declares
/// in more than one scope takes no identity, since one path would name distinct declarations.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
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
    let relative_types = metadata
        .exports
        .as_ref()
        .map(|exports| {
            relative_type_modules(input, files, exports, entries, selected, prepared, &foreign)
        })
        .unwrap_or_default();
    // The declaration script's path is known before a compiler version selects an entry.
    let versioned_script = if metadata.types_versions.is_some() && metadata.exports.is_none() {
        match files.unversioned_types(metadata) {
            Entry::File(path) if is_declaration_file(path.as_str()) => Some(path),
            _ => None,
        }
    } else {
        None
    };
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
        let script = is_script(file);
        if script && versioned_script.as_ref() != Some(file.path) {
            continue;
        }
        let module = format!("{}/{path}", metadata.name);
        let exported = metadata
            .exports
            .as_ref()
            .is_some_and(|exports| exports.spells_other_file(path, &language.name));
        if (exported && !relative_types.contains(file.path))
            || modules
                .get(&(language.name.clone(), module.clone()))
                .is_some_and(|holder| holder.as_deref() != Some(path))
        {
            prepared.unresolved_exports = true;
            continue;
        }
        prepared.anchors.remove(path);
        let mut anchors = declaration_anchors(input, file, Some(&module));
        let physical = module_scoped_anchors(input, file, &mut anchors, &ambient);
        if prepare_anchors(input, file, &module, anchors, prepared) {
            if !physical.is_empty() {
                prepared
                    .scoped_anchors
                    .entry(path.to_owned())
                    .or_default()
                    .extend(physical);
            }
            modules.insert((language.name.clone(), module), Some(path.to_owned()));
            if metadata.exports.is_some() || script {
                prepared
                    .unreached_modules
                    .insert(path.to_owned(), ambient.into_keys().collect());
            }
        }
    }
}

/// Places checked file-local scopes without obscuring a separate module binding.
fn module_scoped_anchors(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    anchors: &mut BTreeMap<String, SymbolId>,
    ambient: &BTreeMap<String, SymbolId>,
) -> ScopedIdentityAnchors {
    let scopes = scopes::FileScopes::new(file.syntax, file.source);
    let mut withheld = scopes.repeated_names(file);
    let original_withheld = withheld.clone();
    let arguments = scopes.separate_module_argument_properties(file, &mut withheld, anchors);
    let lexical = scopes.withheld_lexical_bindings(file, &withheld, anchors);
    let mut physical = scopes.literal_properties(file, &withheld, anchors);
    physical.extend(lexical);
    physical.extend(arguments);
    scopes.place_scoped_call_argument_properties(file, &withheld, anchors, &mut physical);
    physical.extend(scopes.withheld_union_properties(file, &withheld, anchors));
    physical.extend(scopes.withheld_conditional_properties(
        file,
        &withheld,
        anchors,
        &physical,
        input.syntax(),
    ));
    scopes.place_mixed_shorthand(file, &withheld, anchors, &mut physical);
    scopes.place_disjoint_module_bindings(file, &withheld, anchors, &mut physical);
    anchors.retain(|name, _| {
        ambient.contains_key(name) || !super::conflicting_assignment_name(name, &withheld)
    });
    if input.release_version().is_none() {
        return physical;
    }
    let restored = original_withheld
        .difference(&withheld)
        .copied()
        .collect::<BTreeSet<_>>();
    if restored.is_empty() {
        physical.extend(scopes.unplaced_lexical_bindings(file, anchors, &physical));
        return physical;
    }
    let earlier_anchors = anchors
        .iter()
        .filter(|(name, _)| {
            ambient.contains_key(*name) || !super::conflicting_assignment_name(name, &restored)
        })
        .map(|(name, identity)| (name.clone(), identity.clone()))
        .collect();
    let earlier_scopes = physical
        .iter()
        .filter(|(name, _)| !super::conflicting_assignment_name(name, &restored))
        .map(|(name, ordinals)| (name.clone(), ordinals.clone()))
        .collect();
    let earlier = scopes.unplaced_lexical_bindings(file, &earlier_anchors, &earlier_scopes);
    // A restored callable can own prior scope bindings without taking their place.
    // Keep a shared spelling only if it still names an independently placed declaration.
    let shared = file
        .syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let ordinals = earlier.get(&symbol.qualified_name)?;
            (!ordinals.contains_key(&symbol.declaration_ordinal))
                .then_some(symbol.qualified_name.as_str())
        })
        .collect::<BTreeSet<_>>();
    for (name, ordinals) in earlier {
        if !shared.contains(name.as_str()) {
            anchors.remove(&name);
        }
        physical.entry(name).or_default().extend(ordinals);
    }
    physical
}

/// Follow relative imports and re-exports from prepared declaration entries.
/// A physical path is reached once, even when several bindings name it.
fn relative_type_modules(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    exports: &Exports,
    entries: &BTreeSet<(&str, ProjectPath)>,
    selected: &[SelectedFile<'_>],
    prepared: &PreparedNamespace,
    foreign: &BTreeSet<String>,
) -> BTreeSet<ProjectPath> {
    let Exports::Subpaths(routes) = exports else {
        return BTreeSet::new();
    };
    if !routes.0.iter().any(|(key, _)| key.contains('*')) {
        return BTreeSet::new();
    }
    let declarations = selected
        .iter()
        .filter(|file| {
            file.syntax.language().name == "typescript"
                && is_declaration_file(file.path.as_str())
                && selected_source_is_valid(input, file)
                && !is_foreign(file.path.as_str(), foreign)
                && !is_script(file)
        })
        .map(|file| (file.path, file))
        .collect::<BTreeMap<_, _>>();
    let mut pending = declarations
        .values()
        .copied()
        .filter(|file| {
            entries.contains(&("typescript", file.path.clone()))
                && prepared
                    .anchors
                    .get(file.path.as_str())
                    .is_some_and(|anchors| {
                        anchors.is_empty() || *anchors != declaration_anchors(input, file, None)
                    })
        })
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    let mut relative = BTreeSet::new();
    while let Some(file) = pending.pop() {
        if !visited.insert(file.path) {
            continue;
        }
        let reexports = file
            .syntax
            .export_bindings()
            .unwrap_or_default()
            .iter()
            .filter(|binding| binding.container.is_none())
            .filter_map(|binding| binding.source);
        let imports = file
            .syntax
            .static_facts()
            .into_iter()
            .flat_map(rift_syntax::StaticFacts::typescript_module_imports)
            .copied();
        for range in reexports.chain(imports).collect::<BTreeSet<_>>() {
            let Some(specifier) = observations::literal_module_name(file.source, range) else {
                continue;
            };
            let Some(path) = files.relative_declaration(file.path, &specifier) else {
                continue;
            };
            let Some(next) = declarations.get(&path).copied() else {
                continue;
            };
            if entries.contains(&("typescript", path.clone())) {
                continue;
            }
            if exports.spells_other_file(path.as_str(), "typescript")
                && !exports.permits_relative_declaration(path.as_str(), files)
            {
                continue;
            }
            if relative.insert(path) {
                pending.push(next);
            }
        }
    }
    relative
}

/// Whether a TypeScript file is no module: what it declares belongs to the global scope and
/// to the ambient modules it names. A `.mts` or `.cts` source is always a module.
fn is_script(file: &SelectedFile<'_>) -> bool {
    file.syntax.language().name == "typescript"
        && ![".mts", ".cts"]
            .iter()
            .any(|extension| file.path.as_str().ends_with(extension))
        && !file
            .syntax
            .static_facts()
            .is_some_and(rift_syntax::StaticFacts::has_javascript_module_statement)
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
