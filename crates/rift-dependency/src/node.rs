//! What the npm and Bun resolvers share: one package namespace, one install layout.

use std::path::{Path, PathBuf};

use rift_protocol::read::{Language, PackageIdentity};

use crate::catalog::{CatalogEntry, PackageLocation, STDLIB_MANAGER, package_identity};
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
