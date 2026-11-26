//! TypeScript and JavaScript module identities from captured package metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rift_core::{ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::SymbolKind;
use rift_syntax::{ByteRange, LogicalDeclaration, SyntaxExportKind, SyntaxFacts, SyntaxSymbol};
use serde::{Deserialize, Deserializer, de};

use super::{ExportBinding, PreparedNamespace, SelectedFile};
use crate::NamespaceInput;

pub(crate) mod commonjs;
mod entry;
mod internal_modules;
mod observations;

use entry::Entry;
pub(super) use entry::build_paths;
pub(crate) use entry::{PackageFiles, folder_path, nested_runtime_candidates};

/// Establishes declarations in an entry module from captured owner and metadata.
/// Conditional targets no static consumer selects and uncaptured module roots remain unresolved.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
) -> PreparedNamespace {
    let metadata = captured_metadata(input);
    let mut prepared = PreparedNamespace::default();
    // Every entry of the package is decided over the files captured once here.
    let files = PackageFiles::captured(input);
    let mut imported = BTreeSet::new();
    // Each file an entry names, with a module or without one the entries settle.
    let mut entry_files = BTreeSet::new();
    if let Some(metadata) = &metadata {
        let entries = ["typescript", "javascript"].map(|language| {
            let has_files = selected
                .iter()
                .any(|file| file.syntax.language().name == language);
            let (modules, unresolved) = entry_modules(&files, metadata, language, has_files);
            prepared.unresolved_exports |= unresolved;
            (language, modules)
        });
        // The file an admitted observation names holds its module before any file the
        // manifest alone names. Two admitted observations of one module still share its
        // identity (#995).
        imported = observations::import_builds(input, &files, metadata, selected, &entries[1].1);
        let observed = observations::observed_files(input, &files, metadata, selected, &imported);
        let entries = entries.map(|(language, modules)| {
            let kept = modules
                .into_iter()
                .filter(|(path, module)| {
                    module.as_deref().is_none_or(|module| {
                        observed
                            .get(&(language, module))
                            .is_none_or(|files| files.contains(path))
                    })
                })
                .collect::<BTreeMap<_, _>>();
            (language, kept)
        });
        for (language, modules) in &entries {
            entry_files.extend(modules.keys().map(|path| (*language, path.clone())));
        }
        let inventory = selected
            .iter()
            .map(|file| (file.path, file.syntax.language().name.as_str()))
            .collect::<BTreeSet<_>>();
        for (language, modules) in &entries {
            if selected
                .iter()
                .any(|file| file.syntax.language().name == *language)
            {
                prepared.unresolved_exports |= modules
                    .keys()
                    .any(|path| !inventory.contains(&(path, *language)));
            }
        }
        for file in selected {
            let Some(module) = entries
                .iter()
                .find(|(language, _)| *language == file.syntax.language().name.as_str())
                .and_then(|(_, modules)| modules.get(file.path))
                .and_then(Option::as_deref)
            else {
                continue;
            };
            if !selected_source_is_valid(input, file) {
                continue;
            }
            prepare_file(input, file, module, &mut prepared);
        }
        prepare_ambient_modules(input, selected, &mut prepared);
        map_declared_types(input, &files, metadata, selected, &mut prepared);
    }
    observations::prepare(
        input,
        &files,
        metadata.as_ref(),
        selected,
        &imported,
        &mut prepared,
    );
    if let Some(metadata) = &metadata {
        internal_modules::prepare(
            input,
            &files,
            metadata,
            &entry_files,
            selected,
            &mut prepared,
        );
    }
    if input.owner().is_local() {
        for file in selected {
            let language = file.syntax.language();
            let supported = matches!(language.name.as_str(), "javascript" | "typescript")
                && matches!(language.dialect.as_deref(), None | Some("tsx"));
            if supported
                && !prepared.anchors.contains_key(file.path.as_str())
                && selected_source_is_valid(input, file)
            {
                // A relative module path names this source in the selected local owner.
                // Bare names are reserved for admitted package entries and observations.
                let module = format!("./{}", file.path.as_str());
                prepare_file(input, file, &module, &mut prepared);
            }
        }
    }
    retain_unique_scoped_identities(&mut prepared);
    prepared
}

fn retain_unique_scoped_identities(prepared: &mut PreparedNamespace) {
    if prepared.scoped_anchors.is_empty() {
        return;
    }
    let mut owners = BTreeMap::<&SymbolId, Option<&str>>::new();
    for (path, anchors) in &prepared.anchors {
        for identity in anchors.values() {
            owners
                .entry(identity)
                .and_modify(|owner| {
                    if *owner != Some(path.as_str()) {
                        *owner = None;
                    }
                })
                .or_insert(Some(path.as_str()));
        }
    }
    let aliases = prepared
        .exports
        .iter()
        .map(|binding| &binding.identity)
        .collect::<BTreeSet<_>>();
    let mut physical_groups = BTreeMap::<SymbolId, usize>::new();
    for groups in prepared.scoped_anchors.values() {
        for group in groups.values() {
            for identity in group.values().collect::<BTreeSet<_>>() {
                *physical_groups.entry(identity.clone()).or_default() += 1;
            }
        }
    }
    let anchors = &prepared.anchors;
    prepared.scoped_anchors.retain(|path, scoped| {
        scoped.retain(|name, overrides| {
            let Some(anchors) = anchors.get(path) else {
                return false;
            };
            let unique = if let Some(original) = anchors.get(name) {
                owners.get(original).copied().flatten() == Some(path.as_str())
            } else {
                // A private literal key has a physical identity in each body and no
                // common name that a bare use could resolve.
                overrides
                    .values()
                    .all(|identity| physical_groups.get(identity) == Some(&1))
            };
            unique
                && overrides
                    .values()
                    .all(|identity| !owners.contains_key(identity) && !aliases.contains(identity))
        });
        !scoped.is_empty()
    });
}

/// Names what the files of a package declare in ambient modules.
fn prepare_ambient_modules(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
    prepared: &mut PreparedNamespace,
) {
    if !matches!(input.owner(), SymbolOwner::Package { .. }) {
        return;
    }
    for file in selected {
        if file.syntax.language().name == "typescript" && selected_source_is_valid(input, file) {
            let ambient = declaration_anchors(input, file, None);
            if !ambient.is_empty() {
                prepared
                    .anchors
                    .entry(file.path.as_str().to_owned())
                    .or_default()
                    .extend(ambient);
            }
        }
    }
}

fn prepare_file(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: &str,
    prepared: &mut PreparedNamespace,
) -> bool {
    let anchors = declaration_anchors(input, file, Some(module));
    prepare_anchors(input, file, module, anchors, prepared)
}

fn prepare_anchors(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: &str,
    mut anchors: BTreeMap<String, SymbolId>,
    prepared: &mut PreparedNamespace,
) -> bool {
    let scoped = internal_modules::private_local_identities(file, &anchors);
    if input.release_version().is_some()
        && matches!(
            input.owner(),
            rift_protocol::identity::SymbolOwner::Package { .. }
        )
    {
        place_scoped_function_parameters(file, &mut anchors, &scoped);
    }
    if let Some(previous) = prepared.anchors.get(file.path.as_str()) {
        prepared.unresolved_exports |= previous != &anchors;
        return previous == &anchors;
    }
    let (exports, unresolved) = export_bindings(input, file, module, &anchors);
    if !scoped.is_empty() {
        prepared
            .scoped_anchors
            .insert(file.path.as_str().to_owned(), scoped);
    }
    prepared.unresolved_exports |= unresolved;
    prepared.exports.extend(exports);
    prepared
        .anchors
        .insert(file.path.as_str().to_owned(), anchors);
    true
}

fn place_scoped_function_parameters(
    file: &SelectedFile<'_>,
    anchors: &mut BTreeMap<String, SymbolId>,
    scopes: &rift_syntax::ScopedIdentityAnchors,
) {
    if scopes.is_empty() || file.syntax.language().name != "javascript" {
        return;
    }
    let Some(facts) = file.syntax.static_facts() else {
        return;
    };
    let owners = file
        .syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let placements = scopes.get(&symbol.qualified_name)?;
            let name = symbol.name_range?;
            let identity = placements
                .get(&symbol.declaration_ordinal)
                .or_else(|| anchors.get(&symbol.qualified_name))?;
            Some((name, (symbol.item_range, identity.clone())))
        })
        .collect::<BTreeMap<_, _>>();
    let mut owner_count = BTreeMap::<&SymbolId, usize>::new();
    for (_, identity) in owners.values() {
        *owner_count.entry(identity).or_default() += 1;
    }
    let sites = facts
        .parameters()
        .iter()
        .map(|site| (site.name, site))
        .collect::<BTreeMap<_, _>>();
    let mut occupied = anchors.values().cloned().collect::<BTreeSet<_>>();
    for parameter in file
        .syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.is_javascript_scoped_function_parameter())
    {
        let Some(site) = parameter.name_range.and_then(|name| sites.get(&name)) else {
            continue;
        };
        let Some((function, identity)) = site.owner.and_then(|name| owners.get(&name)) else {
            continue;
        };
        if *function != site.function || owner_count.get(identity) != Some(&1) {
            continue;
        }
        let Ok(owner) = SymbolIdentity::parse(identity.as_str()) else {
            continue;
        };
        let mut path = owner.qualified_path().to_vec();
        path.push(parameter.name.clone());
        let Ok(identity) =
            SymbolIdentity::new(owner.owner().clone(), owner.language().clone(), path)
        else {
            continue;
        };
        let Ok(identity) = SymbolId::new(identity.wire_identity()) else {
            continue;
        };
        if occupied.insert(identity.clone()) {
            anchors.insert(parameter.qualified_name.clone(), identity);
        }
    }
}

/// Resolves same-module aliases through recorded declaration and export facts.
fn export_bindings(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: &str,
    anchors: &BTreeMap<String, SymbolId>,
) -> (Vec<ExportBinding>, bool) {
    let Some(bindings) = file.syntax.export_bindings() else {
        return (Vec::new(), true);
    };
    let mut exports = Vec::new();
    let mut unresolved = false;
    let targets = declaration_targets(file.syntax, anchors);
    let owned = owned_exports(file, anchors);
    let anchor_identities = anchors
        .values()
        .map(SymbolId::as_str)
        .collect::<BTreeSet<_>>();
    for binding in bindings {
        if binding.kind != SyntaxExportKind::Named || binding.source.is_some() {
            unresolved = true;
            continue;
        }
        let Some(local) = binding
            .local
            .and_then(|range| binding_name(file.source, range))
        else {
            unresolved = true;
            continue;
        };
        let Some(name) = binding
            .exported
            .and_then(|range| binding_name(file.source, range))
        else {
            unresolved = true;
            continue;
        };
        if binding.local == binding.exported && owned.contains(&binding.local) {
            continue;
        }
        let Some(target) = targets
            .get(&(binding.container.clone(), local))
            .copied()
            .flatten()
        else {
            unresolved = true;
            continue;
        };
        let Ok(target_identity) = SymbolIdentity::parse(target.1.as_str()) else {
            unresolved = true;
            continue;
        };
        let mut path = target_identity.qualified_path().to_vec();
        if binding.container.is_some() && path.last() == Some(&name) {
            // A declaration of an ambient module exports its own name.
            continue;
        }
        if path.first().map(String::as_str) != Some(module) || path.pop().is_none() {
            unresolved = true;
            continue;
        }
        path.push(name.clone());
        let Some(identity) =
            SymbolIdentity::new(input.owner().clone(), file.syntax.language().clone(), path)
                .ok()
                .and_then(|identity| SymbolId::new(identity.wire_identity()).ok())
        else {
            unresolved = true;
            continue;
        };
        if &identity == target.1 {
            continue;
        }
        if anchor_identities.contains(identity.as_str()) {
            unresolved = true;
            continue;
        }
        let qualified_name = binding
            .container
            .as_ref()
            .map_or_else(|| name.clone(), |container| format!("{container}.{name}"));
        exports.push(ExportBinding {
            path: file.path.as_str().to_owned(),
            binding: binding.clone(),
            identity,
            target: target.1.clone(),
            target_path: file.path.as_str().to_owned(),
            target_qualified_name: target.0.qualified_name.clone(),
            name,
            qualified_name,
        });
    }
    let refused = conflicting_export_identities(&exports);
    unresolved |= !refused.is_empty();
    exports.retain(|export| !refused.contains(export.identity.as_str()));
    (exports, unresolved)
}

/// The names an export gives to a declaration published under another owner: a property of
/// an exported object literal under that object, and an assignment that shares its name
/// with a declaration under `exports`.
fn owned_exports(
    file: &SelectedFile<'_>,
    anchors: &BTreeMap<String, SymbolId>,
) -> BTreeSet<Option<ByteRange>> {
    file.syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            (symbol.syntax_element_kind == Some("pair")
                || symbol.container.as_deref() == Some(EXPORTS_NAME))
                && anchors.contains_key(&symbol.qualified_name)
        })
        .map(|symbol| symbol.name_range)
        .collect()
}

pub(crate) fn conflicting_export_identities(exports: &[ExportBinding]) -> BTreeSet<String> {
    let mut targets = BTreeMap::<&str, Option<&SymbolId>>::new();
    for export in exports {
        match targets.entry(export.identity.as_str()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(&export.target));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if *entry.get() != Some(&export.target) {
                    entry.insert(None);
                }
            }
        }
    }
    targets
        .into_iter()
        .filter(|(_, target)| target.is_none())
        .map(|(identity, _)| identity.to_owned())
        .collect()
}

fn binding_name(source: &str, range: ByteRange) -> Option<String> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    let token = source.get(start..end)?;
    if token.starts_with('"') {
        return serde_json::from_str(token).ok();
    }
    if let Some(inner) = token
        .strip_prefix('\'')
        .and_then(|token| token.strip_suffix('\''))
    {
        return (!inner.contains(['\\', '\''])).then(|| inner.to_owned());
    }
    (!token.is_empty()
        && !token
            .chars()
            .any(|character| character.is_whitespace() || "(){}[]=;".contains(character)))
    .then(|| token.to_owned())
}

fn declaration_targets<'facts>(
    syntax: &'facts SyntaxFacts,
    anchors: &'facts BTreeMap<String, SymbolId>,
) -> BTreeMap<(Option<String>, String), Option<DeclarationTarget<'facts>>> {
    let mut targets = BTreeMap::new();
    for symbol in syntax.symbols() {
        if symbol.is_ecmascript_package_binding() {
            continue;
        }
        let Some(identity) = anchors.get(&symbol.qualified_name) else {
            continue;
        };
        let key = (symbol.container.clone(), symbol.name.clone());
        match targets.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some((symbol, identity)));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().map(|(_, previous)| previous) != Some(identity) {
                    entry.insert(None);
                }
            }
        }
    }
    targets
}

fn selected_source_is_valid(input: &NamespaceInput<'_>, file: &SelectedFile<'_>) -> bool {
    matches!(
        file.syntax.language().name.as_str(),
        "typescript" | "javascript"
    ) && file.syntax.source_digest().is_some()
        && input
            .files()
            .iter()
            .any(|source| source.path() == file.path && source.text().len() == file.source.len())
}

fn captured_metadata(input: &NamespaceInput<'_>) -> Option<PackageMetadata> {
    let mut sources = input
        .files()
        .iter()
        .chain(input.context_sources())
        .filter(|source| source.path().as_str() == "package.json");
    let source = sources.next()?;
    if source.text().len() > input.syntax().source_bytes_max() || sources.next().is_some() {
        return None;
    }
    let metadata: PackageMetadata = serde_json::from_str(source.text()).ok()?;
    let accepted = match input.owner() {
        SymbolOwner::Package { manager, name, .. } => {
            manager == "npm"
                && metadata.name == *name
                && input.release_version() == Some(metadata.version.as_str())
        }
        SymbolOwner::Local
        | SymbolOwner::NamedLocal { .. }
        | SymbolOwner::ProfileLocal { .. }
        | SymbolOwner::NamedProfileLocal { .. } => !metadata.name.is_empty(),
        SymbolOwner::Runtime { .. } => false,
    };
    accepted.then_some(metadata)
}

fn entry_modules(
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    language: &str,
    has_files: bool,
) -> (BTreeMap<ProjectPath, Option<String>>, bool) {
    let Some(Exports::Subpaths(exports)) = &metadata.exports else {
        return match entry::resolve(files, metadata, language) {
            Entry::File(path) => (BTreeMap::from([(path, Some(metadata.name.clone()))]), false),
            Entry::Unknown => (BTreeMap::new(), has_files),
            Entry::Absent => (BTreeMap::new(), false),
        };
    };
    let mut candidates = BTreeMap::<ProjectPath, Vec<EntryCandidate<'_>>>::new();
    let mut unresolved = false;
    for (key, target) in &exports.0 {
        let suffix = if key == "." {
            ""
        } else if let Some(suffix) = key.strip_prefix("./") {
            if !export_subpath_is_valid(suffix) {
                unresolved = true;
                continue;
            }
            &key[1..]
        } else {
            unresolved = true;
            continue;
        };
        let path = target
            .target(language)
            .and_then(|path| path.strip_prefix("./"))
            .filter(|relative| export_subpath_is_valid(relative))
            .and_then(|relative| ProjectPath::new(relative).ok());
        let Some(path) = path else {
            unresolved = true;
            continue;
        };
        candidates.entry(path).or_default().push(EntryCandidate {
            key,
            module: format!("{}{suffix}", metadata.name),
            unconditional: target.is_unconditional(language),
        });
    }
    let modules = candidates
        .into_iter()
        .map(|(path, candidates)| {
            let (module, unresolved_aliases) = file_module(candidates);
            unresolved |= unresolved_aliases || module.is_none();
            (path, module)
        })
        .collect();
    (modules, unresolved)
}

/// One subpath whose target is a file.
struct EntryCandidate<'metadata> {
    key: &'metadata str,
    module: String,
    unconditional: bool,
}

/// The module of the selected file and whether competing unconditional aliases remain.
/// Among unconditional routes only the authored package root can settle a shared file.
/// Otherwise preserve the existing precedence of unconditional routes and ordered keys.
fn file_module(mut candidates: Vec<EntryCandidate<'_>>) -> (Option<String>, bool) {
    let unconditional = candidates
        .iter()
        .filter(|candidate| candidate.unconditional)
        .count();
    if unconditional > 1 {
        let root = candidates
            .into_iter()
            .find(|candidate| candidate.unconditional && candidate.key == ".")
            .map(|candidate| candidate.module);
        return (root, true);
    }
    candidates.sort_by_key(|candidate| {
        (
            !candidate.unconditional,
            candidate.key != ".",
            candidate.key,
        )
    });
    let module = candidates
        .into_iter()
        .next()
        .map(|candidate| candidate.module);
    (module, false)
}

fn export_subpath_is_valid(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(|character| {
            character.is_control() || matches!(character, '*' | '%' | '?' | '#' | '\\')
        })
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".." | "node_modules"))
}

fn entry_path(metadata: &PackageMetadata, language: &str) -> Option<ProjectPath> {
    let target = entry_target(metadata, language)?;
    let path = ProjectPath::new(target.strip_prefix("./").unwrap_or(target)).ok()?;
    (!path.as_str().is_empty()).then_some(path)
}

/// Associates explicit declared types with one captured defining export.
/// Callable facts establish category, not parameter or return equivalence.
/// Export conditions that name several implementation files associate none of them.
fn map_declared_types(
    input: &NamespaceInput<'_>,
    files: &PackageFiles<'_>,
    metadata: &PackageMetadata,
    selected: &[SelectedFile<'_>],
    prepared: &mut PreparedNamespace,
) {
    if metadata
        .exports
        .as_ref()
        .is_some_and(Exports::has_several_implementations)
    {
        return;
    }
    // Two written implementations leave open which one the types describe.
    if matches!((&metadata.main, &metadata.module), (Some(main), Some(module)) if main != module) {
        return;
    }
    let Entry::File(types_path) = entry::resolve(files, metadata, "typescript") else {
        return;
    };
    let Entry::File(implementation_path) = entry::resolve(files, metadata, "javascript") else {
        return;
    };
    if types_path == implementation_path {
        return;
    }
    let mut types = selected.iter().filter(|file| file.path == &types_path);
    let Some(types_file) = types.next() else {
        return;
    };
    let mut implementations = selected
        .iter()
        .filter(|file| file.path == &implementation_path);
    let Some(implementation) = implementations.next() else {
        return;
    };
    if types.next().is_some()
        || implementations.next().is_some()
        || types_file.syntax.language().name != "typescript"
        || implementation.syntax.language().name != "javascript"
        || !selected_source_is_valid(input, types_file)
        || !selected_source_is_valid(input, implementation)
    {
        return;
    }
    let Some(types_anchors) = prepared.anchors.get(types_path.as_str()) else {
        return;
    };
    let Some(implementation_anchors) = prepared.anchors.get(implementation_path.as_str()) else {
        return;
    };
    let mapped = map_files(
        types_file,
        implementation,
        types_anchors,
        implementation_anchors,
        None,
    );
    if !mapped.is_empty() {
        prepared
            .mappings
            .entry(types_path.as_str().to_owned())
            .or_default()
            .extend(mapped);
    }
}

fn map_files(
    types_file: &SelectedFile<'_>,
    implementation: &SelectedFile<'_>,
    types_anchors: &BTreeMap<String, SymbolId>,
    implementation_anchors: &BTreeMap<String, SymbolId>,
    container: Option<&str>,
) -> BTreeMap<String, LogicalDeclaration> {
    let types = named_export_targets_in(types_file, types_anchors, container);
    let implementations = named_export_targets(implementation, implementation_anchors);
    let mut mapped = BTreeMap::new();
    for (name, declared) in types {
        let (Some((declared, _)), Some(Some((target, identity)))) =
            (declared, implementations.get(&name))
        else {
            continue;
        };
        let declared_callable = declared.kinds.contains(&SymbolKind::Callable);
        let target_callable = target.kinds.contains(&SymbolKind::Callable);
        let category_matches = match (declared.kind, target.kind) {
            ("variable", "variable") => {
                declared.kinds.contains(&SymbolKind::Value)
                    && target.kinds.contains(&SymbolKind::Value)
                    && declared_callable == target_callable
            }
            ("function" | "variable", "function") => {
                declared_callable
                    && target_callable
                    && (declared.kind == "variable" || !declared.signatures.is_empty())
                    && !target.signatures.is_empty()
            }
            _ => false,
        };
        if !category_matches || target.container.is_some() {
            continue;
        }
        let declared_identity = types_anchors.get(&declared.qualified_name);
        for symbol in types_file.syntax.symbols() {
            if symbol.container.as_deref() == container
                && symbol.name == declared.name
                && types_anchors.get(&symbol.qualified_name) == declared_identity
            {
                mapped.insert(
                    symbol.qualified_name.clone(),
                    LogicalDeclaration {
                        identity: (*identity).clone(),
                        language: implementation.syntax.language().clone(),
                    },
                );
            }
        }
    }
    mapped
}

type DeclarationTarget<'facts> = (&'facts SyntaxSymbol, &'facts SymbolId);

fn named_export_targets<'facts>(
    file: &'facts SelectedFile<'_>,
    anchors: &'facts BTreeMap<String, SymbolId>,
) -> BTreeMap<String, Option<DeclarationTarget<'facts>>> {
    named_export_targets_in(file, anchors, None)
}

fn named_export_targets_in<'facts>(
    file: &'facts SelectedFile<'_>,
    anchors: &'facts BTreeMap<String, SymbolId>,
    container: Option<&str>,
) -> BTreeMap<String, Option<DeclarationTarget<'facts>>> {
    let declarations = declaration_targets(file.syntax, anchors);
    let mut exports = BTreeMap::new();
    for binding in file.syntax.export_bindings().unwrap_or_default() {
        if binding.kind != SyntaxExportKind::Named
            || binding.source.is_some()
            || binding.container.as_deref() != container
        {
            continue;
        }
        let Some(local) = binding
            .local
            .and_then(|range| binding_name(file.source, range))
        else {
            continue;
        };
        let Some(name) = binding
            .exported
            .and_then(|range| binding_name(file.source, range))
        else {
            continue;
        };
        let target = declarations
            .get(&(container.map(str::to_owned), local))
            .copied()
            .flatten();
        match exports.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(target);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if entry.get().map(|(_, identity)| identity) != target.map(|(_, identity)| identity)
                {
                    entry.insert(None);
                }
            }
        }
    }
    exports
}

fn entry_target<'metadata>(
    metadata: &'metadata PackageMetadata,
    language: &str,
) -> Option<&'metadata str> {
    if let Some(exports) = &metadata.exports {
        return exports.target(language);
    }
    if metadata.types_versions.is_some() {
        return None;
    }
    match language {
        "typescript" => metadata.types.as_deref().or(metadata.typings.as_deref()),
        "javascript" => match (&metadata.main, &metadata.module) {
            (Some(main), Some(module)) if main != module => None,
            (Some(main), _) => Some(main),
            (_, Some(module)) => Some(module),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Deserialize)]
struct PackageMetadata {
    name: String,
    version: String,
    types: Option<String>,
    typings: Option<String>,
    main: Option<String>,
    module: Option<String>,
    exports: Option<Exports>,
    #[serde(rename = "typesVersions")]
    types_versions: Option<serde_json::Value>,
    #[serde(rename = "type")]
    module_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CapturedExports {
    Direct(String),
    Entries(OrderedConditions),
    Alternatives(Vec<ExportTarget>),
}

#[derive(Deserialize)]
#[serde(from = "CapturedExports")]
enum Exports {
    Direct(String),
    Subpaths(OrderedConditions),
}

impl From<CapturedExports> for Exports {
    /// A map whose keys name no subpath holds the conditions of the package entry itself.
    fn from(captured: CapturedExports) -> Self {
        match captured {
            CapturedExports::Direct(path) => Self::Direct(path),
            CapturedExports::Entries(entries)
                if entries.0.iter().any(|(key, _)| key.starts_with('.')) =>
            {
                Self::Subpaths(entries)
            }
            CapturedExports::Entries(conditions) => Self::Subpaths(OrderedConditions(vec![(
                ".".to_owned(),
                ExportTarget::Conditions(conditions),
            )])),
            CapturedExports::Alternatives(alternatives) => {
                Self::Subpaths(OrderedConditions(vec![(
                    ".".to_owned(),
                    ExportTarget::Alternatives(alternatives),
                )]))
            }
        }
    }
}

impl Exports {
    /// Whether the subpath `./path` names anything but the file at `path` to a consumer of
    /// `language`. The exact key decides, else the pattern with the longest prefix, as Node
    /// matches them.
    fn spells_other_file(&self, path: &str, language: &str) -> bool {
        let Self::Subpaths(paths) = self else {
            return false;
        };
        let spelled = format!("./{path}");
        if let Some((_, target)) = paths.0.iter().find(|(key, _)| *key == spelled) {
            return target.names_other(&spelled, None, language);
        }
        paths
            .0
            .iter()
            .filter_map(|(key, target)| {
                let (prefix, suffix) = key.split_once('*')?;
                let matched = spelled.strip_prefix(prefix)?.strip_suffix(suffix)?;
                (!matched.is_empty()).then_some((prefix.len(), key.len(), matched, target))
            })
            .max_by_key(|(prefix, key, ..)| (*prefix, *key))
            .is_some_and(|(.., matched, target)| {
                target.names_other(&spelled, Some(matched), language)
            })
    }

    fn target(&self, language: &str) -> Option<&str> {
        match self {
            Self::Direct(path) => Some(path),
            Self::Subpaths(_) => self.entry()?.target(language),
        }
    }

    fn entry(&self) -> Option<&ExportTarget> {
        let Self::Subpaths(paths) = self else {
            return None;
        };
        let (_, target) = paths.0.iter().find(|(key, _)| key == ".")?;
        Some(target)
    }

    /// Whether the conditions of the package entry name more than one file that is no
    /// declared type: which of them a declared type describes depends on the consumer.
    fn has_several_implementations(&self) -> bool {
        self.entry()
            .is_some_and(|target| target.implementations().len() > 1)
    }

    fn observed_targets(
        &self,
        name: &str,
        module: &str,
        conditions: &[&str],
    ) -> Option<(&str, &str)> {
        let key = if name == module {
            ".".to_owned()
        } else {
            let suffix = module.strip_prefix(name)?.strip_prefix('/')?;
            if !export_subpath_is_valid(suffix) {
                return None;
            }
            format!("./{suffix}")
        };
        match self {
            Self::Direct(path) if name == module => Some((path, path)),
            Self::Direct(_) => None,
            Self::Subpaths(paths) => {
                let target = &paths.0.iter().find(|(subpath, _)| subpath == &key)?.1;
                target
                    .selected_target(true, conditions)
                    .zip(target.selected_target(false, conditions))
            }
        }
    }
}

/// The condition naming declared types, which a TypeScript consumer alone selects.
const TYPES_CONDITION: &str = "types";
/// The condition every consumer selects.
const DEFAULT_CONDITION: &str = "default";
/// How a static consumer loads a module. Node matches exactly one of `import` and `require`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Loader {
    Module,
    CommonJs,
}

/// The conditions a consumer of `language` matches beside `types` and `default`. Custom,
/// community and bundler conditions match none: no static consumer enables them.
fn consumer_conditions(language: &str, loader: Loader) -> &'static [&'static str] {
    match (language, loader) {
        ("typescript", Loader::Module) => &["node", "import"],
        ("typescript", Loader::CommonJs) => &["node", "require"],
        (_, Loader::Module) => &["node", "import", "module-sync"],
        (_, Loader::CommonJs) => &["node", "require", "module-sync"],
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExportTarget {
    Direct(String),
    Conditions(OrderedConditions),
    Alternatives(Vec<ExportTarget>),
    Unresolved(UnresolvedTarget),
}

#[derive(Deserialize)]
#[serde(from = "Option<Vec<de::IgnoredAny>>")]
enum UnresolvedTarget {
    /// `null`, which excludes the subpath for the conditions it is written under.
    Excluded,
    /// A target list with an unsupported element.
    List,
}

impl From<Option<Vec<de::IgnoredAny>>> for UnresolvedTarget {
    fn from(captured: Option<Vec<de::IgnoredAny>>) -> Self {
        match captured {
            None => Self::Excluded,
            Some(_) => Self::List,
        }
    }
}

/// What one export target gives a consumer.
enum SelectedTarget<'metadata> {
    Path {
        path: &'metadata str,
        declared: bool,
    },
    /// The written target gives this consumer no file.
    Excluded,
    /// Which file the written target gives this consumer is not established.
    Refused,
    /// The earlier array choice is unresolved even if another loader could select a file.
    RefusedAlternative,
    /// No condition of the consumer is written: a later condition can still match.
    Unmatched,
}

impl ExportTarget {
    /// The target that holds the module identity for `language`: the one an ECMAScript
    /// module consumer selects in the declared order, else the one a `require` selects.
    fn target(&self, language: &str) -> Option<&str> {
        for loader in [Loader::Module, Loader::CommonJs] {
            match self.select(
                language == "typescript",
                consumer_conditions(language, loader),
                false,
            ) {
                SelectedTarget::Path { path, .. } => return Some(path),
                SelectedTarget::RefusedAlternative => return None,
                SelectedTarget::Excluded | SelectedTarget::Refused | SelectedTarget::Unmatched => {}
            }
        }
        None
    }

    /// Whether the target was resolved by its first condition alone: a direct file, a first
    /// `default`, or a first `types` for TypeScript. A subpath that first resolves through
    /// the condition walk never takes a file from such a subpath.
    fn is_unconditional(&self, language: &str) -> bool {
        match self {
            Self::Direct(_) => true,
            Self::Unresolved(_) => false,
            Self::Alternatives(targets) => targets
                .first()
                .is_some_and(|target| target.is_unconditional(language)),
            Self::Conditions(conditions) => {
                conditions.0.first().is_some_and(|(condition, target)| {
                    (condition == DEFAULT_CONDITION
                        || (condition == TYPES_CONDITION && language == "typescript"))
                        && target.is_unconditional(language)
                })
            }
        }
    }

    /// Whether the target gives a consumer of `language` anything but `file`, with `matched`
    /// in place of `*`. Conditions no static consumer enables count when it enables none.
    fn names_other(&self, file: &str, matched: Option<&str>, language: &str) -> bool {
        let is_file = |path: &str| match matched {
            Some(matched) => path.replace('*', matched) == file,
            None => path == file,
        };
        let mut unmatched = true;
        for loader in [Loader::Module, Loader::CommonJs] {
            let conditions = consumer_conditions(language, loader);
            match self.select(language == "typescript", conditions, false) {
                SelectedTarget::Unmatched => continue,
                SelectedTarget::Excluded => {}
                SelectedTarget::Path { path, .. } if is_file(path) => {}
                SelectedTarget::Path { .. }
                | SelectedTarget::Refused
                | SelectedTarget::RefusedAlternative => return true,
            }
            unmatched = false;
        }
        let mut pending = vec![self];
        while let Some(target) = pending.pop().filter(|_| unmatched) {
            match target {
                Self::Direct(path) if is_file(path) => {}
                Self::Unresolved(UnresolvedTarget::Excluded) => {}
                Self::Direct(_) | Self::Unresolved(UnresolvedTarget::List) => return true,
                Self::Alternatives(targets) => pending.extend(targets),
                Self::Conditions(conditions) => {
                    pending.extend(conditions.0.iter().map(|(_, target)| target));
                }
            }
        }
        false
    }

    fn selected_target(&self, types: bool, selected: &[&str]) -> Option<&str> {
        match self.select(types, selected, false) {
            SelectedTarget::Path { path, .. } => Some(path),
            SelectedTarget::Excluded
            | SelectedTarget::Refused
            | SelectedTarget::RefusedAlternative
            | SelectedTarget::Unmatched => None,
        }
    }

    /// The JSON reader that captured the metadata refuses nesting past its recursion limit.
    fn select(&self, types: bool, selected: &[&str], declared: bool) -> SelectedTarget<'_> {
        let conditions = match self {
            Self::Direct(path) => return SelectedTarget::Path { path, declared },
            Self::Unresolved(UnresolvedTarget::Excluded) => return SelectedTarget::Excluded,
            Self::Unresolved(UnresolvedTarget::List) => return SelectedTarget::Refused,
            Self::Alternatives(targets) => {
                return Self::select_alternative(targets, types, selected, declared);
            }
            Self::Conditions(conditions) => conditions,
        };
        for (name, target) in &conditions.0 {
            // A compiler version decides this selector, and no version is captured.
            if name.starts_with("types@") {
                if types {
                    return SelectedTarget::Refused;
                }
                continue;
            }
            if name != DEFAULT_CONDITION
                && !(types && name == TYPES_CONDITION)
                && !selected.contains(&name.as_str())
            {
                continue;
            }
            match target.select(types, selected, declared || name == TYPES_CONDITION) {
                SelectedTarget::Unmatched => {}
                found => return found,
            }
        }
        SelectedTarget::Unmatched
    }

    /// Use an exact declaration target selected under a written `types` condition.
    /// A runtime path can redirect to types through extension substitution, which this
    /// metadata walk does not prove.
    /// Only a nonmatching condition lets the next array member decide the entry.
    fn select_alternative<'metadata>(
        targets: &'metadata [Self],
        types: bool,
        selected: &[&str],
        declared: bool,
    ) -> SelectedTarget<'metadata> {
        if targets.is_empty() {
            return SelectedTarget::Excluded;
        }
        for target in targets {
            match target.select(types, selected, declared) {
                SelectedTarget::Unmatched => {}
                SelectedTarget::Refused => return SelectedTarget::RefusedAlternative,
                SelectedTarget::Path { path, declared } => {
                    let Some(relative) = path.strip_prefix("./") else {
                        return SelectedTarget::RefusedAlternative;
                    };
                    if !export_subpath_is_valid(relative)
                        || relative
                            .split('/')
                            .any(|part| part.eq_ignore_ascii_case("node_modules"))
                        || (types
                            && (!declared
                                || !crate::static_resolution::is_declaration_file(relative)))
                    {
                        return SelectedTarget::RefusedAlternative;
                    }
                    return SelectedTarget::Path { path, declared };
                }
                found => return found,
            }
        }
        SelectedTarget::Unmatched
    }

    /// Every file written outside a declared types condition.
    fn implementations(&self) -> BTreeSet<&str> {
        let mut paths = BTreeSet::new();
        let mut pending = vec![self];
        while let Some(target) = pending.pop() {
            match target {
                Self::Direct(path) => {
                    paths.insert(path.as_str());
                }
                Self::Conditions(conditions) => pending.extend(
                    conditions
                        .0
                        .iter()
                        .filter(|(name, _)| name != TYPES_CONDITION)
                        .map(|(_, target)| target),
                ),
                Self::Alternatives(targets) => pending.extend(targets),
                Self::Unresolved(_) => {}
            }
        }
        paths
    }
}

/// JSON condition order decides which target applies; retain the order from serde's map reader.
struct OrderedConditions(Vec<(String, ExportTarget)>);

impl<'de> Deserialize<'de> for OrderedConditions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ConditionsVisitor;
        impl<'de> de::Visitor<'de> for ConditionsVisitor {
            type Value = OrderedConditions;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("package export conditions")
            }
            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut conditions = Vec::new();
                let mut names = BTreeSet::new();
                while let Some((name, value)) = map.next_entry::<String, ExportTarget>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom("duplicate package export condition"));
                    }
                    conditions.push((name, value));
                }
                Ok(OrderedConditions(conditions))
            }
        }
        deserializer.deserialize_map(ConditionsVisitor)
    }
}

fn declaration_anchors(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: Option<&str>,
) -> BTreeMap<String, SymbolId> {
    declaration_anchors_with_public(input, file, module, &BTreeSet::new())
}

fn declaration_anchors_with_public(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: Option<&str>,
    public: &BTreeSet<String>,
) -> BTreeMap<String, SymbolId> {
    let syntax = file.syntax;
    let source = file.source;
    let mut paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(module) = module {
        // An export assignment whose name a module-scope declaration takes is declared
        // under `exports`, the module's own object.
        paths.insert(
            EXPORTS_NAME.to_owned(),
            vec![module.to_owned(), EXPORTS_NAME.to_owned()],
        );
    }
    let mut anchors = BTreeMap::new();
    let conflicts = assignment_name_conflicts(syntax, source);
    let assignments = syntax
        .export_bindings()
        .unwrap_or_default()
        .iter()
        .filter(|binding| binding.source.is_none())
        .filter_map(|binding| Some((binding.range, binding.local?, binding.container.as_deref())))
        .collect::<BTreeSet<_>>();
    let mut declarations = syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.syntax_element_kind != Some("assignment_expression")
                || member_assignment(symbol)
                || symbol.kinds.contains(&SymbolKind::Public)
                || public.contains(&symbol.qualified_name)
                || symbol.name_range.is_some_and(|name| {
                    assignments.contains(&(symbol.item_range, name, export_container(symbol)))
                })
        })
        .collect::<Vec<_>>();
    // A checked out-of-line member may precede its hoisted constructor in source.
    // Process containing declarations first without changing the document's source order.
    declarations.sort_by_key(|symbol| symbol.qualified_name.len());
    for symbol in declarations {
        if symbol.is_ecmascript_package_binding()
            && (input.release_version().is_none()
                || !matches!(
                    input.owner(),
                    rift_protocol::identity::SymbolOwner::Package { .. }
                ))
        {
            continue;
        }
        if conflicting_assignment_name(&symbol.qualified_name, &conflicts) {
            continue;
        }
        let literal = declared_module_name(symbol, source);
        let parameter_scope = supplemental_parameter_scope(symbol);
        let Some(mut path) = literal
            .as_ref()
            .map(|name| vec![name.clone()])
            .or_else(|| container_path(symbol, &paths, module))
        else {
            continue;
        };
        if let Some(scope) = parameter_scope {
            path.push(scope.to_owned());
        } else if symbol.qualified_name != defining_name(symbol) {
            continue;
        }
        if literal.is_none() {
            path.push(symbol.name.clone());
        }
        let Ok(identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            path.clone(),
        ) else {
            continue;
        };
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            paths.insert(symbol.qualified_name.clone(), path);
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    let mut anchors = super::retain_valid_declarations(input, file, anchors);
    if let Some(module) = module
        && let Ok(identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            vec![module.to_owned()],
        )
        && let Ok(identity) = SymbolId::new(identity.wire_identity())
    {
        anchors.insert(".".to_owned(), identity);
    }
    anchors
}

fn assignment_name_conflicts<'source>(
    syntax: &'source SyntaxFacts,
    source: &str,
) -> BTreeSet<&'source str> {
    let lexical = syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.syntax_element_kind != Some("assignment_expression"))
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    // A member assigned to a class or a constructor function is another facet of a
    // same-named member (D38), never a conflict.
    let assignments = syntax
        .symbols()
        .iter()
        .filter(|symbol| {
            symbol.syntax_element_kind == Some("assignment_expression")
                && !member_assignment(symbol)
        })
        .map(|symbol| symbol.qualified_name.as_str())
        .filter(|name| lexical.contains(name))
        .collect::<BTreeSet<_>>();
    let literal = syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.syntax_element_kind == Some("assignment_expression"))
        .filter(|symbol| assignment_name_is_literal(symbol, source))
        .map(|symbol| symbol.qualified_name.as_str())
        .collect::<BTreeSet<_>>();
    let generated = syntax
        .symbols()
        .iter()
        .filter(|symbol| symbol.syntax_element_kind == Some("assignment_expression"))
        .filter(|symbol| !assignment_name_is_literal(symbol, source))
        .map(|symbol| symbol.qualified_name.as_str());
    assignments
        .into_iter()
        .chain(generated.filter(|name| literal.contains(name)))
        .collect()
}

/// Whether an assignment declares a member of the declaration it belongs to: a method
/// or a field assigned to a class, a constructor function or its prototype.
fn member_assignment(symbol: &SyntaxSymbol) -> bool {
    matches!(symbol.kind, "method" | "property") && symbol.container.is_some()
}

fn assignment_name_is_literal(symbol: &SyntaxSymbol, source: &str) -> bool {
    let Some(range) = symbol.name_range else {
        return false;
    };
    let (Ok(start), Ok(end)) = (usize::try_from(range.start), usize::try_from(range.end)) else {
        return false;
    };
    source.get(start..end) == Some(symbol.name.as_str())
}

fn conflicting_assignment_name(mut name: &str, conflicts: &BTreeSet<&str>) -> bool {
    // Each step removes a segment from the provider-bounded qualified name.
    loop {
        if conflicts.contains(name) {
            return true;
        }
        let Some((parent, _)) = name.rsplit_once('.') else {
            return false;
        };
        name = parent;
    }
}

fn supplemental_parameter_scope(symbol: &SyntaxSymbol) -> Option<&str> {
    let name = if symbol.is_ecmascript_callback_parameter() && symbol.container.is_none() {
        symbol.qualified_name.as_str()
    } else if symbol.is_ecmascript_overload_parameter() {
        symbol
            .qualified_name
            .strip_prefix(symbol.container.as_deref()?)?
            .strip_prefix('.')?
    } else {
        return None;
    };
    let (scope, name) = name.split_once('.')?;
    (name == symbol.name).then_some(scope)
}

fn defining_name(symbol: &SyntaxSymbol) -> String {
    match &symbol.container {
        Some(container) => format!("{container}.{}", symbol.name),
        None => symbol.name.clone(),
    }
}

fn declared_module_name(symbol: &SyntaxSymbol, source: &str) -> Option<String> {
    if symbol.kind != "namespace" || symbol.container.is_some() {
        return None;
    }
    let range = symbol.name_range?;
    let start = usize::try_from(symbol.range.start).ok()?;
    let end = usize::try_from(range.start).ok()?;
    let prefix = source.get(start..end)?;
    if !prefix.split_whitespace().eq(["declare", "module"]) {
        return None;
    }
    observations::literal_module_name(source, range)
}

/// The container the syntax provider gives an export assignment that shares its name with
/// a declaration of the module.
const EXPORTS_NAME: &str = "exports";

/// The container of an export the symbol declares: `exports` is the module itself.
fn export_container(symbol: &SyntaxSymbol) -> Option<&str> {
    symbol
        .container
        .as_deref()
        .filter(|container| *container != EXPORTS_NAME)
}

fn container_path(
    symbol: &SyntaxSymbol,
    paths: &BTreeMap<String, Vec<String>>,
    module: Option<&str>,
) -> Option<Vec<String>> {
    match &symbol.container {
        Some(container) => paths.get(container).cloned(),
        None => module.map(|module| vec![module.to_owned()]),
    }
}

#[cfg(test)]
mod tests;
