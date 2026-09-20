//! What the npm and Bun resolvers share: one package namespace, one install layout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::{Language, PackageIdentity};
use serde::Deserialize;

use crate::catalog::{CatalogEntry, PackageLocation, STDLIB_MANAGER, package_identity};
use crate::context::is_whole_version;
use crate::manifest::StaticFileFailure;
use crate::resolver::Inspector;

/// The package namespace every npm and Bun entry belongs to: both install from the npm
/// registry, so one package has one identity whichever tool pinned it.
pub(crate) const NPM_MANAGER: &str = "npm";
/// The manifest file name both resolvers claim.
pub(crate) const PACKAGE_MANIFEST_FILE_NAME: &str = "package.json";
/// The directory installed packages sit under, beside the manifest.
pub(crate) const NODE_MODULES_DIRECTORY_NAME: &str = "node_modules";
/// The npm package carrying the TypeScript compiler and its `lib.*.d.ts` library.
pub(crate) const TYPESCRIPT_PACKAGE_NAME: &str = "typescript";
/// The directory below the TypeScript package holding the library declarations.
pub(crate) const TYPESCRIPT_LIBRARY_DIRECTORY_NAME: &str = "lib";
/// The language whose syntax provider parses installed packages' declaration files.
const TYPESCRIPT_LANGUAGE_NAME: &str = "typescript";

/// The TypeScript language, with no dialect.
#[must_use]
pub(crate) fn typescript_language() -> Language {
    Language {
        name: TYPESCRIPT_LANGUAGE_NAME.to_owned(),
        dialect: None,
    }
}

/// One npm package identity.
#[must_use]
pub(crate) fn npm_identity(name: &str, version: &str) -> PackageIdentity {
    package_identity(NPM_MANAGER, name, version)
}

/// One dependency entry for the package installed at `install_path` below the manifest directory.
/// The install directory is its source root when the inspector finds it.
///
/// `install_path` is the lockfile's own spelling, `node_modules/<name>` or a nested
/// `node_modules/<parent>/node_modules/<name>`, with forward slashes.
#[must_use]
pub(crate) fn installed_entry(
    inspector: &mut dyn Inspector,
    manifest_directory: &Path,
    install_path: &str,
    name: &str,
    version: &str,
    declared_directly: bool,
) -> CatalogEntry {
    let root = manifest_directory.join(install_path);
    let source_root = inspector.directory_exists(&root).then_some(root);
    CatalogEntry::dependency(
        npm_identity(name, version),
        typescript_language(),
        source_root,
        declared_directly,
    )
}

/// The TypeScript library entry for a workspace whose lockfile pins `typescript`.
///
/// The library is the `lib.*.d.ts` set the installed compiler ships, so it is cataloged as
/// the standard library of that TypeScript version: `stdlib/typescript@<version>` with the
/// package's `lib` directory as its root when the package is installed.
#[must_use]
pub(crate) fn typescript_library_entry(
    inspector: &mut dyn Inspector,
    manifest_directory: &Path,
    version: &str,
) -> CatalogEntry {
    let identity = package_identity(STDLIB_MANAGER, TYPESCRIPT_PACKAGE_NAME, version);
    let mut entry = CatalogEntry::new(identity, PackageLocation::Stdlib, typescript_language());
    let library = hoisted_install_root(manifest_directory, TYPESCRIPT_PACKAGE_NAME)
        .join(TYPESCRIPT_LIBRARY_DIRECTORY_NAME);
    if inspector.directory_exists(&library) {
        entry = entry.with_source_root(library);
    }
    entry
}

/// The directory a hoisted package installs to: `node_modules/<name>` beside the manifest.
#[must_use]
pub(crate) fn hoisted_install_root(manifest_directory: &Path, name: &str) -> PathBuf {
    manifest_directory
        .join(NODE_MODULES_DIRECTORY_NAME)
        .join(name)
}

/// The character opening a scoped package name, `@scope/name`.
pub(crate) const SCOPE_PREFIX: char = '@';
/// The character between a package name and its version in a reference.
pub(crate) const NAME_VERSION_SEPARATOR: char = '@';
/// The version prefix of an alias: the real `<name>@<version>` follows it.
pub(crate) const ALIAS_VERSION_PREFIX: &str = "npm:";
/// The version prefixes naming bytes no public registry serves: a path, a link, a
/// repository, a tarball, another package of this workspace, or a catalog entry the
/// workspace root resolves.
const LOCAL_VERSION_PREFIXES: [&str; 10] = [
    "file:",
    "link:",
    "portal:",
    "workspace:",
    "catalog:",
    "git:",
    "git+",
    "github:",
    "http:",
    "https:",
];

/// The separator a repository shorthand carries: `user/repo`, with an optional `#ref`.
/// No registry version range holds one, so a value carrying it names a repository.
const SHORTHAND_SEPARATOR: char = '/';

/// Splits `<name>@<version>` at the first `@` past a scope's own.
///
/// A name carries no `@` but the scope's, so `@types/react@19.2.18` names `@types/react`,
/// and a version spelled as a URL keeps every `@` it carries. Absent when either side
/// is empty.
pub(crate) fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let scope_bytes = usize::from(reference.starts_with(SCOPE_PREFIX));
    let separator = reference[scope_bytes..].find(NAME_VERSION_SEPARATOR)? + scope_bytes;
    let name = &reference[..separator];
    let version = &reference[separator + NAME_VERSION_SEPARATOR.len_utf8()..];
    let name_present = !name.is_empty();
    let version_present = !version.is_empty();
    (name_present && version_present).then_some((name, version))
}

/// Whether a public registry serves the package one npm version text names.
///
/// A prefixed locator and a bare `user/repo` shorthand both name bytes this machine
/// resolves, so neither is a package a global index could answer for.
pub(crate) fn version_availability(version: &str) -> PackageAvailability {
    let prefixed = LOCAL_VERSION_PREFIXES
        .iter()
        .any(|prefix| version.starts_with(prefix));
    let elsewhere = prefixed || version.contains(SHORTHAND_SEPARATOR);
    if elsewhere {
        PackageAvailability::LocalOnly
    } else {
        PackageAvailability::Canonical
    }
}

/// The selector one npm version text states.
///
/// npm reads a bare `1.2.3` as that exact release, so a whole version pins and every
/// range, tag, or locator is a requirement.
pub(crate) fn npm_selector(version: &str) -> PackageSelector {
    if is_whole_version(version) {
        PackageSelector::Version(version.to_owned())
    } else {
        PackageSelector::Requirement(version.to_owned())
    }
}

/// The `package.json` document, the dependency maps the static context reads.
#[derive(Deserialize)]
pub(crate) struct PackageManifest {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

impl PackageManifest {
    /// One entry per declared dependency, in map then key order.
    pub(crate) fn declared(&self) -> impl Iterator<Item = PackageContextEntry> {
        [
            &self.dependencies,
            &self.dev_dependencies,
            &self.optional_dependencies,
        ]
        .into_iter()
        .flat_map(|map| map.iter())
        .map(|(key, version)| declared_entry(key, version))
    }
}

/// Parses `package.json` bytes, naming the parser's message when they are not its
/// document.
pub(crate) fn parse_package_manifest(bytes: &[u8]) -> Result<PackageManifest, StaticFileFailure> {
    serde_json::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(PACKAGE_MANIFEST_FILE_NAME, error.to_string())
    })
}

/// The entry one `package.json` dependency value declares. An `npm:` value aliases
/// another package, and the entry names the package it aliases.
fn declared_entry(key: &str, version: &str) -> PackageContextEntry {
    let (name, version) = version
        .strip_prefix(ALIAS_VERSION_PREFIX)
        .and_then(split_reference)
        .unwrap_or((key, version));
    PackageContextEntry::new(
        NPM_MANAGER,
        name,
        npm_selector(version),
        version_availability(version),
    )
}
