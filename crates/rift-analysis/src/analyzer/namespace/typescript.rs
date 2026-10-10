//! TypeScript and JavaScript module identities from captured package metadata.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rift_core::{FileDigest, ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOccurrence, SymbolOwner};
use rift_protocol::read::SymbolFacet;
use rift_syntax::{ByteRange, LogicalDeclaration, SyntaxExportKind, SyntaxFacts, SyntaxSymbol};
use serde::{Deserialize, Deserializer, de};

use super::{ExportBinding, PreparedNamespace, SelectedFile};
use crate::NamespaceInput;

mod observations;

/// Establishes declarations in an entry module from captured owner and metadata.
/// Conditional targets without selected conditions and uncaptured module roots remain unresolved.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    selected: &[SelectedFile<'_>],
) -> PreparedNamespace {
    let metadata = captured_metadata(input);
    let mut prepared = PreparedNamespace::default();
    if let Some(metadata) = &metadata {
        for file in selected {
            let Some(path) = entry_path(metadata, &file.syntax.language().name) else {
                continue;
            };
            if file.path != &path || !selected_source_is_valid(input, file) {
                continue;
            }
            prepare_file(input, file, &metadata.name, &mut prepared);
        }
        map_declared_types(input, metadata, selected, &mut prepared);
    }
    observations::prepare(input, metadata.as_ref(), selected, &mut prepared);
    prepared
}

fn prepare_file(
    input: &NamespaceInput<'_>,
    file: &SelectedFile<'_>,
    module: &str,
    prepared: &mut PreparedNamespace,
) -> bool {
    let anchors = declaration_anchors(input, file.syntax, file.source, module);
    if let Some(previous) = prepared.anchors.get(file.path.as_str()) {
        prepared.unresolved_exports |= previous != &anchors;
        return previous == &anchors;
    }
    let (exports, unresolved) = export_bindings(input, file, module, &anchors);
    prepared.unresolved_exports |= unresolved;
    prepared.exports.extend(exports);
    prepared
        .anchors
        .insert(file.path.as_str().to_owned(), anchors);
    true
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

fn conflicting_export_identities(exports: &[ExportBinding]) -> BTreeSet<String> {
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
    ) && !file.syntax.has_errors()
        && file.syntax.source_digest() == Some(&FileDigest::of(file.source.as_bytes()))
        && input
            .files()
            .iter()
            .any(|source| source.path() == file.path && source.text() == file.source)
}

fn captured_metadata(input: &NamespaceInput<'_>) -> Option<PackageMetadata> {
    let mut sources = input
        .files()
        .iter()
        .chain(input.context_sources())
        .filter(|source| source.path().as_str() == "package.json");
    let source = sources.next()?;
    if source.text().len() > input.syntax().source_bytes_max()
        || sources.any(|other| other.text() != source.text())
    {
        return None;
    }
    let metadata: PackageMetadata = serde_json::from_str(source.text()).ok()?;
    let accepted = match input.owner() {
        SymbolOwner::Package {
            manager,
            name,
            version,
            ..
        } => manager == "npm" && metadata.name == *name && metadata.version == *version,
        SymbolOwner::Local | SymbolOwner::NamedLocal { .. } => !metadata.name.is_empty(),
        SymbolOwner::Runtime { .. } => false,
    };
    accepted.then_some(metadata)
}

fn entry_path(metadata: &PackageMetadata, language: &str) -> Option<ProjectPath> {
    let target = entry_target(metadata, language)?;
    let path = ProjectPath::new(target.strip_prefix("./").unwrap_or(target)).ok()?;
    (!path.as_str().is_empty()).then_some(path)
}

/// Associates explicit declared types with one captured defining export.
/// Callable facts establish category, not parameter or return equivalence.
fn map_declared_types(
    input: &NamespaceInput<'_>,
    metadata: &PackageMetadata,
    selected: &[SelectedFile<'_>],
    prepared: &mut PreparedNamespace,
) {
    let Some(types_path) = entry_path(metadata, "typescript") else {
        return;
    };
    let Some(implementation_path) = entry_path(metadata, "javascript") else {
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
        let declared_callable = declared.facets.contains(&SymbolFacet::Callable);
        let target_callable = target.facets.contains(&SymbolFacet::Callable);
        let category_matches = match (declared.kind, target.kind) {
            ("variable", "variable") => {
                declared.facets.contains(&SymbolFacet::Value)
                    && target.facets.contains(&SymbolFacet::Value)
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
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Exports {
    Direct(String),
    Subpaths(ExportSubpaths),
}

impl Exports {
    fn target(&self, language: &str) -> Option<&str> {
        match self {
            Self::Direct(path) => Some(path),
            Self::Subpaths(paths) => paths.root.as_ref()?.target(language),
        }
    }

    fn observed_targets(
        &self,
        name: &str,
        module: &str,
        conditions: &[&str],
    ) -> Option<(&str, &str)> {
        if name != module {
            return None;
        }
        match self {
            Self::Direct(path) => Some((path, path)),
            Self::Subpaths(paths) => {
                let target = paths.root.as_ref()?;
                target
                    .selected_target(true, conditions)
                    .zip(target.selected_target(false, conditions))
            }
        }
    }
}

#[derive(Deserialize)]
struct ExportSubpaths {
    #[serde(rename = ".")]
    root: Option<ExportTarget>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ExportTarget {
    Direct(String),
    Conditions(OrderedConditions),
}

impl ExportTarget {
    fn target(&self, language: &str) -> Option<&str> {
        match self {
            Self::Direct(path) => Some(path),
            Self::Conditions(conditions) => {
                let (condition, value) = conditions.0.first()?;
                let matched =
                    condition == "default" || (condition == "types" && language == "typescript");
                if matched {
                    value.target(language)
                } else {
                    None
                }
            }
        }
    }

    fn selected_target(&self, types: bool, selected: &[&str]) -> Option<&str> {
        match self {
            Self::Direct(path) => Some(path),
            Self::Conditions(conditions) => {
                for (name, target) in &conditions.0 {
                    if name.starts_with("types@") {
                        return None;
                    }
                    if (name == "default"
                        || (types && name == "types")
                        || selected.contains(&name.as_str()))
                        && let Some(path) = target.selected_target(types, selected)
                    {
                        return Some(path);
                    }
                }
                None
            }
        }
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
    syntax: &SyntaxFacts,
    source: &str,
    module: &str,
) -> BTreeMap<String, SymbolId> {
    let mut groups: BTreeMap<String, Vec<&SyntaxSymbol>> = BTreeMap::new();
    for symbol in syntax.symbols() {
        groups
            .entry(defining_name(symbol))
            .or_default()
            .push(symbol);
    }
    let revision = crate::documentation::content_digest(source.as_bytes()).0;
    let mut paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut anchors = BTreeMap::new();
    for symbol in syntax.symbols() {
        let Some(mut path) = container_path(symbol, &paths, module) else {
            continue;
        };
        path.push(symbol.name.clone());
        let base = defining_name(symbol);
        let group = &groups[&base];
        let overload = syntax.language().name == "typescript" && is_function_overload(group);
        let occurrence = if symbol.qualified_name == base || overload {
            None
        } else {
            let Some(number) = symbol
                .qualified_name
                .strip_prefix(&format!("{base}~"))
                .and_then(|number| number.parse::<u32>().ok())
            else {
                continue;
            };
            if group.len() < 2 {
                continue;
            }
            Some(number)
        };
        let Ok(mut identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            path.clone(),
        ) else {
            continue;
        };
        if let Some(number) = occurrence {
            let Ok(qualified) = SymbolOccurrence::new(number, revision.clone())
                .and_then(|occurrence| identity.clone().with_occurrence(occurrence))
            else {
                continue;
            };
            identity = qualified;
        }
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            paths.insert(symbol.qualified_name.clone(), path.clone());
            if overload {
                paths.insert(base, path);
            }
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    anchors
}

fn defining_name(symbol: &SyntaxSymbol) -> String {
    match &symbol.container {
        Some(container) => format!("{container}.{}", symbol.name),
        None => symbol.name.clone(),
    }
}

fn container_path(
    symbol: &SyntaxSymbol,
    paths: &BTreeMap<String, Vec<String>>,
    module: &str,
) -> Option<Vec<String>> {
    match &symbol.container {
        Some(container) => paths.get(container).cloned(),
        None => Some(vec![module.to_owned()]),
    }
}

fn is_function_overload(group: &[&SyntaxSymbol]) -> bool {
    group.len() > 1
        && group.iter().all(|symbol| symbol.kind == "function")
        && group
            .iter()
            .filter(|symbol| symbol.body_range.is_some())
            .count()
            <= 1
}

#[cfg(test)]
mod tests;
