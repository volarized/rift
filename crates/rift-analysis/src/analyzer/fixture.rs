//! Packages the analyzer's tests analyze: bytes in memory, placed under one fixed package
//! identity.

use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::index::PackagePublication;
use rift_protocol::read::{Language, PackageIdentity};
use rift_syntax::{ShippedLanguage, SyntaxLimits};

use super::{PackageAnalysis, PackageAnalyzer, RiftError};
use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};

/// The package every fixture is analyzed as.
pub(super) fn identity() -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    }
}

#[test]
fn parsed_file_preserves_source_path_digest_and_declarations() {
    let path = ProjectPath::new("src/lib.rs").expect("path");
    let text = "/// Opens a file.\npub fn café() {}\n";
    let file = super::parsed_file(
        PackageSource::new(&path, text),
        &identity(),
        &language(ShippedLanguage::Rust),
        SyntaxLimits::default(),
    )
    .expect("supported source");
    assert_eq!(file.path(), &path);
    assert_eq!(file.source(), text);
    assert_eq!(file.digest(), rift_core::FileDigest::of(text.as_bytes()));
    assert!(!file.executable());
    assert_eq!(file.syntax().symbols().len(), 1);
    assert_eq!(file.syntax().symbols()[0].name, "café");
    assert_eq!(file.syntax().source_digest(), Some(&file.digest()));
}

pub(super) fn language(shipped: ShippedLanguage) -> Language {
    shipped.language()
}

/// The dependency origin every fixture's declarations carry.
pub(super) fn origin(package: &PackageIdentity) -> ContributionOrigin {
    ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("origin")
}

pub(super) fn package_analysis(
    shipped: ShippedLanguage,
    files: Vec<(&str, &str)>,
) -> PackageAnalysis {
    package_result(shipped, files, None).expect("the package analyzes")
}

/// One package of `files` in `shipped`, analyzed under `syntax` bounds when set.
pub(super) fn package_result(
    shipped: ShippedLanguage,
    files: Vec<(&str, &str)>,
    syntax: Option<SyntaxLimits>,
) -> Result<PackageAnalysis, RiftError> {
    let package = identity();
    let language = language(shipped);
    let origin = origin(&package);
    let files: Vec<(ProjectPath, &str)> = files
        .into_iter()
        .map(|(path, text)| (ProjectPath::new(path).expect("path"), text))
        .collect();
    let sources: Vec<PackageSource<'_>> = files
        .iter()
        .map(|(path, text)| PackageSource::new(path, text))
        .collect();
    let files_max = u32::try_from(files.len()).expect("test file count");
    let bytes_max = files
        .iter()
        .map(|(_, text)| u64::try_from(text.len()).expect("test byte count"))
        .sum();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        syntax.map_or(ExactPackageLimits::new(files_max, bytes_max), |limits| {
            ExactPackageLimits::new(files_max, bytes_max).with_syntax(limits)
        }),
    )
    .expect("bounded package input");
    PackageAnalyzer::analyze(input, 1)
}

/// One package of `files` in `shipped`, analyzed.
pub(super) fn analyzed(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> PackagePublication {
    package_analysis(shipped, files).publication().clone()
}
