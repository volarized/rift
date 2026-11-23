//! Names dependency and standard-library targets that selected sources spell.
//!
//! A site reaches this pass only after resolution against the selected sources answered
//! nothing. The authored path is expanded through the file's own bindings, and the owner of
//! its root comes from the manifests captured with the selection or from the language's own
//! standard-library names. The identity is the spelled public path under that owner. An
//! admitted dependency publication replaces it with the defining identity.
//!
//! A site whose owner is not established names no target. It is counted by edge kind.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use rift_dependency::StandardLibrary;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{EdgeKind, Language};
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{Binding, File, Index, Work};
use crate::NamespaceInput;

mod names;
mod sites;

const CARGO_SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
const NPM_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];
/// Dependency entry keys that select a source other than the default registry.
const CARGO_FOREIGN_KEYS: [&str; 3] = ["path", "git", "registry"];
/// Poetry dependency keys that select a source other than the default registry.
const POETRY_FOREIGN_KEYS: [&str; 5] = ["path", "git", "url", "file", "source"];
/// Version specifiers that select a source other than the default registry.
const NPM_FOREIGN_PREFIXES: [&str; 9] = [
    "workspace:",
    "file:",
    "link:",
    "portal:",
    "git",
    "http:",
    "https:",
    "github:",
    "catalog:",
];
const NPM_ALIAS_PREFIX: &str = "npm:";
const NODE_SCHEME: &str = "node:";
const NPM_INSTALL_DIRECTORY: &str = "node_modules";
/// Suffixes of a package entry written in TypeScript.
const TYPESCRIPT_SUFFIXES: [&str; 4] = [".ts", ".tsx", ".mts", ".cts"];

/// One source occurrence whose target is owned by a dependency or a standard library.
#[derive(Debug)]
pub(crate) struct ExternalOccurrence {
    pub(crate) binding: rift_core::DeclarationBinding,
    pub(crate) target: rift_core::SymbolId,
    pub(crate) source_digest: rift_core::FileDigest,
    pub(crate) callee_range: ByteRange,
    pub(crate) role: ReferenceRole,
    pub(crate) edge_from: Option<rift_core::SymbolId>,
    pub(crate) edge_kind: Option<EdgeKind>,
    /// Whether an admitted dependency publication declares the target.
    pub(crate) declared: bool,
}

/// Dependency owners named by the manifests captured with one selection.
pub(super) struct Owners {
    owner: SymbolOwner,
    /// Manifest directory, then library name to registry package.
    cargo: Vec<(String, BTreeMap<String, String>)>,
    /// Manifest directory, then authored dependency name to registry package.
    npm: Vec<(String, BTreeMap<String, String>)>,
    /// Names of npm packages the selection itself holds.
    npm_selected: BTreeSet<String>,
    /// Directory holding an install folder, then package name to the language its
    /// manifest's entry is written in.
    npm_languages: BTreeMap<String, BTreeMap<String, &'static str>>,
    /// Normalized distribution names.
    pypi: BTreeSet<String>,
    /// Normalized distribution names a manifest selects from another source.
    pypi_foreign: BTreeSet<String>,
}

impl Owners {
    pub(super) fn new(namespace: &NamespaceInput<'_>, work: &mut Work) -> Option<Self> {
        let mut owners = Self {
            owner: namespace.owner().clone(),
            cargo: Vec::new(),
            npm: Vec::new(),
            npm_selected: BTreeSet::new(),
            npm_languages: BTreeMap::new(),
            pypi: BTreeSet::new(),
            pypi_foreign: BTreeSet::new(),
        };
        let mut workspace = Vec::new();
        let mut inherited = Vec::new();
        for source in namespace.files().iter().chain(namespace.context_sources()) {
            if !work.charge() {
                return None;
            }
            let path = source.path().as_str();
            let (directory, name) = path.rsplit_once('/').unwrap_or(("", path));
            match name {
                "Cargo.toml" => {
                    let Ok(manifest) = toml::from_str::<toml::Table>(source.text()) else {
                        continue;
                    };
                    let (direct, pending) =
                        cargo_dependencies(&manifest, &mut workspace, directory, work)?;
                    inherited.push((owners.cargo.len(), pending));
                    owners.cargo.push((directory.to_owned(), direct));
                }
                "package.json" => {
                    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(source.text())
                    else {
                        continue;
                    };
                    owners.npm_package(directory, &manifest, work)?;
                    owners
                        .npm
                        .push((directory.to_owned(), npm_dependencies(&manifest, work)?));
                }
                "pyproject.toml" => {
                    let Ok(manifest) = toml::from_str::<toml::Table>(source.text()) else {
                        continue;
                    };
                    pypi_dependencies(&manifest, &mut owners, work)?;
                }
                "METADATA" | "PKG-INFO" => {
                    for line in source.text().lines() {
                        if !work.charge() {
                            return None;
                        }
                        if line.is_empty() {
                            break;
                        }
                        if let Some(requirement) = line.strip_prefix("Requires-Dist:") {
                            owners.pypi.extend(requirement_name(requirement));
                        }
                    }
                }
                _ => {}
            }
        }
        for (index, pending) in inherited {
            for name in pending {
                if !work.charge() {
                    return None;
                }
                let directory = owners.cargo.get(index)?.0.clone();
                let Some(package) =
                    nearest(&workspace, &directory).and_then(|rows| rows.get(&name))
                else {
                    continue;
                };
                let package = package.clone();
                owners
                    .cargo
                    .get_mut(index)?
                    .1
                    .insert(name.replace('-', "_"), package);
            }
        }
        Some(owners)
    }

    /// Records the package one `package.json` describes: a package the selection holds, or
    /// an installed package with the language of its entry.
    fn npm_package(
        &mut self,
        directory: &str,
        manifest: &serde_json::Value,
        work: &mut Work,
    ) -> Option<()> {
        let install = format!("{NPM_INSTALL_DIRECTORY}/");
        let Some(start) = directory.rfind(&install) else {
            let name = manifest.get("name").and_then(serde_json::Value::as_str);
            self.npm_selected.extend(name.map(str::to_owned));
            return Some(());
        };
        let root = directory.get(..start)?;
        if !root.is_empty() && !root.ends_with('/') {
            return Some(());
        }
        let name = directory.get(start.checked_add(install.len())?..)?;
        self.npm_languages
            .entry(root.trim_end_matches('/').to_owned())
            .or_default()
            .insert(name.to_owned(), npm_entry_language(manifest, work)?);
        Some(())
    }

    /// Language of the nearest installed copy of one npm package, when its manifest is
    /// captured.
    fn npm_language(&self, file: &str, package: &str) -> Option<&'static str> {
        let mut directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
        loop {
            if let Some(language) = self
                .npm_languages
                .get(directory)
                .and_then(|packages| packages.get(package))
            {
                return Some(language);
            }
            if directory.is_empty() {
                return None;
            }
            directory = directory.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    /// Owner and public path of a Rust path whose root no selected source declares.
    fn rust(&self, file: &str, path: &[String]) -> Option<(SymbolOwner, Vec<String>)> {
        let (head, tail) = path.split_first()?;
        if names::RUST_RUNTIME_CRATES.contains(&head.as_str()) {
            return Some((StandardLibrary::Rust.symbol_owner(), path.to_vec()));
        }
        let directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
        if let Some(package) = nearest(&self.cargo, directory).and_then(|rows| rows.get(head)) {
            let mut spelled = vec![package.replace('-', "_")];
            spelled.extend(tail.iter().cloned());
            return Some((registry_package("cargo", package)?, spelled));
        }
        let prelude = names::rust_prelude(head)?;
        let mut spelled = prelude
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<String>>();
        spelled.extend(tail.iter().cloned());
        Some((StandardLibrary::Rust.symbol_owner(), spelled))
    }

    /// Owner of a Python module path whose root no selected source declares.
    fn python(&self, path: &[String]) -> Option<SymbolOwner> {
        let root = path.first()?;
        if names::PYTHON_STANDARD_MODULES
            .binary_search(&root.as_str())
            .is_ok()
        {
            return Some(StandardLibrary::Python.symbol_owner());
        }
        let distribution = normalized_distribution(root);
        if !self.pypi.contains(&distribution) || self.pypi_foreign.contains(&distribution) {
            return None;
        }
        registry_package("pypi", &distribution)
    }

    /// Owner and module segment of an ECMAScript module specifier.
    fn javascript(&self, file: &str, specifier: &str) -> Option<(SymbolOwner, String)> {
        // A directory specifier names no module entry.
        if !dependency_specifier(specifier) || specifier.ends_with('/') {
            return None;
        }
        if let Some(module) = specifier.strip_prefix(NODE_SCHEME) {
            return Some((StandardLibrary::Node.symbol_owner(), module.to_owned()));
        }
        let mut parts = specifier.splitn(3, '/');
        let first = parts.next()?;
        let name = if first.starts_with('@') {
            let end = first
                .len()
                .checked_add(1)?
                .checked_add(parts.next()?.len())?;
            specifier.get(..end)?
        } else {
            first
        };
        if names::NODE_BUILTIN_MODULES.binary_search(&name).is_ok() {
            return Some((StandardLibrary::Node.symbol_owner(), specifier.to_owned()));
        }
        let directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
        let package = nearest(&self.npm, directory)?.get(name)?;
        // A package manager links a package the selection holds instead of installing it.
        if self.npm_selected.contains(package) {
            return None;
        }
        let module = format!("{package}{}", specifier.get(name.len()..)?);
        Some((registry_package("npm", package)?, module))
    }

    /// The identity of one spelled path, in the language its owner establishes. Every file
    /// that spells one target names one identity, whatever language the file is in.
    fn identity(
        &self,
        file: &File<'_>,
        owner: SymbolOwner,
        path: Vec<String>,
    ) -> Option<rift_core::SymbolId> {
        if owner == self.owner {
            return None;
        }
        let spelled = file.document.facts.language().name.as_str();
        let name = match &owner {
            SymbolOwner::Runtime { runtime } => {
                StandardLibrary::from_symbol_runtime(runtime)?.symbol_language()
            }
            SymbolOwner::Package { manager, name, .. } if manager == "npm" => self
                .npm_language(file.document.path.as_str(), name)
                .unwrap_or(spelled),
            _ => spelled,
        };
        let language = Language {
            name: name.to_owned(),
            dialect: None,
        };
        let identity = SymbolIdentity::new(owner, language, path).ok()?;
        rift_core::SymbolId::new(identity.wire_identity()).ok()
    }
}

fn dependency_specifier(specifier: &str) -> bool {
    !specifier.is_empty() && !specifier.starts_with(['.', '/', '#'])
}

/// Owner of a package in its manager's default registry, or nothing for a name the
/// identity grammar does not spell.
fn registry_package(manager: &str, name: &str) -> Option<SymbolOwner> {
    let registry = SymbolOwner::default_registry(manager)?;
    SymbolOwner::registry_package(manager, registry, name).ok()
}

/// Rows of the manifest in the nearest directory at or above `directory`.
fn nearest<'rows, T>(manifests: &'rows [(String, T)], directory: &str) -> Option<&'rows T> {
    manifests
        .iter()
        .filter(|(root, _)| {
            root.is_empty()
                || directory == root
                || directory
                    .strip_prefix(root.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|(root, _)| root.len())
        .map(|(_, rows)| rows)
}

/// The registry package one Cargo dependency entry selects, or nothing for an entry
/// that names a path, a Git source, or another registry.
fn cargo_package(name: &str, entry: &toml::Value) -> Option<String> {
    match entry {
        toml::Value::String(_) => Some(name.to_owned()),
        toml::Value::Table(table) => {
            if CARGO_FOREIGN_KEYS
                .iter()
                .any(|key| table.contains_key(*key))
            {
                return None;
            }
            Some(
                table
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(name)
                    .to_owned(),
            )
        }
        _ => None,
    }
}

type CargoRows = (BTreeMap<String, String>, Vec<String>);

/// Direct registry dependencies by library name, and the names inherited from a workspace.
fn cargo_dependencies(
    manifest: &toml::Table,
    workspace: &mut Vec<(String, BTreeMap<String, String>)>,
    directory: &str,
    work: &mut Work,
) -> Option<CargoRows> {
    let mut direct = BTreeMap::new();
    let mut inherited = Vec::new();
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(toml::Table::values)
        .filter_map(toml::Value::as_table);
    for table in std::iter::once(manifest).chain(targets) {
        for section in CARGO_SECTIONS {
            let Some(entries) = table.get(section).and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, entry) in entries {
                if !work.charge() {
                    return None;
                }
                let inherits = entry
                    .get("workspace")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false);
                if inherits {
                    inherited.push(name.clone());
                } else if let Some(package) = cargo_package(name, entry) {
                    direct.insert(name.replace('-', "_"), package);
                }
            }
        }
    }
    let shared = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table);
    if let Some(entries) = shared {
        let mut rows = BTreeMap::new();
        for (name, entry) in entries {
            if !work.charge() {
                return None;
            }
            if let Some(package) = cargo_package(name, entry) {
                rows.insert(name.clone(), package);
            }
        }
        workspace.push((directory.to_owned(), rows));
    }
    Some((direct, inherited))
}

fn npm_dependencies(
    manifest: &serde_json::Value,
    work: &mut Work,
) -> Option<BTreeMap<String, String>> {
    let mut rows = BTreeMap::new();
    for section in NPM_SECTIONS {
        let Some(entries) = manifest.get(section).and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (name, selector) in entries {
            if !work.charge() {
                return None;
            }
            let Some(selector) = selector.as_str() else {
                continue;
            };
            if let Some(alias) = selector.strip_prefix(NPM_ALIAS_PREFIX) {
                let package = match alias.rfind('@') {
                    Some(0) | None => alias,
                    Some(end) => alias.get(..end)?,
                };
                rows.insert(name.clone(), package.to_owned());
            } else if !NPM_FOREIGN_PREFIXES
                .iter()
                .any(|prefix| selector.starts_with(prefix))
                && !selector.contains('/')
            {
                rows.insert(name.clone(), name.clone());
            }
        }
    }
    Some(rows)
}

/// Language a package's entry is written in: TypeScript when the manifest names a type
/// declaration or a TypeScript entry, else JavaScript.
fn npm_entry_language(manifest: &serde_json::Value, work: &mut Work) -> Option<&'static str> {
    let typescript = ["types", "typings"]
        .iter()
        .any(|key| manifest.get(key).is_some_and(serde_json::Value::is_string));
    if typescript {
        return Some("typescript");
    }
    let mut pending = ["main", "module", "exports"]
        .iter()
        .filter_map(|key| manifest.get(key))
        .collect::<Vec<_>>();
    while let Some(entry) = pending.pop() {
        if !work.charge() {
            return None;
        }
        match entry {
            serde_json::Value::String(path)
                if TYPESCRIPT_SUFFIXES
                    .iter()
                    .any(|suffix| path.ends_with(suffix)) =>
            {
                return Some("typescript");
            }
            serde_json::Value::Array(entries) => pending.extend(entries),
            serde_json::Value::Object(entries) => pending.extend(entries.values()),
            _ => {}
        }
    }
    Some("javascript")
}

fn pypi_dependencies(manifest: &toml::Table, owners: &mut Owners, work: &mut Work) -> Option<()> {
    let project = manifest.get("project").and_then(toml::Value::as_table);
    let direct = project
        .and_then(|project| project.get("dependencies"))
        .and_then(toml::Value::as_array);
    let groups = [
        project.and_then(|project| project.get("optional-dependencies")),
        manifest.get("dependency-groups"),
    ];
    let grouped = groups
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_table)
        .flat_map(toml::Table::values)
        .filter_map(toml::Value::as_array);
    for requirements in direct.into_iter().chain(grouped) {
        for requirement in requirements {
            if !work.charge() {
                return None;
            }
            owners
                .pypi
                .extend(requirement.as_str().and_then(requirement_name));
        }
    }
    let tools = manifest.get("tool");
    let sources = tools
        .and_then(|tool| tool.get("uv"))
        .and_then(|uv| uv.get("sources"))
        .and_then(toml::Value::as_table);
    for name in sources.into_iter().flat_map(toml::Table::keys) {
        if !work.charge() {
            return None;
        }
        owners.pypi_foreign.insert(normalized_distribution(name));
    }
    let poetry = tools
        .and_then(|tool| tool.get("poetry"))
        .and_then(|poetry| poetry.get("dependencies"))
        .and_then(toml::Value::as_table);
    for (name, selector) in poetry.into_iter().flatten() {
        if !work.charge() {
            return None;
        }
        // One dependency can list several constraints, each with its own source.
        let constraints = selector
            .as_array()
            .map_or(std::slice::from_ref(selector), Vec::as_slice);
        let foreign = constraints
            .iter()
            .filter_map(toml::Value::as_table)
            .any(|table| {
                POETRY_FOREIGN_KEYS
                    .iter()
                    .any(|key| table.contains_key(*key))
            });
        if foreign {
            owners.pypi_foreign.insert(normalized_distribution(name));
        } else if name != "python" {
            owners.pypi.insert(normalized_distribution(name));
        }
    }
    Some(())
}

/// The normalized distribution a requirement names, or nothing for a direct reference.
fn requirement_name(requirement: &str) -> Option<String> {
    let requirement = requirement.trim();
    let end = requirement
        .find(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
        })
        .unwrap_or(requirement.len());
    let (name, rest) = requirement.split_at(end);
    let rest = rest.trim_start();
    let rest = match rest.strip_prefix('[') {
        Some(extras) => extras.split_once(']')?.1.trim_start(),
        None => rest,
    };
    (!name.is_empty() && !rest.starts_with('@')).then(|| normalized_distribution(name))
}

/// Lowercase with each run of `-`, `_`, and `.` written as one `-`.
fn normalized_distribution(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut separated = false;
    for character in name.chars() {
        if matches!(character, '-' | '_' | '.') {
            separated = true;
            continue;
        }
        if separated && !normalized.is_empty() {
            normalized.push('-');
        }
        separated = false;
        normalized.push(character.to_ascii_lowercase());
    }
    normalized
}

/// A Rust path without its generic arguments, which select no other declaration.
fn without_generic_arguments(text: &str, work: &mut Work) -> Option<String> {
    let mut path = String::with_capacity(text.len());
    let mut depth = 0usize;
    for character in text.chars() {
        if !work.charge() {
            return None;
        }
        match character {
            '<' => depth = depth.checked_add(1)?,
            '>' => depth = depth.checked_sub(1)?,
            _ if depth == 0 => path.push(character),
            _ => {}
        }
    }
    (depth == 0).then(|| path.replace("::::", "::"))
}

fn identifier_path(text: &str, separator: &str, work: &mut Work) -> Option<Vec<String>> {
    let mut path = Vec::new();
    for part in text.split(separator) {
        if !work.charge() {
            return None;
        }
        let part = part.trim();
        let mut characters = part.chars();
        if !characters
            .next()
            .is_some_and(|character| character.is_alphabetic() || matches!(character, '_' | '$'))
            || !characters
                .all(|character| character.is_alphanumeric() || matches!(character, '_' | '$'))
        {
            return None;
        }
        path.push(part.to_owned());
    }
    Some(path)
}

impl<'source> Index<'source> {
    /// The dependency or standard-library callee one unresolved call spells.
    pub(super) fn external_call(
        &self,
        file: &File<'source>,
        call: &StaticCall,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        if file.blocked(call.range.start, work) {
            return None;
        }
        let position = call.range.start;
        match file.document.facts.language().name.as_str() {
            "rust" => match call.callee {
                StaticCallee::Direct | StaticCallee::Qualified => {
                    self.external_rust(file, call.callee_range, position, true, work)
                }
                _ => None,
            },
            // A callee that is not a plain dotted path spells no declaration.
            "python" => self.external_python(file, file.text(call.callee_range)?, position, work),
            "javascript" | "typescript" => {
                self.external_javascript(file, file.text(call.callee_range)?, position, work)
            }
            _ => None,
        }
    }

    /// The dependency or standard-library declaration one unresolved import names.
    pub(super) fn external_import(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        match binding.kind {
            StaticBindingKind::RustUse { path } => self.external_use(file, path, work),
            // A Python or ECMAScript import spells its whole path. No binding changes it.
            StaticBindingKind::PythonModule { module } => {
                let path = identifier_path(file.text(module)?, ".", work)?;
                self.python_target(file, path)
            }
            StaticBindingKind::PythonImport { .. } => {
                let path = python_binding_path(file, binding, work)?;
                self.python_target(file, path)
            }
            StaticBindingKind::JavaScriptImport { .. }
            | StaticBindingKind::JavaScriptRequire { .. } => {
                let import = self.javascript_binding_path(file, binding, work)?;
                self.javascript_import_target(file, import, &[])
            }
            _ => None,
        }
    }

    /// The dependency or standard-library path one unresolved `use` leaf or glob spells.
    fn external_use(
        &self,
        file: &File<'source>,
        path: ByteRange,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        if file.blocked(path.start, work) {
            return None;
        }
        let parts = self.rust.spelled_use(file, path, work)?;
        let (owner, spelled) = self.owners.rust(file.document.path.as_str(), &parts)?;
        self.owners.identity(file, owner, spelled)
    }

    /// The identity of one absolute Python path whose root no selected source declares.
    fn python_target(
        &self,
        file: &File<'source>,
        path: Vec<String>,
    ) -> Option<rift_core::SymbolId> {
        let root = path.first()?;
        if self.edge_python.declares_root(root) {
            return None;
        }
        let owner = self.owners.python(&path)?;
        self.owners.identity(file, owner, path)
    }

    /// The dependency or standard-library type one unresolved heritage or type use spells.
    pub(super) fn external_type(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        if file.blocked(range.start, work) {
            return None;
        }
        match file.document.facts.language().name.as_str() {
            "rust" => self.external_rust(file, range, range.start, false, work),
            "python" => self.external_python(file, file.text(range)?, range.start, work),
            "javascript" | "typescript" => {
                self.external_javascript(file, file.text(range)?, range.start, work)
            }
            _ => None,
        }
    }

    fn external_rust(
        &self,
        file: &File<'source>,
        range: ByteRange,
        position: u64,
        value: bool,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        let text = file.text(range)?;
        let text = without_generic_arguments(text, work)?;
        let parts = self.rust.spelled_path(file, position, &text, value, work)?;
        let (owner, spelled) = self.owners.rust(file.document.path.as_str(), &parts)?;
        self.owners.identity(file, owner, spelled)
    }

    fn external_python(
        &self,
        file: &File<'source>,
        text: &str,
        position: u64,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        let parts = identifier_path(text, ".", work)?;
        let (head, tail) = parts.split_first()?;
        if file.javascript_reassigned(head, None) {
            return None;
        }
        let mut path = match file.binding(head, position, |_| true, work) {
            Binding::Found(binding) => python_binding_path(file, binding, work)?,
            Binding::Missing
                if names::PYTHON_BUILTINS.binary_search(&head.as_str()).is_ok()
                    && file.qualified_target(head).is_none() =>
            {
                vec!["builtins".to_owned(), head.clone()]
            }
            Binding::Missing => return None,
            Binding::Blocked => python_repeated_import(file, head, position, work)?,
        };
        path.extend(tail.iter().cloned());
        self.python_target(file, path)
    }

    fn external_javascript(
        &self,
        file: &File<'source>,
        text: &str,
        position: u64,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        let parts = identifier_path(text, ".", work)?;
        let (head, tail) = parts.split_first()?;
        if file.javascript_reassigned(head, tail.first().map(String::as_str)) {
            return None;
        }
        // A type a documentation comment imports names no value.
        let value = |kind| !matches!(kind, StaticBindingKind::JavaScriptTypeImport { .. });
        let Binding::Found(binding) = file.binding(head, position, value, work) else {
            return None;
        };
        self.javascript_import_target(
            file,
            self.javascript_binding_path(file, binding, work)?,
            tail,
        )
    }

    /// The package type one written type names through its nearest import binding.
    ///
    /// A local or ambiguous binding names no package type.
    fn external_javascript_type(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
    ) -> Option<rift_core::SymbolId> {
        let parts = identifier_path(file.text(range)?, ".", work)?;
        let (head, tail) = parts.split_first()?;
        let binding = super::javascript::js_binding(
            file,
            &self.javascript,
            head,
            range.start,
            ReferenceRole::Type,
            work,
        )?;
        if file.javascript_reassigned(head, tail.first().map(String::as_str)) {
            return None;
        }
        let import = match binding.kind {
            StaticBindingKind::JavaScriptTypeImport { module, export } => {
                (file.text(module)?, Some(file.text(export)?))
            }
            StaticBindingKind::JavaScriptImport { .. }
            | StaticBindingKind::JavaScriptEdgeNamespace { .. } => {
                self.javascript_binding_path(file, binding, work)?
            }
            _ => return None,
        };
        self.javascript_import_target(file, import, tail)
    }

    /// The module one specifier names in a dependency or the standard library.
    fn javascript_module_target(
        &self,
        file: &File<'source>,
        specifier: &str,
    ) -> Option<rift_core::SymbolId> {
        self.javascript_import_target(file, (specifier, None), &[])
    }

    fn javascript_import_target(
        &self,
        file: &File<'source>,
        (specifier, export): (&str, Option<&str>),
        tail: &[String],
    ) -> Option<rift_core::SymbolId> {
        let (owner, module) = self
            .owners
            .javascript(file.document.path.as_str(), specifier)?;
        let mut path = vec![module];
        // The default export of a Node.js module is the module itself.
        let export = export
            .filter(|name| *name != "default" || !matches!(owner, SymbolOwner::Runtime { .. }));
        path.extend(export.map(str::to_owned));
        path.extend(tail.iter().cloned());
        self.owners.identity(file, owner, path)
    }

    /// Module specifier and exported name one import binding spells.
    fn javascript_binding_path(
        &self,
        file: &File<'source>,
        binding: &StaticBinding,
        work: &mut Work,
    ) -> Option<(&'source str, Option<&'source str>)> {
        match binding.kind {
            StaticBindingKind::JavaScriptImport { module, export } => Some((
                file.text(module)?,
                Some(match export {
                    Some(export) => file.text(export)?,
                    None => "default",
                }),
            )),
            StaticBindingKind::JavaScriptEdgeNamespace { module } => {
                Some((file.text(module)?, None))
            }
            StaticBindingKind::JavaScriptRequire {
                module,
                export,
                require,
            } if self.builtin_require(file, require.start, work) => Some((
                file.text(module)?,
                match export {
                    Some(export) => Some(file.text(export)?),
                    None => None,
                },
            )),
            _ => None,
        }
    }
}

/// Absolute module path one Python import binding spells.
fn python_binding_path(
    file: &File<'_>,
    binding: &StaticBinding,
    work: &mut Work,
) -> Option<Vec<String>> {
    let (module, export) = match binding.kind {
        StaticBindingKind::PythonImport { module, export } => (module, export),
        // `import a.b` binds `a`; the site spells the rest.
        StaticBindingKind::PythonModule { .. } => (binding.name, None),
        _ => return None,
    };
    // A relative import starts with a dot, which no identifier does.
    let mut path = identifier_path(file.text(module)?, ".", work)?;
    if let Some(export) = export {
        path.push(file.text(export)?.to_owned());
    }
    Some(path)
}

/// Path every import of one Python name spells in the innermost scope that binds it, or
/// nothing when the bindings differ or one is no import.
fn python_repeated_import(
    file: &File<'_>,
    name: &str,
    position: u64,
    work: &mut Work,
) -> Option<Vec<String>> {
    let mut bindings = Vec::new();
    file.innermost_bindings(
        name,
        position,
        work,
        |_| true,
        |binding, first| {
            if first {
                bindings.clear();
            }
            bindings.push(binding);
        },
    )?;
    let mut spelled: Option<Vec<String>> = None;
    for binding in bindings {
        if !work.charge() || binding.available_from > position {
            return None;
        }
        let path = python_binding_path(file, binding, work)?;
        if spelled.as_ref().is_some_and(|known| *known != path) {
            return None;
        }
        spelled = Some(path);
    }
    spelled
}

/// Replaces spelled Rust identities with the defining identities admitted dependency
/// publications declare.
pub(super) fn admit_declared(
    namespace: &NamespaceInput<'_>,
    occurrences: &mut [ExternalOccurrence],
    work_max: usize,
) {
    if namespace.dependencies().is_empty() {
        return;
    }
    let mut work = 0usize;
    for occurrence in occurrences {
        let Ok(identity) = SymbolIdentity::parse(occurrence.target.as_str()) else {
            continue;
        };
        for dependency in namespace.dependencies() {
            if dependency.publication().owner != *identity.owner() {
                continue;
            }
            let mut path = identity.qualified_path().to_vec();
            if let Some(root) = path.first_mut() {
                dependency.name().clone_into(root);
            }
            let Some((target, _, _)) =
                dependency.target(&path, identity.language(), &mut work, work_max)
            else {
                continue;
            };
            occurrence.target = dependency.defining(target, &mut work, work_max);
            occurrence.declared = true;
        }
    }
}
