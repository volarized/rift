//! Rust library namespaces established by captured Cargo metadata.

use std::collections::{BTreeMap, VecDeque};

use rift_core::{FileDigest, ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOccurrence, SymbolOwner};
use rift_protocol::index::python_identifier_is_valid;
use rift_syntax::SyntaxFacts;
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use crate::NamespaceInput;

/// Establishes the selected library root from captured metadata once per inventory.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    selected: &[super::SelectedFile<'_>],
) -> super::Anchors {
    let inventory = selected
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    if inventory.len() != selected.len() {
        return BTreeMap::new();
    }
    let originals = input
        .files()
        .iter()
        .map(|file| (file.path().as_str(), file.text()))
        .collect::<BTreeMap<_, _>>();
    let libraries = match input.owner() {
        SymbolOwner::Package { .. } => library(input).into_iter().collect(),
        SymbolOwner::Runtime { runtime, .. } if runtime == "rust" => libraries(input, &inventory),
        SymbolOwner::Local | SymbolOwner::NamedLocal { .. } => libraries(input, &inventory),
        SymbolOwner::Runtime { .. } => Vec::new(),
    };
    let mut assigned = BTreeMap::new();
    let mut pending = VecDeque::new();
    for (root, name) in libraries {
        let namespace = vec![name];
        if SymbolIdentity::new(
            input.owner().clone(),
            input.language().clone(),
            namespace.clone(),
        )
        .is_err()
        {
            continue;
        }
        let directory = root
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_owned();
        if assigned.insert(root.clone(), namespace.clone()).is_some() {
            return BTreeMap::new();
        }
        pending.push_back((root, namespace, directory));
    }
    let mut anchors = BTreeMap::new();
    while let Some((path, namespace, directory)) = pending.pop_front() {
        let Some(file) = inventory.get(path.as_str()) else {
            continue;
        };
        if originals.get(path.as_str()).copied() != Some(file.source)
            || !source_is_valid(file.syntax, file.source)
        {
            continue;
        }
        anchors.insert(
            path.clone(),
            file_anchors(input, file.syntax, &namespace, file.source),
        );
        for (child, local) in module_children(file.syntax, &directory, &inventory) {
            let mut child_namespace = namespace.clone();
            child_namespace.extend(local);
            if SymbolIdentity::new(
                input.owner().clone(),
                file.syntax.language().clone(),
                child_namespace.clone(),
            )
            .is_err()
            {
                continue;
            }
            if let Some(previous) = assigned.get(&child) {
                if previous != &child_namespace {
                    return BTreeMap::new();
                }
                continue;
            }
            assigned.insert(child.clone(), child_namespace.clone());
            let child_directory = child
                .strip_suffix("/mod.rs")
                .or_else(|| child.strip_suffix(".rs"))
                .unwrap_or(&child)
                .to_owned();
            pending.push_back((child, child_namespace, child_directory));
        }
    }
    anchors
}

fn source_is_valid(syntax: &SyntaxFacts, source: &str) -> bool {
    syntax.language().name == "rust"
        && syntax.language().dialect.is_none()
        && !syntax.has_errors()
        && syntax.source_digest() == Some(&FileDigest::of(source.as_bytes()))
}

fn module_has_default_path(symbol: &rift_syntax::SyntaxSymbol) -> bool {
    symbol.kind == "module"
        && matches!(symbol.node_kind, None | Some("mod_item"))
        && symbol.module_path == Some(rift_syntax::RustModulePath::Absent)
}

fn module_children(
    syntax: &SyntaxFacts,
    directory: &str,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
) -> Vec<(String, Vec<String>)> {
    let symbols = syntax
        .symbols()
        .iter()
        .map(|symbol| (symbol.qualified_name.as_str(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut children = Vec::new();
    for symbol in syntax.symbols() {
        if !module_has_default_path(symbol)
            || symbol.body_range.is_some()
            || !source_identifier(&symbol.name).is_some_and(|name| crate_name_is_valid(&name))
        {
            continue;
        }
        let mut local = symbol
            .container
            .as_deref()
            .map_or_else(Vec::new, |container| {
                container.split("::").map(str::to_owned).collect()
            });
        let mut prefix = String::new();
        if local.iter().any(|name| {
            if !prefix.is_empty() {
                prefix.push_str("::");
            }
            prefix.push_str(name);
            source_identifier(name).is_none()
                || !symbols.get(prefix.as_str()).is_some_and(|parent| {
                    module_has_default_path(parent) && parent.body_range.is_some()
                })
        }) {
            continue;
        }
        local.push(symbol.name.clone());
        if symbol.qualified_name != local.join("::") {
            continue;
        }
        let Some(local) = local
            .iter()
            .map(|name| source_identifier(name))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let relative = local.join("/");
        let stem = if directory.is_empty() {
            relative
        } else {
            format!("{directory}/{relative}")
        };
        let candidates = [format!("{stem}.rs"), format!("{stem}/mod.rs")];
        let admitted = candidates
            .iter()
            .filter(|path| {
                ProjectPath::new(path.as_str()).is_ok() && inventory.contains_key(path.as_str())
            })
            .collect::<Vec<_>>();
        if let [child] = admitted.as_slice() {
            children.push(((*child).clone(), local));
        }
    }
    children
}

fn file_anchors(
    input: &NamespaceInput<'_>,
    syntax: &SyntaxFacts,
    namespace: &[String],
    source: &str,
) -> BTreeMap<String, SymbolId> {
    let revision = crate::documentation::content_digest(source.as_bytes()).0;
    let mut counts = BTreeMap::<String, usize>::new();
    for symbol in syntax.symbols() {
        let name = match symbol.container.as_deref() {
            Some(container) => format!("{container}::{}", symbol.name),
            None => symbol.name.clone(),
        };
        *counts.entry(name).or_default() += 1;
    }
    let mut anchors = BTreeMap::new();
    for symbol in syntax.symbols() {
        let base = match symbol.container.as_deref() {
            Some(container) => format!("{container}::{}", symbol.name),
            None => symbol.name.clone(),
        };
        let occurrence = if symbol.qualified_name == base {
            None
        } else if let Some(number) = symbol.qualified_name.strip_prefix(&format!("{base}~")) {
            if counts.get(&base).copied().unwrap_or_default() < 2 {
                continue;
            }
            match number.parse::<u32>() {
                Ok(number) => Some(number),
                Err(_) => continue,
            }
        } else {
            continue;
        };
        let Some(names) = base
            .split("::")
            .map(source_identifier)
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut qualified_path = namespace.to_vec();
        qualified_path.extend(names);
        let Ok(mut identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            qualified_path,
        ) else {
            continue;
        };
        if let Some(number) = occurrence {
            let Ok(occurrence) = SymbolOccurrence::new(number, revision.clone()) else {
                continue;
            };
            let Ok(qualified) = identity.with_occurrence(occurrence) else {
                continue;
            };
            identity = qualified;
        }
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    anchors
}

fn library(input: &NamespaceInput<'_>) -> Option<(String, String)> {
    let SymbolOwner::Package {
        manager,
        name,
        version,
        ..
    } = input.owner()
    else {
        return None;
    };
    if manager != "cargo" {
        return None;
    }
    let mut library = None;
    for source in input.files().iter().chain(input.context_sources()) {
        let manifest = source.path().as_str();
        let parent = if manifest == "Cargo.toml" {
            ""
        } else if let Some(parent) = manifest.strip_suffix("/Cargo.toml") {
            parent
        } else {
            continue;
        };
        if source.text().len() > input.syntax().source_bytes_max() {
            return None;
        }
        let metadata: CargoMetadata = toml::from_str(source.text()).ok()?;
        let package = metadata.package?;
        if package.name != *name || package.version != *version {
            continue;
        }
        if package.autolib == Some(false) && metadata.lib.is_none() {
            continue;
        }
        let explicit = metadata.lib.unwrap_or_default();
        let relative = explicit.path.as_deref().unwrap_or("src/lib.rs");
        let relative = ProjectPath::new(relative).ok()?;
        if relative.as_str().is_empty() {
            return None;
        }
        let root = if parent.is_empty() {
            relative.as_str().to_owned()
        } else {
            format!("{parent}/{}", relative.as_str())
        };
        let root = ProjectPath::new(root).ok()?;
        let name = explicit
            .name
            .unwrap_or_else(|| package.name.replace('-', "_"));
        if !crate_name_is_valid(&name)
            || library.replace((root.as_str().to_owned(), name)).is_some()
        {
            return None;
        }
    }
    library
}

/// Retains the selected owner while captured Cargo targets establish individual crate roots.
fn libraries(
    input: &NamespaceInput<'_>,
    selected: &BTreeMap<&str, &super::SelectedFile<'_>>,
) -> Vec<(String, String)> {
    let mut libraries = BTreeMap::new();
    for source in input.files().iter().chain(input.context_sources()) {
        let manifest = source.path().as_str();
        let parent = if manifest == "Cargo.toml" {
            ""
        } else if let Some(parent) = manifest.strip_suffix("/Cargo.toml") {
            parent
        } else {
            continue;
        };
        if source.text().len() > input.syntax().source_bytes_max() {
            continue;
        }
        let Ok(metadata) = toml::from_str::<RuntimeCargoMetadata>(source.text()) else {
            continue;
        };
        let Some(package) = metadata.package else {
            continue;
        };
        let enabled = package.autolib != Some(false) || metadata.lib.is_some();
        let library = metadata.lib.unwrap_or_default();
        let name = library
            .name
            .unwrap_or_else(|| package.name.replace('-', "_"));
        if !crate_name_is_valid(&name) {
            continue;
        }
        let root = enabled
            .then(|| library.path.as_deref().unwrap_or("src/lib.rs"))
            .and_then(|path| ProjectPath::new(path).ok())
            .filter(|path| !path.as_str().is_empty())
            .and_then(|relative| {
                let path = if parent.is_empty() {
                    relative.as_str().to_owned()
                } else {
                    format!("{parent}/{}", relative.as_str())
                };
                ProjectPath::new(path).ok()
            })
            .filter(|root| selected.contains_key(root.as_str()))
            .map(|root| root.as_str().to_owned());
        match libraries.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(root);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }
    let mut roots = BTreeMap::new();
    for root in libraries.values().flatten() {
        *roots.entry(root.clone()).or_insert(0_usize) += 1;
    }
    libraries
        .into_iter()
        .filter_map(|(name, root)| {
            let root = root?;
            (roots.get(&root) == Some(&1)).then_some((root, name))
        })
        .collect()
}

fn source_identifier(name: &str) -> Option<String> {
    let name = if let Some(raw) = name.strip_prefix("r#") {
        if ["_", "crate", "self", "Self", "super"].contains(&raw) {
            return None;
        }
        raw
    } else {
        name
    };
    if name
        .chars()
        .any(|character| matches!(character, '\u{200c}' | '\u{200d}'))
    {
        return None;
    }
    let name = name.nfc().collect::<String>();
    python_identifier_is_valid(&name).then_some(name)
}

fn crate_name_is_valid(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Deserialize)]
struct CargoMetadata {
    package: Option<CargoPackage>,
    lib: Option<CargoLibrary>,
}

#[derive(Deserialize)]
struct RuntimeCargoMetadata {
    package: Option<RuntimeCargoPackage>,
    lib: Option<CargoLibrary>,
}

#[derive(Deserialize)]
struct RuntimeCargoPackage {
    name: String,
    autolib: Option<bool>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    autolib: Option<bool>,
}

#[derive(Default, Deserialize)]
struct CargoLibrary {
    name: Option<String>,
    path: Option<String>,
}

#[cfg(test)]
mod tests;
