//! Packages the analyzer's tests analyze: bytes in memory, placed under one fixed package
//! identity.

use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::index::PackagePublication;
use rift_protocol::read::{Language, PackageIdentity};
use rift_syntax::{ShippedLanguage, SyntaxLimits};

use super::{PackageAnalysis, PackageAnalysisError, PackageAnalyzer};
use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};

/// The package every fixture is analyzed as.
pub(super) fn identity() -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    }
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
) -> Result<PackageAnalysis, PackageAnalysisError> {
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
