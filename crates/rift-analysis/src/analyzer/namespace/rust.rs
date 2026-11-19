//! Rust library namespaces established by captured Cargo metadata.

use std::collections::{BTreeMap, VecDeque};

use rift_core::{FileDigest, ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::python_identifier_is_valid;
use rift_syntax::{ShippedLanguage, SyntaxFacts, SyntaxLimits};
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use crate::NamespaceInput;

mod macros;

/// Establishes the selected library root from captured metadata once per inventory.
pub(super) fn prepare(
    input: &NamespaceInput<'_>,
    selected: &[super::SelectedFile<'_>],
) -> super::PreparedNamespace {
    let inventory = selected
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    if inventory.len() != selected.len() {
        return super::PreparedNamespace::default();
    }
    let originals = original_sources(input);
    let libraries = selected_libraries(input, &inventory);
    let mut assigned = BTreeMap::new();
    let mut pending = VecDeque::new();
    for (root, name) in libraries {
        let Some((namespace, directory, library)) = library_root(input, &root, name) else {
            continue;
        };
        if assigned.insert(root.clone(), namespace.clone()).is_some() {
            return super::PreparedNamespace::default();
        }
        pending.push_back((
            root,
            namespace,
            directory,
            macros::Scope::default(),
            library,
        ));
    }
    let mut anchors = BTreeMap::new();
    let mut requested = Vec::new();
    while let Some((path, namespace, directory, scope, library)) = pending.pop_front() {
        let Some(file) = inventory.get(path.as_str()) else {
            continue;
        };
        if originals.get(path.as_str()).copied() != Some(file.source)
            || !source_is_valid(file.syntax, file.source)
        {
            continue;
        }
        let mut admitted = std::collections::BTreeSet::new();
        let mut exports = Vec::new();
        let children = macros::children(
            file,
            &directory,
            &inventory,
            &originals,
            (scope, &mut admitted, &mut exports),
            input.syntax(),
            library,
        );
        requested.extend(
            exports
                .into_iter()
                .map(|export| (namespace.clone(), export)),
        );
        let declared = file_anchors(input, file.syntax, &namespace, &admitted);
        anchors.insert(
            path.clone(),
            super::retain_valid_declarations(input, file, declared),
        );
        for (child, local, child_scope) in children {
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
                    return super::PreparedNamespace::default();
                }
                continue;
            }
            assigned.insert(child.clone(), child_namespace.clone());
            let child_directory = child
                .strip_suffix("/mod.rs")
                .or_else(|| child.strip_suffix(".rs"))
                .unwrap_or(&child)
                .to_owned();
            pending.push_back((
                child,
                child_namespace,
                child_directory,
                child_scope,
                library,
            ));
        }
    }
    let mut prepared = super::PreparedNamespace {
        anchors,
        outside_namespaces: target_paths(input, &inventory, &originals, &assigned),
        ..Default::default()
    };
    local_exports(input, selected, &requested, &mut prepared);
    prepared
}

/// Original paths of the Cargo targets other than a library, and of the modules they
/// declare, that no library namespace reaches.
///
/// A library namespace is the only Rust namespace that establishes identities, so a
/// declaration in one of these files carries none by construction. A file no target
/// reaches stays outside this set: its declarations may belong to a library module that
/// a file which failed analysis, or a module declaration no path resolves, hides.
fn target_paths(
    input: &NamespaceInput<'_>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
    originals: &BTreeMap<&str, &str>,
    assigned: &BTreeMap<String, Vec<String>>,
) -> std::collections::BTreeSet<String> {
    let mut reached = std::collections::BTreeSet::new();
    let mut pending = target_roots(input, inventory)
        .into_iter()
        .map(|root| {
            let directory = root
                .rsplit_once('/')
                .map_or("", |(parent, _)| parent)
                .to_owned();
            (root, directory)
        })
        .collect::<VecDeque<_>>();
    while let Some((path, directory)) = pending.pop_front() {
        let Some(file) = inventory.get(path.as_str()) else {
            continue;
        };
        if !reached.insert(path.clone())
            || originals.get(path.as_str()).copied() != Some(file.source)
            || !source_is_valid(file.syntax, file.source)
        {
            continue;
        }
        for (child, _) in module_children(file, &directory, inventory, input.syntax()) {
            let child_directory = child
                .strip_suffix("/mod.rs")
                .or_else(|| child.strip_suffix(".rs"))
                .unwrap_or(&child)
                .to_owned();
            pending.push_back((child, child_directory));
        }
    }
    reached.retain(|path| !assigned.contains_key(path));
    reached
}

/// Crate roots of the captured Cargo targets other than a library: the build script,
/// binaries, examples, integration tests, and benchmarks at their conventional paths or
/// at the `path` a manifest names.
fn target_roots(
    input: &NamespaceInput<'_>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
) -> std::collections::BTreeSet<String> {
    const SECTIONS: [&str; 4] = ["bin", "example", "test", "bench"];
    const DIRECTORIES: [&str; 4] = ["src/bin", "examples", "tests", "benches"];
    let mut roots = std::collections::BTreeSet::new();
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
        let Ok(metadata) = toml::from_str::<toml::Table>(source.text()) else {
            continue;
        };
        let Some(package) = metadata.get("package").and_then(toml::Value::as_table) else {
            continue;
        };
        let joined = |relative: &str| {
            ProjectPath::new(relative)
                .ok()
                .filter(|relative| !relative.as_str().is_empty())
                .map(|relative| {
                    if parent.is_empty() {
                        relative.as_str().to_owned()
                    } else {
                        format!("{parent}/{}", relative.as_str())
                    }
                })
        };
        let explicit = SECTIONS
            .iter()
            .filter_map(|section| metadata.get(*section).and_then(toml::Value::as_array))
            .flatten()
            .filter_map(|target| target.get("path").and_then(toml::Value::as_str))
            .chain(package.get("build").and_then(toml::Value::as_str))
            .chain(["build.rs", "src/main.rs"]);
        roots.extend(explicit.filter_map(joined));
        for directory in DIRECTORIES {
            let Some(directory) = joined(directory) else {
                continue;
            };
            let prefix = format!("{directory}/");
            roots.extend(
                inventory
                    .range(prefix.as_str()..)
                    .map(|(path, _)| *path)
                    .take_while(|path| path.starts_with(&prefix))
                    .filter(|path| {
                        let relative = &path[prefix.len()..];
                        relative
                            .strip_suffix(".rs")
                            .is_some_and(|stem| !stem.contains('/'))
                            || relative
                                .strip_suffix("/main.rs")
                                .is_some_and(|stem| !stem.contains('/'))
                    })
                    .map(|path| (*path).to_owned()),
            );
        }
    }
    roots.retain(|root| inventory.contains_key(root.as_str()));
    roots
}

fn library_root(
    input: &NamespaceInput<'_>,
    root: &str,
    name: String,
) -> Option<(Vec<String>, String, bool)> {
    let namespace = vec![name];
    if SymbolIdentity::new(
        input.owner().clone(),
        ShippedLanguage::Rust.language(),
        namespace.clone(),
    )
    .is_err()
    {
        return None;
    }
    let directory = root
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent)
        .to_owned();
    let library = input
        .library()
        .is_some_and(|(_, selected)| selected.path().as_str() == root);
    Some((namespace, directory, library))
}

fn original_sources<'a>(input: &NamespaceInput<'a>) -> BTreeMap<&'a str, &'a str> {
    input
        .files()
        .iter()
        .map(|file| (file.path().as_str(), file.text()))
        .collect()
}

fn selected_libraries(
    input: &NamespaceInput<'_>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
) -> Vec<(String, String)> {
    match input.owner() {
        SymbolOwner::Package { .. } => library(input).into_iter().collect(),
        SymbolOwner::Runtime { runtime, .. } if runtime == "rust" => libraries(input, inventory),
        SymbolOwner::Local
        | SymbolOwner::NamedLocal { .. }
        | SymbolOwner::ProfileLocal { .. }
        | SymbolOwner::NamedProfileLocal { .. } => libraries(input, inventory),
        SymbolOwner::Runtime { .. } => Vec::new(),
    }
}

fn source_is_valid(syntax: &SyntaxFacts, source: &str) -> bool {
    syntax.language().name == "rust"
        && syntax.language().dialect.is_none()
        && syntax.source_digest() == Some(&FileDigest::of(source.as_bytes()))
}

fn module_has_default_path(symbol: &rift_syntax::SyntaxSymbol) -> bool {
    symbol.kind == "module"
        && matches!(symbol.syntax_element_kind, None | Some("mod_item"))
        && symbol.module_path == Some(rift_syntax::RustModulePath::Absent)
}

fn module_children(
    file: &super::SelectedFile<'_>,
    directory: &str,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
    limits: SyntaxLimits,
) -> Vec<(String, Vec<String>)> {
    module_paths(
        file,
        directory,
        &|path| inventory.contains_key(path),
        limits,
    )
}

fn module_paths(
    file: &super::SelectedFile<'_>,
    directory: &str,
    contains: &dyn Fn(&str) -> bool,
    limits: SyntaxLimits,
) -> Vec<(String, Vec<String>)> {
    let Some(invalid) = super::errors::invalid_declarations(file, limits) else {
        return Vec::new();
    };
    let syntax = file.syntax;
    let symbols = syntax
        .symbols()
        .iter()
        .map(|symbol| (symbol.qualified_name.as_str(), symbol))
        .collect::<BTreeMap<_, _>>();
    let mut children = Vec::new();
    for symbol in syntax.symbols() {
        if symbol.kind != "module"
            || invalid.contains(&symbol.item_range)
            || !matches!(symbol.syntax_element_kind, None | Some("mod_item"))
            || !matches!(
                symbol.module_path,
                Some(
                    rift_syntax::RustModulePath::Absent
                        | rift_syntax::RustModulePath::Literal { .. }
                )
            )
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
        let candidates =
            if let Some(rift_syntax::RustModulePath::Literal { path, .. }) = &symbol.module_path {
                let parent = if local.len() == 1 {
                    file.path
                        .as_str()
                        .rsplit_once('/')
                        .map_or("", |(parent, _)| parent)
                        .to_owned()
                } else {
                    let inline = local[..local.len() - 1].join("/");
                    if directory.is_empty() {
                        inline
                    } else {
                        format!("{directory}/{inline}")
                    }
                };
                vec![if parent.is_empty() {
                    path.as_str().to_owned()
                } else {
                    format!("{parent}/{}", path.as_str())
                }]
            } else {
                let relative = local.join("/");
                let stem = if directory.is_empty() {
                    relative
                } else {
                    format!("{directory}/{relative}")
                };
                vec![format!("{stem}.rs"), format!("{stem}/mod.rs")]
            };
        let admitted = candidates
            .iter()
            .filter(|path| ProjectPath::new(path.as_str()).is_ok() && contains(path.as_str()))
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
    admitted: &std::collections::BTreeSet<rift_syntax::ByteRange>,
) -> BTreeMap<String, SymbolId> {
    let supported = |symbol: &rift_syntax::SyntaxSymbol| {
        let Some(invocations) = syntax.macro_invocations() else {
            return true;
        };
        let index =
            invocations.partition_point(|(range, _, _)| range.start <= symbol.item_range.start);
        index
            .checked_sub(1)
            .and_then(|index| invocations.get(index))
            .is_none_or(|(range, _, _)| {
                symbol.item_range.end > range.end || admitted.contains(range)
            })
    };
    let mut anchors = BTreeMap::new();
    for symbol in syntax.symbols().iter().filter(|symbol| supported(symbol)) {
        let base = match symbol.container.as_deref() {
            Some(container) => format!("{container}::{}", symbol.name),
            None => symbol.name.clone(),
        };
        if symbol.qualified_name != base {
            continue;
        }
        let Some(names) = base
            .split("::")
            .map(source_identifier)
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let mut qualified_path = namespace.to_vec();
        qualified_path.extend(names);
        let Ok(identity) = SymbolIdentity::new(
            input.owner().clone(),
            syntax.language().clone(),
            qualified_path,
        ) else {
            continue;
        };
        if let Ok(identity) = SymbolId::new(identity.wire_identity()) {
            anchors.insert(symbol.qualified_name.clone(), identity);
        }
    }
    anchors
}

pub(super) fn library(input: &NamespaceInput<'_>) -> Option<(String, String)> {
    let SymbolOwner::Package { manager, name, .. } = input.owner() else {
        return None;
    };
    let version = input.release_version()?;
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
    library_roots(input, &|path| selected.contains_key(path))
}

fn library_roots(
    input: &NamespaceInput<'_>,
    contains: &dyn Fn(&str) -> bool,
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
            .filter(|root| contains(root.as_str()))
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

// Resolves only original public bindings against this inventory's admitted declarations.
fn local_exports(
    input: &NamespaceInput<'_>,
    selected: &[super::SelectedFile<'_>],
    requested: &[(Vec<String>, macros::Export)],
    prepared: &mut super::PreparedNamespace,
) {
    let Some(work_max) = input
        .syntax()
        .syntax_elements_max()
        .checked_mul(input.syntax().syntax_depth_max())
    else {
        prepared.unresolved_exports = !requested.is_empty();
        return;
    };
    let mut work = 0usize;
    // Both are built on first use and read by every later binding: visiting every
    // declaration and every identity again per binding made this pass quadratic in
    // the crate's public imports.
    let mut declarations = None;
    let mut anchored = None;
    for (namespace, (path, range, name, target)) in requested {
        // Captured conditional declarations do not establish export completeness.
        prepared.unresolved_exports = true;
        let local = requested_path(namespace, target).and_then(|target_path| {
            let declarations = declarations.get_or_insert_with(|| {
                local_declarations(selected, &prepared.anchors, &mut work, work_max)
            });
            local_target(
                input,
                declarations.as_ref()?,
                &target_path,
                &mut work,
                work_max,
            )
            .map(|(file, symbol, target)| {
                (
                    target.clone(),
                    file.path.as_str().to_owned(),
                    symbol.qualified_name.clone(),
                )
            })
        });
        let target = match local {
            Some(local) => Some(local),
            None if is_path_keyword(target) => None,
            None => external_target(input, target, &mut work, work_max),
        };
        let Some((target, target_path, target_qualified_name)) = target else {
            continue;
        };
        let mut alias_path = namespace.clone();
        alias_path.push(name.clone());
        let Some(identity) = SymbolIdentity::new(
            input.owner().clone(),
            ShippedLanguage::Rust.language(),
            alias_path,
        )
        .ok()
        .and_then(|identity| SymbolId::new(identity.wire_identity()).ok()) else {
            continue;
        };
        if identity == target {
            continue;
        }
        if anchored
            .get_or_insert_with(|| {
                prepared
                    .anchors
                    .values()
                    .flat_map(BTreeMap::values)
                    .map(SymbolId::as_str)
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .contains(identity.as_str())
        {
            continue;
        }
        prepared.exports.push(super::ExportBinding {
            path: path.as_str().to_owned(),
            binding: rift_syntax::SyntaxExportBinding {
                kind: rift_syntax::SyntaxExportKind::Named,
                range: *range,
                local: None,
                exported: None,
                source: None,
                container: None,
                type_only: false,
            },
            identity,
            target,
            target_path,
            target_qualified_name,
            name: name.clone(),
            qualified_name: name.clone(),
        });
    }
    let conflicts = super::typescript::conflicting_export_identities(&prepared.exports);
    prepared
        .exports
        .retain(|export| !conflicts.contains(export.identity.as_str()));
}

fn external_target(
    input: &NamespaceInput<'_>,
    path: &[String],
    work: &mut usize,
    work_max: usize,
) -> Option<(SymbolId, String, String)> {
    let mut found = None;
    for dependency in input.dependencies() {
        *work = work.checked_add(1)?;
        if *work > work_max {
            return None;
        }
        if dependency.name() != path.first()?.as_str() {
            continue;
        }
        let (identity, declaration, unit) =
            dependency.target(path, &ShippedLanguage::Rust.language(), work, work_max)?;
        let name = SymbolIdentity::parse(identity.as_str())
            .ok()?
            .qualified_path()
            .last()?
            .clone();
        if found
            .replace((identity, unit.path.0.clone(), name))
            .is_some()
            || declaration.unit != unit.unit
        {
            return None;
        }
    }
    found
}

/// The crate-local path a `use` target names from the module at
/// `namespace`: `crate::`, `self::`, and `super::` read as written, and a
/// path starting at any other name starts in the current module, where a
/// child module or an item of that name resolves it before an external
/// crate does.
fn requested_path(namespace: &[String], target: &[String]) -> Option<Vec<String>> {
    let (head, tail) = target.split_first()?;
    let mut path = match head.as_str() {
        "crate" => vec![namespace.first()?.clone()],
        "self" => namespace.to_vec(),
        "super" if namespace.len() > 1 => namespace[..namespace.len() - 1].to_vec(),
        "super" => return None,
        _ => {
            let mut path = namespace.to_vec();
            path.push(head.clone());
            path
        }
    };
    path.extend(tail.iter().cloned());
    Some(path)
}

/// Whether a `use` target starts with a path keyword, which never names an
/// external crate.
fn is_path_keyword(target: &[String]) -> bool {
    target
        .first()
        .is_some_and(|head| matches!(head.as_str(), "crate" | "self" | "super"))
}

/// One admitted declaration: its file, its syntax facts, and its identity.
type LocalDeclaration<'input> = (
    &'input super::SelectedFile<'input>,
    &'input rift_syntax::SyntaxSymbol,
    &'input SymbolId,
);

/// Every admitted declaration of the inventory by identity, in selection order. Each
/// declaration is visited once and counts one unit of `work`; `None` once `work`
/// passes `work_max`.
fn local_declarations<'input>(
    selected: &'input [super::SelectedFile<'input>],
    anchors: &'input super::Anchors,
    work: &mut usize,
    work_max: usize,
) -> Option<BTreeMap<&'input str, Vec<LocalDeclaration<'input>>>> {
    let mut declarations = BTreeMap::<&str, Vec<_>>::new();
    for file in selected {
        let Some(anchors) = anchors.get(file.path.as_str()) else {
            continue;
        };
        for symbol in file.syntax.symbols() {
            if symbol.kind == "impl_item" {
                continue;
            }
            *work = work.checked_add(1)?;
            if *work > work_max {
                return None;
            }
            if let Some(identity) = anchors.get(&symbol.qualified_name) {
                declarations
                    .entry(identity.as_str())
                    .or_default()
                    .push((file, symbol, identity));
            }
        }
    }
    Some(declarations)
}

/// The one public declaration `path` names among the admitted declarations; `None`
/// when no declaration or more than one carries that identity, or when it is not
/// `pub` or its range lies outside its source.
fn local_target<'input>(
    input: &NamespaceInput<'_>,
    declarations: &BTreeMap<&'input str, Vec<LocalDeclaration<'input>>>,
    path: &[String],
    work: &mut usize,
    work_max: usize,
) -> Option<LocalDeclaration<'input>> {
    let requested = SymbolIdentity::new(
        input.owner().clone(),
        ShippedLanguage::Rust.language(),
        path.to_vec(),
    )
    .ok()?
    .wire_identity();
    *work = work.checked_add(1)?;
    if *work > work_max {
        return None;
    }
    let [(file, symbol, identity)] = declarations.get(requested.as_str())?.as_slice() else {
        return None;
    };
    let spelled = file
        .source
        .get(usize::try_from(symbol.range.start).ok()?..usize::try_from(symbol.range.end).ok()?)
        .is_some();
    (symbol.visibility.as_deref() == Some("pub") && spelled).then_some((*file, *symbol, *identity))
}

#[cfg(test)]
mod tests;

pub(super) fn namespace_paths(
    input: &NamespaceInput<'_>,
    documents: &[&crate::IndexedFile],
    contains: &dyn Fn(&str) -> bool,
    selected: &std::collections::BTreeSet<ProjectPath>,
    files_max: usize,
) -> Result<std::collections::BTreeSet<ProjectPath>, rift_error::RiftError> {
    let mut paths = std::collections::BTreeSet::new();
    let reaches = |path: &str, directory: &str| {
        selected.iter().any(|selected| {
            selected.as_str() == path
                || directory.is_empty()
                || selected
                    .as_str()
                    .strip_prefix(directory)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        })
    };
    let mut insert = |path: String| -> Result<(), rift_error::RiftError> {
        let Ok(path) = ProjectPath::new(path) else {
            return Ok(());
        };
        if !paths.contains(&path) && paths.len() >= files_max {
            return rift_error::errors::analysis::package_input_too_many_files()
                .field("package_files_max")
                .bound(u64::try_from(files_max).unwrap_or(u64::MAX))
                .observed(u64::try_from(paths.len().saturating_add(1)).unwrap_or(u64::MAX))
                .fail();
        }
        paths.insert(path);
        Ok(())
    };
    let roots = library_roots(input, contains);
    let root_paths = roots
        .iter()
        .map(|(path, _)| path.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for (path, _) in &roots {
        let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        if reaches(path, directory) {
            insert(path.clone())?;
        }
    }
    for document in documents {
        if document.syntax().language().name != "rust" {
            continue;
        }
        let path = document.path().as_str();
        let directory = if let Some(directory) = path.strip_suffix("/mod.rs") {
            directory
        } else if root_paths.contains(path) {
            path.rsplit_once('/').map_or("", |(directory, _)| directory)
        } else {
            path.strip_suffix(".rs").unwrap_or(path)
        };
        for (path, _) in module_paths(
            &super::SelectedFile {
                path: document.path(),
                source: document.source(),
                syntax: document.syntax(),
            },
            directory,
            contains,
            input.syntax(),
        ) {
            let directory = path
                .strip_suffix("/mod.rs")
                .or_else(|| path.strip_suffix(".rs"))
                .unwrap_or(&path);
            if reaches(&path, directory) {
                insert(path)?;
            }
        }
    }
    Ok(paths)
}
