//! Test fixtures shared by the failure, walk, package, and dependency index suites.

use std::fs;
use std::path::Path;

use rift_core::ProjectPath;
use rift_dependency::{CatalogEntry, DependencyCatalog, Resolution, ResolverName};
use rift_protocol::read::{Language, PackageIdentity};
use rift_syntax::ShippedLanguage;

use super::{PackageFiles, PackageIndex, PackageIndexError, PackageIndexViolation};
use crate::workspace::{SymbolMatch, TextSourceFile};

pub(super) fn identity(manager: &str, name: &str, version: &str) -> PackageIdentity {
    PackageIdentity {
        manager: manager.to_owned(),
        name: name.to_owned(),
        version: version.to_owned(),
    }
}

pub(super) fn tokio() -> PackageIdentity {
    identity("cargo", "tokio", "1.53.1")
}

pub(super) fn language(shipped: ShippedLanguage) -> Language {
    shipped.language()
}

pub(super) fn rooted(
    identity: PackageIdentity,
    shipped: ShippedLanguage,
    root: &Path,
) -> CatalogEntry {
    CatalogEntry::dependency(identity, language(shipped), Some(root.to_path_buf()), false)
}

pub(super) fn text(path: &str, content: &str) -> TextSourceFile {
    TextSourceFile::from_content(ProjectPath::new(path).expect("path"), content.to_owned())
}

pub(super) fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture directory");
    }
    fs::write(path, bytes).expect("fixture file");
}

pub(super) fn sorted_paths(files: &PackageFiles) -> Vec<&str> {
    let mut paths: Vec<&str> = files
        .files()
        .iter()
        .map(|file| file.path().as_str())
        .collect();
    paths.sort_unstable();
    paths
}

pub(super) fn names<'a>(matches: &[SymbolMatch<'a>]) -> Vec<&'a str> {
    matches
        .iter()
        .map(|matched| matched.symbol.qualified_name.as_str())
        .collect()
}

pub(super) fn catalog(entries: Vec<CatalogEntry>) -> DependencyCatalog {
    DependencyCatalog::assemble(vec![(
        ResolverName::Cargo,
        Resolution {
            entries,
            inputs: Vec::new(),
            degradations: Vec::new(),
        },
    )])
}

pub(super) fn rust_package(name: &str, source: &str) -> PackageIndex {
    let entry = CatalogEntry::dependency(
        identity("cargo", name, "1.0.0"),
        language(ShippedLanguage::Rust),
        None,
        false,
    );
    let files = PackageFiles::new(vec![text("src/lib.rs", source)], 0);
    PackageIndex::build(&entry, &files, 1).expect("package builds")
}

pub(super) fn violation_of(error: &PackageIndexError) -> PackageIndexViolation {
    error.fault().violation()
}
