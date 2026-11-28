//! Names dependency and standard-library targets that selected sources spell.
//!
//! A site reaches this pass only after resolution against the selected sources answered
//! nothing. The authored path is expanded through the file's own bindings, and the owner of
//! its root comes from the manifests captured with the selection or from the language's own
//! standard-library names. The identity is the spelled public path under that owner. An
//! admitted dependency publication replaces it with the defining identity.

use std::collections::{BTreeMap, BTreeSet};

use rift_core::ReferenceRole;
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::{EdgeKind, Language};
use rift_syntax::{ByteRange, StaticBinding, StaticBindingKind, StaticCall, StaticCallee};

use super::{Binding, File, Index, StaticResolution, Work};
use crate::NamespaceInput;

mod names;

pub(super) use names::{RUST_RUNTIME_CRATES, rust_prelude};

const CARGO_SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
const NPM_SECTIONS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
];
/// Dependency entry keys that select a source other than the default registry.
const CARGO_FOREIGN_KEYS: [&str; 3] = ["path", "git", "registry"];
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

/// One source occurrence whose target is owned by a dependency or a standard library.
#[derive(Debug)]
pub(crate) struct ExternalOccurrence {
    pub(crate) binding: rift_core::DeclarationBinding,
    pub(crate) target: rift_core::SymbolId,
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
    /// Normalized distribution names.
    pypi: BTreeSet<String>,
}

impl Owners {
    pub(super) fn new(namespace: &NamespaceInput<'_>, work: &mut Work) -> Option<Self> {
        let mut owners = Self {
            owner: namespace.owner().clone(),
            cargo: Vec::new(),
            npm: Vec::new(),
            pypi: BTreeSet::new(),
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
                    owners
                        .npm
                        .push((directory.to_owned(), npm_dependencies(&manifest, work)?));
                }
                "pyproject.toml" => {
                    let Ok(manifest) = toml::from_str::<toml::Table>(source.text()) else {
                        continue;
                    };
                    pypi_dependencies(&manifest, &mut owners.pypi, work)?;
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

    /// Owner and public path of a Rust path whose root no selected source declares.
    fn rust(&self, file: &str, path: &[String]) -> Option<(SymbolOwner, Vec<String>)> {
        let (head, tail) = path.split_first()?;
        if names::RUST_RUNTIME_CRATES.contains(&head.as_str()) {
            return Some((runtime("rust"), path.to_vec()));
        }
        let directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
        if let Some(package) = nearest(&self.cargo, directory).and_then(|rows| rows.get(head)) {
            let mut spelled = vec![package.replace('-', "_")];
            spelled.extend(tail.iter().cloned());
            return Some((registry_package("cargo", "crates.io", package), spelled));
        }
        let prelude = names::rust_prelude(head)?;
        let mut spelled = prelude
            .split("::")
            .map(str::to_owned)
            .collect::<Vec<String>>();
        spelled.extend(tail.iter().cloned());
        Some((runtime("rust"), spelled))
    }

    /// Owner of a Python module path whose root no selected source declares.
    fn python(&self, path: &[String]) -> Option<SymbolOwner> {
        let root = path.first()?;
        if names::PYTHON_STANDARD_MODULES
            .binary_search(&root.as_str())
            .is_ok()
        {
            return Some(runtime("cpython"));
        }
        let distribution = normalized_distribution(root);
        self.pypi
            .contains(&distribution)
            .then(|| registry_package("pypi", "pypi.org", &distribution))
    }

    /// Owner and module segment of an ECMAScript module specifier.
    fn javascript(&self, file: &str, specifier: &str) -> Option<(SymbolOwner, String)> {
        if !dependency_specifier(specifier) {
            return None;
        }
        if let Some(module) = specifier.strip_prefix(NODE_SCHEME) {
            return Some((runtime("node"), module.to_owned()));
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
            return Some((runtime("node"), specifier.to_owned()));
        }
        let directory = file.rsplit_once('/').map_or("", |(directory, _)| directory);
        let package = nearest(&self.npm, directory)?.get(name)?;
        let module = format!("{package}{}", specifier.get(name.len()..)?);
        Some((registry_package("npm", "npmjs.org", package), module))
    }

    fn identity(
        &self,
        owner: SymbolOwner,
        language: &Language,
        path: Vec<String>,
    ) -> Option<rift_core::SymbolId> {
        if owner == self.owner {
            return None;
        }
        let language = Language {
            name: language.name.clone(),
            dialect: None,
        };
        let identity = SymbolIdentity::new(owner, language, path).ok()?;
        rift_core::SymbolId::new(identity.wire_identity()).ok()
    }
}

fn dependency_specifier(specifier: &str) -> bool {
    !specifier.is_empty() && !specifier.starts_with(['.', '/', '#'])
}

fn runtime(name: &str) -> SymbolOwner {
    SymbolOwner::Runtime {
        runtime: name.to_owned(),
    }
}

fn registry_package(manager: &str, registry: &str, name: &str) -> SymbolOwner {
    SymbolOwner::Package {
        manager: manager.to_owned(),
        registry: registry.to_owned(),
        name: name.to_owned(),
    }
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

fn pypi_dependencies(
    manifest: &toml::Table,
    rows: &mut BTreeSet<String>,
    work: &mut Work,
) -> Option<()> {
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
            rows.extend(requirement.as_str().and_then(requirement_name));
        }
    }
    let poetry = manifest
        .get("tool")
        .and_then(|tool| tool.get("poetry"))
        .and_then(|poetry| poetry.get("dependencies"))
        .and_then(toml::Value::as_table);
    for (name, selector) in poetry.into_iter().flatten() {
        if !work.charge() {
            return None;
        }
        let foreign = selector.as_table().is_some_and(|table| {
            ["path", "git", "url"]
                .iter()
                .any(|key| table.contains_key(*key))
        });
        if name != "python" && !foreign {
            rows.insert(normalized_distribution(name));
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
        if file.blocked(binding.name.start, work) {
            return None;
        }
        let language = file.document.facts.language();
        match binding.kind {
            StaticBindingKind::RustUse { path } => {
                let parts = self.rust.spelled_use(file, path, work)?;
                let (owner, spelled) = self.owners.rust(file.document.path.as_str(), &parts)?;
                self.owners.identity(owner, language, spelled)
            }
            StaticBindingKind::PythonImport { .. } => {
                let path = python_binding_path(file, binding, work)?;
                let owner = self.owners.python(&path)?;
                self.owners.identity(owner, language, path)
            }
            StaticBindingKind::JavaScriptImport { .. }
            | StaticBindingKind::JavaScriptEdgeNamespace { .. }
            | StaticBindingKind::JavaScriptRequire { .. } => {
                let (specifier, export) = self.javascript_binding_path(file, binding, work)?;
                let (owner, module) = self
                    .owners
                    .javascript(file.document.path.as_str(), specifier)?;
                let mut path = vec![module];
                path.extend(export.map(str::to_owned));
                self.owners.identity(owner, language, path)
            }
            _ => None,
        }
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
        self.rust_external_path(file, &parts)
    }

    pub(super) fn rust_external_path(
        &self,
        file: &File<'source>,
        parts: &[String],
    ) -> Option<rift_core::SymbolId> {
        let (owner, spelled) = self.owners.rust(file.document.path.as_str(), parts)?;
        self.owners
            .identity(owner, file.document.facts.language(), spelled)
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
        let mut path = match self.python_binding(file, head, position, work) {
            Binding::Found(binding) => python_binding_path(file, binding, work)?,
            Binding::Missing
                if names::PYTHON_BUILTINS.binary_search(&head.as_str()).is_ok()
                    && file.qualified_target(head).is_none() =>
            {
                vec!["builtins".to_owned(), head.clone()]
            }
            Binding::Missing | Binding::Blocked => return None,
        };
        path.extend(tail.iter().cloned());
        let owner = self.owners.python(&path)?;
        self.owners
            .identity(owner, file.document.facts.language(), path)
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
        let Binding::Found(binding) = file.binding(head, position, |_| true, work) else {
            return None;
        };
        self.javascript_import_target(
            file,
            self.javascript_binding_path(file, binding, work)?,
            tail,
        )
    }

    /// Resolves a written TypeScript type through its nearest import binding.
    ///
    /// An imported type with no established package target keeps its unresolved coverage.
    /// A local or ambiguous binding supplies no external observation.
    pub(super) fn record_external_typescript_type_use(
        &self,
        file: &File<'source>,
        range: ByteRange,
        work: &mut Work,
        result: &mut StaticResolution,
    ) -> Option<()> {
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
        if !matches!(
            binding.kind,
            StaticBindingKind::JavaScriptImport { .. }
                | StaticBindingKind::JavaScriptEdgeNamespace { .. }
        ) || file.javascript_reassigned(head, tail.first().map(String::as_str))
        {
            return None;
        }
        let import @ (specifier, _) = self.javascript_binding_path(file, binding, work)?;
        // A directory specifier is also eligible for the relative module resolver.
        if !dependency_specifier(specifier) || specifier.ends_with('/') {
            return None;
        }
        let occurrence = self
            .javascript_import_target(file, import, tail)
            .and_then(|target| {
                Self::external_occurrence(file, range, range, ReferenceRole::Type, target)
            });
        if let Some(occurrence) = occurrence {
            result.external.push(occurrence);
        } else {
            result.record_kind(None, EdgeKind::HasType);
        }
        Some(())
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
        path.extend(export.map(str::to_owned));
        path.extend(tail.iter().cloned());
        self.owners
            .identity(owner, file.document.facts.language(), path)
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
    let StaticBindingKind::PythonImport { module, export } = binding.kind else {
        return None;
    };
    let module = file.text(module)?;
    if module.starts_with('.') {
        return None;
    }
    let mut path = identifier_path(module, ".", work)?;
    if let Some(export) = export {
        path.push(file.text(export)?.to_owned());
    }
    Some(path)
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
