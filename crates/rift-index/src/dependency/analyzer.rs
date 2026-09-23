use std::path::Path;

use rift_analysis::{
    ExactPackageInput, PackageAnalysisViolation, PackageInputError, PackageInputViolation,
    PackageSource,
};
use rift_core::{ContributionOrigin, Fault, SourceKind as CoreSourceKind, SourceLocation};
use rift_dependency::{CatalogEntry, PackageLocation};
use rift_protocol::read::PackageIdentity;

use super::failure::{PackageIndexError, PackageIndexFault, PackageIndexViolation};
use super::walk::PackageFiles;

pub use rift_analysis::{AnalyzedFile, PackageAnalysis, PackageAnalyzer};

pub(super) fn analyze(
    entry: &CatalogEntry,
    files: &PackageFiles,
    revision: u64,
) -> Result<PackageAnalysis, PackageIndexError> {
    let package = entry.identity();
    let origin = ContributionOrigin::new(Some(source_location(entry)), CoreSourceKind::Authored)
        .map_err(|error| {
            PackageIndexFault::new(PackageIndexViolation::Identity, package).caused_by(error)
        })?;
    let sources: Vec<PackageSource<'_>> = files
        .files()
        .iter()
        .map(|file| PackageSource::new(file.path(), file.content()))
        .collect();
    let input = ExactPackageInput::new(
        package,
        entry.language(),
        &origin,
        &sources,
        files.analysis_limits(),
    )
    .map_err(|error| {
        input_error(
            error,
            package,
            files
                .files()
                .first()
                .map(super::super::workspace::TextSourceFile::path),
        )
    })?;
    PackageAnalyzer::analyze(input, revision).map_err(analysis_error)
}

fn input_error(
    error: PackageInputError,
    package: &PackageIdentity,
    first_path: Option<&rift_core::ProjectPath>,
) -> PackageIndexError {
    let fault = error.fault();
    let (violation, field, limit, required) = match fault.violation() {
        PackageInputViolation::InvalidIdentity | PackageInputViolation::InvalidOrigin => {
            (PackageIndexViolation::Identity, None, 0, 0)
        }
        PackageInputViolation::DuplicatePath => (PackageIndexViolation::InvalidPath, None, 0, 0),
        PackageInputViolation::TooManyFiles => {
            let evidence = fault.limit_evidence();
            (
                PackageIndexViolation::PackageFilesExceeded,
                Some(super::PACKAGE_FILES_MAX_FIELD),
                evidence.as_ref().map_or(0, |value| value.limit),
                evidence.as_ref().map_or(0, |value| value.required),
            )
        }
        PackageInputViolation::TooManyBytes => {
            let evidence = fault.limit_evidence();
            (
                PackageIndexViolation::PackageBytesExceeded,
                Some(super::PACKAGE_BYTES_MAX_FIELD),
                evidence.as_ref().map_or(0, |value| value.limit),
                evidence.as_ref().map_or(0, |value| value.required),
            )
        }
    };
    let mut index_fault = PackageIndexFault::new(violation, package);
    let identity_failure = matches!(
        fault.violation(),
        PackageInputViolation::InvalidIdentity | PackageInputViolation::InvalidOrigin
    );
    let fallback_path = identity_failure
        .then(|| first_path.map(rift_core::ProjectPath::as_str))
        .flatten();
    if let Some(path) = fault.path().or(fallback_path) {
        index_fault = index_fault.at(Path::new(path));
    }
    if let Some(field) = field {
        index_fault = index_fault.breached(field, limit, required);
    }
    index_fault.caused_by(error).into()
}

fn analysis_error(error: rift_analysis::PackageAnalysisError) -> PackageIndexError {
    let fault = error.fault();
    let violation = match fault.violation() {
        PackageAnalysisViolation::Identity => PackageIndexViolation::Identity,
        PackageAnalysisViolation::Syntax => PackageIndexViolation::Syntax,
        PackageAnalysisViolation::Provider => PackageIndexViolation::Provider,
        PackageAnalysisViolation::PackageDeclarationsExceeded => {
            PackageIndexViolation::PackageDeclarationsExceeded
        }
    };
    let mut index_fault = PackageIndexFault::new(violation, fault.package());
    if let Some(path) = fault.path() {
        index_fault = index_fault.at(Path::new(path.as_str()));
    }
    index_fault.caused_by(error).into()
}

fn source_location(entry: &CatalogEntry) -> SourceLocation {
    match entry.location() {
        PackageLocation::Dependency => SourceLocation::Dependency {
            package: entry.identity().clone(),
        },
        PackageLocation::Stdlib => SourceLocation::Stdlib {},
    }
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use rift_analysis::{ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource};
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_dependency::{CatalogEntry, PackageLocation};
    use rift_protocol::canonical::canonical_json;
    use rift_syntax::ShippedLanguage;
    use sha2::{Digest as _, Sha256};

    use super::super::DependencyIndexLimits;
    use super::super::fixture::{identity, language, text};
    use super::super::walk::{PackageFiles, package_files};
    use super::analyze;

    #[test]
    fn local_adapter_matches_in_memory_package_input_bytes() {
        let package = identity("cargo", "beacon", "1.0.0");
        let language = language(ShippedLanguage::Rust);
        let entry = CatalogEntry::dependency(package.clone(), language.clone(), None, true);
        let local_files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);
        let local = analyze(&entry, &local_files, 1).expect("local input");

        let path = ProjectPath::new("src/lib.rs").expect("path");
        let in_memory_source = PackageSource::new(&path, "pub fn spawn() {}\n");
        let sources = [in_memory_source];
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin");
        let input = ExactPackageInput::new(
            &package,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(1, 19),
        )
        .expect("bounded input");
        let in_memory = PackageAnalyzer::analyze(input, 1).expect("analysis");

        assert_eq!(
            canonical_json(local.publication()).expect("local publication"),
            canonical_json(in_memory.publication()).expect("in-memory publication")
        );
    }

    #[test]
    fn cached_archive_matches_in_memory_package_input_bytes() {
        let package = identity("cargo", "beacon", "1.0.0");
        let language = language(ShippedLanguage::Rust);
        let source = "pub fn spawn() {}\n";
        let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(source.len()).expect("fixture length"));
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, "beacon-1.0.0/src/lib.rs", source.as_bytes())
            .expect("archive member");
        let archive_bytes = archive
            .into_inner()
            .expect("tar archive")
            .finish()
            .expect("gzip archive");
        let digest: [u8; 32] = Sha256::digest(&archive_bytes).into();
        let directory = tempfile::tempdir().expect("archive directory");
        let path = directory.path().join("beacon-1.0.0.crate");
        std::fs::write(&path, archive_bytes).expect("archive bytes");
        let entry = CatalogEntry::dependency(package.clone(), language.clone(), None, true)
            .with_source_archive(path, digest);
        let files =
            package_files(&entry, &DependencyIndexLimits::default()).expect("archive files");
        let archived = analyze(&entry, &files, 1).expect("archive analysis");

        let source_path = ProjectPath::new("src/lib.rs").expect("path");
        let in_memory_source = PackageSource::new(&source_path, source);
        let sources = [in_memory_source];
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin");
        let input = ExactPackageInput::new(
            &package,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(1, 19),
        )
        .expect("bounded input");
        let in_memory = PackageAnalyzer::analyze(input, 1).expect("analysis");

        assert_eq!(
            canonical_json(archived.publication()).expect("archive publication"),
            canonical_json(in_memory.publication()).expect("in-memory publication")
        );
    }

    #[test]
    fn standard_library_origin_survives_local_adapter() {
        let entry = CatalogEntry::new(
            identity("cargo", "beacon", "1.0.0"),
            PackageLocation::Stdlib,
            language(ShippedLanguage::Rust),
        );
        let files = PackageFiles::new(vec![text("src/lib.rs", "pub fn spawn() {}\n")], 0);

        let analysis = analyze(&entry, &files, 1).expect("analysis");

        assert_eq!(
            analysis.publication().symbols[0].origin.location,
            Some(rift_protocol::read::SourceLocationKind::Stdlib)
        );
    }
}
