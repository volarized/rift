//! Rust library namespaces established by captured Cargo metadata.

use std::collections::{BTreeMap, VecDeque};

use rift_core::{FileDigest, ProjectPath, SymbolId};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::index::python_identifier_is_valid;
use rift_protocol::read::Language;
use rift_syntax::{ShippedLanguage, SyntaxFacts, SyntaxLimits};
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use crate::NamespaceInput;

pub(super) mod configuration;
mod implementations;
mod macros;

pub(crate) use implementations::file_scope;

/// The modules the conditional item chains of one file declare, each with whether the
/// configuration admits its branch: admitted when one declaring branch is, inactive when
/// every one is not, unknown otherwise.
pub(crate) fn chain_modules<'a>(
    source: &'a str,
    syntax: &SyntaxFacts,
    configuration: Option<&configuration::Configuration<'_>>,
) -> BTreeMap<&'a str, Option<bool>> {
    let mut modules = BTreeMap::new();
    let Some(limits) = syntax.syntax_limits() else {
        return modules;
    };
    for (range, _, _) in syntax.macro_invocations().unwrap_or_default() {
        let Some(tokens) = macros::chain::token_texts(source, *range, limits) else {
            continue;
        };
        let Some(chain) = macros::chain::modules(&tokens, limits.syntax_depth_max()) else {
            continue;
        };
        for (name, condition) in chain {
            let admitted =
                configuration.and_then(|configuration| configuration.condition(&condition));
            modules
                .entry(name)
                .and_modify(|status| {
                    *status = match (*status, admitted) {
                        (Some(true), _) | (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    }
                })
                .or_insert(admitted);
        }
    }
    modules
}

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
    let Some((mut assigned, mut pending, root)) = library_roots_pending(input, &inventory) else {
        return super::PreparedNamespace::default();
    };
    let mut anchors = BTreeMap::new();
    let mut requested = Vec::new();
    let configurations = configuration::Configurations::new(input);
    let mut declaring = BTreeMap::<String, Vec<(String, Declaring)>>::new();
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
        let mut conditioned = Vec::new();
        let children = macros::children(
            file,
            &directory,
            &inventory,
            &originals,
            (scope, &mut admitted, &mut exports, &mut conditioned),
            input.syntax(),
            library,
        );
        requested.extend(
            exports
                .into_iter()
                .map(|export| (namespace.clone(), export)),
        );
        for (child, _, item) in module_children(file, &directory, &inventory, input.syntax()) {
            declaring
                .entry(child)
                .or_default()
                .push((path.clone(), Declaring::Item(item)));
        }
        let configuration = configurations.file(path.as_str());
        for (child, condition) in conditioned {
            declaring.entry(child).or_default().push((
                path.clone(),
                Declaring::Known(configuration.condition(&condition)),
            ));
        }
        let declared = file_anchors(input, file.syntax, &namespace, &admitted);
        anchors.insert(
            path.clone(),
            super::retain_valid_declarations(input, file, declared),
        );
        if !queue_module_children(
            input.owner(),
            file.syntax.language(),
            &namespace,
            library,
            children,
            &mut assigned,
            &mut pending,
        ) {
            return super::PreparedNamespace::default();
        }
    }
    let admitted = module_admission(&configurations, &inventory, &declaring, assigned.len());
    let modules = Modules {
        configurations: &configurations,
        admitted: &admitted,
    };
    implementations::place(input, selected, &assigned, &modules, &mut anchors);
    let rust_modules = assigned
        .iter()
        .filter(|(path, _)| anchors.contains_key(*path))
        .filter_map(|(path, module)| Some((path.clone(), module_identity(input, module)?)))
        .collect();
    let mut prepared = super::PreparedNamespace {
        anchors,
        rust_modules,
        outside_namespaces: target_paths(input, &inventory, &originals, &assigned),
        ..Default::default()
    };
    local_exports(input, selected, &requested, &admitted, &mut prepared);
    retain_root_anchor(input, root, &inventory, &assigned, &mut prepared);
    prepared
}

/// Queues children with valid namespaces. A competing assignment refuses the inventory.
fn queue_module_children(
    owner: &SymbolOwner,
    language: &Language,
    parent: &[String],
    library: bool,
    children: Vec<(String, Vec<String>, macros::Scope)>,
    assigned: &mut BTreeMap<String, Vec<String>>,
    pending: &mut Pending,
) -> bool {
    for (child, local, child_scope) in children {
        let mut child_namespace = parent.to_vec();
        child_namespace.extend(local);
        if SymbolIdentity::new(owner.clone(), language.clone(), child_namespace.clone()).is_err() {
            continue;
        }
        if let Some(previous) = assigned.get(&child) {
            if previous != &child_namespace {
                return false;
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
    true
}

fn module_identity(input: &NamespaceInput<'_>, module: &[String]) -> Option<SymbolId> {
    let identity = SymbolIdentity::new(
        input.owner().clone(),
        ShippedLanguage::Rust.language(),
        module.to_vec(),
    )
    .ok()?;
    SymbolId::new(identity.wire_identity()).ok()
}

/// Retains the accepted Cargo root namespace for package imports.
fn retain_root_anchor(
    input: &NamespaceInput<'_>,
    root: Option<String>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
    assigned: &BTreeMap<String, Vec<String>>,
    prepared: &mut super::PreparedNamespace,
) {
    if let Some(path) = root
        && let Some(file) = inventory.get(path.as_str())
        && !file.syntax.has_errors()
        && file.syntax.left_out_declaration_count() == 0
        && prepared.anchors.contains_key(&path)
        && let Some(namespace) = assigned.get(&path)
        && let Ok(identity) = SymbolIdentity::new(
            input.owner().clone(),
            file.syntax.language().clone(),
            namespace.clone(),
        )
        && let Ok(identity) = SymbolId::new(identity.wire_identity())
    {
        prepared.rust_package_root = Some((path, identity));
    }
}

/// Whether the configuration admits each module file: its declaring `mod` items and every
/// ancestor's. A file several alternative items declare is admitted when one of them is;
/// a file reached only through inactive items is not; anything else unknown stays unknown.
/// Files no ordinary `mod` item or conditional item chain declares (crate roots, other
/// macro-produced modules) keep the admission of their own items.
fn module_admission(
    configurations: &configuration::Configurations<'_>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
    declaring: &BTreeMap<String, Vec<(String, Declaring)>>,
    depth_max: usize,
) -> BTreeMap<String, Option<bool>> {
    let mut admitted = BTreeMap::<String, Option<bool>>::new();
    // Each pass settles at least one more module depth; module files bound the passes.
    for _ in 0..=depth_max {
        let mut changed = false;
        for (child, items) in declaring {
            let mut status = Some(false);
            for (parent, item) in items {
                let parent_status = if declaring.contains_key(parent) {
                    let Some(parent_status) = admitted.get(parent) else {
                        status = None;
                        continue;
                    };
                    *parent_status
                } else {
                    Some(true)
                };
                let item_status = match item {
                    Declaring::Item(item) => inventory.get(parent.as_str()).and_then(|file| {
                        configurations
                            .file(parent)
                            .admitted(file.syntax, file.source, *item)
                    }),
                    Declaring::Known(status) => *status,
                };
                status = match (status, and(parent_status, item_status)) {
                    (_, Some(true)) | (Some(true), _) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                };
            }
            if admitted.get(child) != Some(&status) {
                admitted.insert(child.clone(), status);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    admitted
}

/// What declares a module file in its parent: an ordinary `mod` item, whose attributes the
/// configuration evaluates, or a conditional item chain branch with its evaluated condition.
enum Declaring {
    Item(rift_syntax::ByteRange),
    Known(Option<bool>),
}

fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
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
        for (child, _, _) in module_children(file, &directory, inventory, input.syntax()) {
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
        let joined = |relative: &str| target_path(parent, relative);
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

type Pending = VecDeque<(String, Vec<String>, String, macros::Scope, bool)>;
type LibraryRoots = (BTreeMap<String, Vec<String>>, Pending, Option<String>);

/// The selected library roots, assigned to their crate namespaces and queued for the
/// module walk. Two roots of one path refuse the whole inventory.
fn library_roots_pending(
    input: &NamespaceInput<'_>,
    inventory: &BTreeMap<&str, &super::SelectedFile<'_>>,
) -> Option<LibraryRoots> {
    let mut assigned = BTreeMap::new();
    let mut pending = VecDeque::new();
    for (root, name) in selected_libraries(input, inventory) {
        let Some((namespace, directory, library)) = library_root(input, &root, name) else {
            continue;
        };
        if assigned.insert(root.clone(), namespace.clone()).is_some() {
            return None;
        }
        pending.push_back((
            root,
            namespace,
            directory,
            macros::Scope::default(),
            library,
        ));
    }
    let package_root = pending
        .front()
        .filter(|_| matches!(input.owner(), SymbolOwner::Package { .. }))
        .map(|(path, ..)| path.clone());
    Some((assigned, pending, package_root))
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
) -> Vec<(String, Vec<String>, rift_syntax::ByteRange)> {
    module_paths(
        file,
        directory,
        &|path| inventory.contains_key(path),
        limits,
    )
}

/// Child module files with their logical names and the declaring `mod` item's range.
///
/// Alternative declarations of one module, such as `#[cfg]`-selected `#[path]` files,
/// each keep their own entry.
fn module_paths(
    file: &super::SelectedFile<'_>,
    directory: &str,
    contains: &dyn Fn(&str) -> bool,
    limits: SyntaxLimits,
) -> Vec<(String, Vec<String>, rift_syntax::ByteRange)> {
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
            children.push(((*child).clone(), local, symbol.item_range));
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
    let mut owners = BTreeMap::<String, Vec<String>>::new();
    for symbol in syntax.symbols().iter().filter(|symbol| supported(symbol)) {
        let base = match symbol.container.as_deref() {
            Some(container) => format!("{container}::{}", symbol.name),
            None => symbol.name.clone(),
        };
        if symbol.qualified_name != base {
            continue;
        }
        let structural = symbol.kind == "impl_item" && symbol.name.starts_with(['&', '[', '(']);
        let names = if structural {
            let mut names = match symbol.container.as_deref() {
                Some(container) => owner_path(container, &owners),
                None => Some(Vec::new()),
            };
            if let Some(names) = &mut names {
                names.push(symbol.name.nfc().collect());
            }
            names
        } else {
            owner_path(&base, &owners)
        };
        let Some(names) = names else {
            continue;
        };
        if structural {
            owners.insert(base, names.clone());
        }
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

fn owner_path(base: &str, owners: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    let mut prefix = base;
    // Each step removes a path component; structural owners stay one component.
    loop {
        if let Some(owner) = owners.get(prefix) {
            let mut names = owner.clone();
            if let Some(suffix) = base.get(prefix.len()..)?.strip_prefix("::") {
                names.extend(
                    suffix
                        .split("::")
                        .map(source_identifier)
                        .collect::<Option<Vec<_>>>()?,
                );
            }
            return Some(names);
        }
        let Some((parent, _)) = prefix.rsplit_once("::") else {
            return base.split("::").map(source_identifier).collect();
        };
        prefix = parent;
    }
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
        let root = target_path(parent, relative)?;
        let name = crate_name(package.name, explicit.name);
        if !crate_name_is_valid(&name) || library.replace((root, name)).is_some() {
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
        let name = crate_name(package.name, library.name);
        if !crate_name_is_valid(&name) {
            continue;
        }
        let root = enabled
            .then(|| library.path.as_deref().unwrap_or("src/lib.rs"))
            .and_then(|path| target_path(parent, path))
            .filter(|root| contains(root));
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

/// The original path of a target a manifest in `parent` names, where `.` names the folder it
/// stands in.
fn target_path(parent: &str, relative: &str) -> Option<String> {
    let relative = relative
        .split('/')
        .filter(|part| *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    let relative = ProjectPath::new(relative).ok()?;
    match (parent, relative.as_str()) {
        (_, "") => None,
        ("", relative) => Some(relative.to_owned()),
        (parent, relative) => {
            let path = ProjectPath::new(format!("{parent}/{relative}")).ok()?;
            Some(path.as_str().to_owned())
        }
    }
}

/// The crate name Cargo derives from a library target, which is named after its package when
/// the manifest names none.
///
/// Manifests published before RFC 940 spell a library name with hyphens.
fn crate_name(package: String, library: Option<String>) -> String {
    library.unwrap_or(package).replace('-', "_")
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
    admitted: &BTreeMap<String, Option<bool>>,
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
    let configurations = configuration::Configurations::new(input);
    let files = selected
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let modules = Modules {
        configurations: &configurations,
        admitted,
    };
    let refused = conflicting_requests(input, &files, requested, &modules);
    // Build declaration and identity lookups once for every later binding and pass.
    let mut declarations = None;
    let mut export_paths = ExportPaths::default();
    let mut completed = std::collections::BTreeSet::new();
    'passes: for _ in 0..requested.len() {
        let before = prepared.exports.len();
        for request in requested {
            let (namespace, (path, range, name, _, _)) = request;
            let Some(next) = work.checked_add(1) else {
                break 'passes;
            };
            work = next;
            if work > work_max {
                break 'passes;
            }
            let Some(identity) = export_identity(input, namespace, name) else {
                continue;
            };
            if completed.contains(&identity) || refused.contains(&identity) {
                continue;
            }
            let Some(file) = files.get(path.as_str()) else {
                continue;
            };
            if modules.admitted(file, *range) != Some(true) {
                continue;
            }
            // Captured conditional declarations do not establish export completeness.
            prepared.unresolved_exports = true;
            let declarations = declarations.get_or_insert_with(|| {
                local_declarations(selected, &prepared.anchors, &modules, &mut work, work_max)
            });
            let Some(declarations) = declarations else {
                break 'passes;
            };
            let visibility = request
                .1
                .4
                .and_then(|range| macros::text_range(file.source, range));
            let target = export_target(
                input,
                declarations,
                (prepared, &mut export_paths),
                (&modules, visibility),
                request,
                &mut work,
                work_max,
            );
            let Some((target, target_path, target_qualified_name)) = target else {
                continue;
            };
            if identity == target {
                continue;
            }
            if declarations.identities.contains(identity.as_str()) {
                continue;
            }
            if let Some(range) = request.1.4 {
                let Some(visibility) = macros::text_range(file.source, range) else {
                    continue;
                };
                prepared
                    .alias_visibilities
                    .insert(identity.clone(), visibility.to_owned());
            }
            completed.insert(identity.clone());
            prepared.exports.push(export_binding(
                request,
                identity,
                (target, target_path, target_qualified_name),
            ));
        }
        if prepared.exports.len() == before {
            break;
        }
    }
    let conflicts = super::typescript::conflicting_export_identities(&prepared.exports);
    prepared
        .exports
        .retain(|export| !conflicts.contains(export.identity.as_str()));
}

/// Configuration admission of captured items, including their module file's admission.
struct Modules<'configuration, 'input> {
    configurations: &'configuration configuration::Configurations<'input>,
    admitted: &'configuration BTreeMap<String, Option<bool>>,
}

/// The first byte of `range`, which no item inside it starts before.
const fn first(range: rift_syntax::ByteRange) -> rift_syntax::ByteRange {
    rift_syntax::ByteRange {
        start: range.start,
        end: range.start + 1,
    }
}

impl Modules<'_, '_> {
    /// The admission of a module file itself; a file no ordinary item declares is admitted.
    fn file(&self, file: &super::SelectedFile<'_>) -> Option<bool> {
        let module = self
            .admitted
            .get(file.path.as_str())
            .copied()
            .unwrap_or(Some(true));
        let scope = file_scope(file.source)?;
        and(
            module,
            self.configurations.file(file.path.as_str()).admitted(
                file.syntax,
                file.source,
                first(scope),
            ),
        )
    }

    /// The admission of a module item: its own attributes and, for an inline module, the
    /// inner attributes of its body.
    fn module(
        &self,
        file: &super::SelectedFile<'_>,
        symbol: &rift_syntax::SyntaxSymbol,
    ) -> Option<bool> {
        let item = self.declared(file, symbol.item_range);
        match symbol.body_range {
            Some(body) => and(item, self.declared(file, body)),
            None => item,
        }
    }

    /// The admission of a declaration itself, whatever the conditions of its contents.
    fn declared(
        &self,
        file: &super::SelectedFile<'_>,
        range: rift_syntax::ByteRange,
    ) -> Option<bool> {
        self.admitted(file, first(range))
    }

    fn admitted(
        &self,
        file: &super::SelectedFile<'_>,
        range: rift_syntax::ByteRange,
    ) -> Option<bool> {
        let module = self
            .admitted
            .get(file.path.as_str())
            .copied()
            .unwrap_or(Some(true));
        and(
            module,
            self.configurations
                .file(file.path.as_str())
                .admitted(file.syntax, file.source, range),
        )
    }
}

fn export_binding(
    request: &(Vec<String>, macros::Export),
    identity: SymbolId,
    target: (SymbolId, String, String),
) -> super::ExportBinding {
    let (_, (path, range, name, _, _)) = request;
    let (target, target_path, target_qualified_name) = target;
    super::ExportBinding {
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
    }
}

/// The first export binding at each qualified path, in the order the bindings were added.
///
/// Every request of every pass asks which earlier binding its target path names. Reading
/// each binding's identity again for each question made one pass cost the requests times
/// the bindings; the bindings only grow, so each identity is parsed once, when it joins.
#[derive(Default)]
struct ExportPaths {
    first: BTreeMap<Vec<String>, usize>,
    /// The first binding whose identity does not parse; a scan stops there.
    unparsed: Option<usize>,
    indexed: usize,
}

impl ExportPaths {
    /// The first binding at `path` and the bindings a scan in order reads to answer:
    /// up to and including the match or the first unparsed identity, else all of them.
    fn first(
        &mut self,
        exports: &[super::ExportBinding],
        path: &[String],
    ) -> (Option<usize>, usize) {
        for (index, export) in exports.iter().enumerate().skip(self.indexed) {
            match SymbolIdentity::parse(export.identity.as_str()) {
                Ok(identity) => {
                    self.first
                        .entry(identity.qualified_path().to_vec())
                        .or_insert(index);
                }
                Err(_) => {
                    self.unparsed.get_or_insert(index);
                }
            }
        }
        self.indexed = exports.len();
        let found = self.first.get(path).copied();
        let last = match (found, self.unparsed) {
            (Some(found), Some(unparsed)) => found.min(unparsed),
            (Some(last), None) | (None, Some(last)) => last,
            (None, None) => return (None, exports.len()),
        };
        (found.filter(|found| *found == last), last.saturating_add(1))
    }
}

fn export_target(
    input: &NamespaceInput<'_>,
    declarations: &LocalDeclarations<'_>,
    (prepared, export_paths): (&super::PreparedNamespace, &mut ExportPaths),
    (modules, visibility): (&Modules<'_, '_>, Option<&str>),
    request: &(Vec<String>, macros::Export),
    work: &mut usize,
    work_max: usize,
) -> Option<(SymbolId, String, String)> {
    let (namespace, (_, _, _, target, _)) = request;
    let local = requested_path(namespace, target).and_then(|target_path| {
        // The charge is what reading the bindings in order costs, so the bound holds
        // the same requests as before.
        let (found, read) = export_paths.first(&prepared.exports, &target_path);
        *work = work.checked_add(read)?;
        if *work > work_max {
            return None;
        }
        if let Some(export) = found.and_then(|found| prepared.exports.get(found)) {
            return Some((
                export.target.clone(),
                export.target_path.clone(),
                export.target_qualified_name.clone(),
            ));
        }
        if export_paths.unparsed.is_some() {
            return None;
        }
        local_target(
            input,
            declarations,
            modules,
            (&target_path, namespace, visibility),
            work,
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
    match local {
        Some(local) => Some(local),
        None if is_path_keyword(target)
            || local_export_head(input, namespace, target, prepared, declarations) =>
        {
            None
        }
        None => external_target(input, target, work, work_max),
    }
}

fn local_export_head(
    input: &NamespaceInput<'_>,
    namespace: &[String],
    target: &[String],
    prepared: &super::PreparedNamespace,
    declarations: &LocalDeclarations<'_>,
) -> bool {
    let Some(identity) = target
        .first()
        .and_then(|name| export_identity(input, namespace, name))
    else {
        return true;
    };
    declarations.identities.contains(identity.as_str())
        || prepared
            .exports
            .iter()
            .any(|export| export.identity == identity)
}

fn export_identity(
    input: &NamespaceInput<'_>,
    namespace: &[String],
    name: &str,
) -> Option<SymbolId> {
    let mut path = namespace.to_vec();
    path.push(name.to_owned());
    let identity = SymbolIdentity::new(
        input.owner().clone(),
        ShippedLanguage::Rust.language(),
        path,
    )
    .ok()?;
    SymbolId::new(identity.wire_identity()).ok()
}

fn conflicting_requests(
    input: &NamespaceInput<'_>,
    files: &BTreeMap<&str, &super::SelectedFile<'_>>,
    requested: &[(Vec<String>, macros::Export)],
    modules: &Modules<'_, '_>,
) -> std::collections::BTreeSet<SymbolId> {
    let mut seen = std::collections::BTreeSet::new();
    let mut refused = std::collections::BTreeSet::new();
    for (namespace, (path, range, name, _, _)) in requested {
        let Some(identity) = export_identity(input, namespace, name) else {
            continue;
        };
        let Some(file) = files.get(path.as_str()) else {
            continue;
        };
        let admitted = modules.admitted(file, *range);
        if admitted == Some(false) {
            continue;
        }
        if admitted.is_none() || !seen.insert(identity.clone()) {
            refused.insert(identity);
        }
    }
    refused
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

/// Cached declarations by identity, plus every established anchor for local-name checks.
struct LocalDeclarations<'input> {
    by_identity: BTreeMap<&'input str, Vec<LocalDeclaration<'input>>>,
    identities: std::collections::BTreeSet<&'input str>,
}

/// Visits each declaration once under `work_max`; known inactive declarations stay out.
fn local_declarations<'input>(
    selected: &'input [super::SelectedFile<'input>],
    anchors: &'input super::Anchors,
    modules: &Modules<'_, '_>,
    work: &mut usize,
    work_max: usize,
) -> Option<LocalDeclarations<'input>> {
    let mut declarations = LocalDeclarations {
        by_identity: BTreeMap::new(),
        identities: anchors
            .values()
            .flat_map(BTreeMap::values)
            .map(SymbolId::as_str)
            .collect(),
    };
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
            if modules.admitted(file, symbol.item_range) == Some(false) {
                continue;
            }
            if let Some(identity) = anchors.get(&symbol.qualified_name) {
                declarations
                    .by_identity
                    .entry(identity.as_str())
                    .or_default()
                    .push((file, symbol, identity));
            }
        }
    }
    Some(declarations)
}

/// The one active declaration `path` names with a source range in its file, when the
/// re-export written in `namespace` at `visibility` (`None` for `pub`) may name it.
fn local_target<'input>(
    input: &NamespaceInput<'_>,
    declarations: &LocalDeclarations<'input>,
    modules: &Modules<'_, '_>,
    (path, namespace, visibility): (&[String], &[String], Option<&str>),
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
    let [(file, symbol, identity)] = declarations.by_identity.get(requested.as_str())?.as_slice()
    else {
        return None;
    };
    let admitted = modules.admitted(file, symbol.item_range);
    let spelled = file
        .source
        .get(usize::try_from(symbol.range.start).ok()?..usize::try_from(symbol.range.end).ok()?)
        .is_some();
    let (_, module) = path.split_last()?;
    (admitted == Some(true)
        && reexport_allowed(symbol.visibility.as_deref(), module, namespace, visibility)
        && spelled)
        .then_some((*file, *symbol, *identity))
}

/// Whether a re-export written in `namespace` at `reexport` (`None` for `pub`) may name an
/// item of `module` declared at `declared`: the re-exporting module sees the item, and the
/// re-export is visible nowhere the item is not. A public item allows every re-export.
fn reexport_allowed(
    declared: Option<&str>,
    module: &[String],
    namespace: &[String],
    reexport: Option<&str>,
) -> bool {
    if declared == Some("pub") {
        return true;
    }
    let Some(reexport) = reexport else {
        return false;
    };
    let (Some(item), Some(written)) = (
        visible_within(declared, module),
        visible_within(Some(reexport), namespace),
    ) else {
        return false;
    };
    namespace.starts_with(&item) && written.starts_with(&item)
}

/// The module whose descendants see an item of `module` declared at `visibility`.
fn visible_within(visibility: Option<&str>, module: &[String]) -> Option<Vec<String>> {
    let spelled = visibility.map(|visibility| {
        visibility
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>()
    });
    match spelled.as_deref() {
        None | Some("pub(self)") => Some(module.to_vec()),
        Some("pub(crate)") => Some(vec![module.first()?.clone()]),
        Some("pub(super)") => Some(module.split_last()?.1.to_vec()),
        Some(spelled) => {
            let path = spelled.strip_prefix("pub(in")?.strip_suffix(')')?;
            let segments = path.split("::").map(str::to_owned).collect::<Vec<_>>();
            let resolved = requested_path(module, &segments)?;
            // The path names an ancestor of the item's module, or the module itself.
            module.starts_with(&resolved).then_some(resolved)
        }
    }
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
        for (path, _, _) in module_paths(
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
