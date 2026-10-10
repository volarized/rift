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
        registry: "crates.io".to_owned(),
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
        &identity().owner().expect("fixture owner"),
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
    package_result_with_metadata(shipped, files, syntax, None)
}

pub(super) fn package_result_with_metadata(
    shipped: ShippedLanguage,
    files: Vec<(&str, &str)>,
    syntax: Option<SyntaxLimits>,
    metadata: Option<(&str, &str)>,
) -> Result<PackageAnalysis, RiftError> {
    let mut package = identity();
    match shipped {
        ShippedLanguage::JavaScript
        | ShippedLanguage::TypeScript
        | ShippedLanguage::TypeScriptTsx => {
            package.manager = "npm".to_owned();
            package.registry = "registry.npmjs.org".to_owned();
        }
        ShippedLanguage::Python => {
            package.manager = "pypi".to_owned();
            package.registry = "pypi.org".to_owned();
        }
        _ => {}
    }
    let owner = package.owner().expect("fixture owner");
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
    let python_module = if shipped == ShippedLanguage::Python {
        if files
            .iter()
            .any(|(path, _)| matches!(path.as_str(), "mod.py" | "mod.pyi"))
        {
            Some("mod")
        } else if files
            .iter()
            .any(|(path, _)| path.as_str().starts_with("pkg/"))
        {
            Some("pkg")
        } else {
            None
        }
    } else {
        None
    };
    let default_metadata = fixture_metadata(shipped, &files, python_module);
    let (metadata_name, metadata_text) = metadata.unwrap_or(default_metadata);
    let metadata_path = ProjectPath::new(metadata_name).expect("fixture metadata path");
    let metadata = [PackageSource::new(&metadata_path, metadata_text)];
    let context = if (shipped == ShippedLanguage::Rust
        || python_module.is_some()
        || metadata_name == "package.json")
        && !files.iter().any(|(path, _)| path.as_str() == metadata_name)
    {
        metadata.as_slice()
    } else {
        &[]
    };
    let files_max = u32::try_from(files.len() + context.len()).expect("test file count");
    let bytes_max = files
        .iter()
        .map(|(_, text)| u64::try_from(text.len()).expect("test byte count"))
        .chain(
            context
                .iter()
                .map(|_| u64::try_from(metadata_text.len()).expect("metadata byte count")),
        )
        .sum();
    let roots = python_module
        .map(|module| {
            crate::PackageImportRoot::new(
                None,
                vec![module.to_owned()],
                if module == "mod" {
                    crate::PackageImportRootOrigin::PyModules
                } else {
                    crate::PackageImportRootOrigin::Flit
                },
            )
            .expect("captured fixture import root")
        })
        .into_iter()
        .collect::<Vec<_>>();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        syntax.map_or(ExactPackageLimits::new(files_max, bytes_max), |limits| {
            ExactPackageLimits::new(files_max, bytes_max).with_syntax(limits)
        }),
    )
    .expect("bounded package input")
    .with_framework_context(context, &[])
    .expect("captured fixture package metadata")
    .with_import_roots(&roots)
    .expect("captured fixture Python module placement");
    PackageAnalyzer::analyze(input, 1)
}

fn fixture_metadata(
    shipped: ShippedLanguage,
    files: &[(ProjectPath, &str)],
    python_module: Option<&str>,
) -> (&'static str, &'static str) {
    match python_module {
        Some("mod") => (
            "setup.py",
            "from setuptools import setup\nsetup(py_modules=[\"mod\"])\n",
        ),
        Some("pkg") => (
            "pyproject.toml",
            "[build-system]\nbuild-backend = \"flit_core.buildapi\"\n[tool.flit.module]\nname = \"pkg\"\n",
        ),
        _ if matches!(
            shipped,
            ShippedLanguage::JavaScript | ShippedLanguage::TypeScript
        ) && files.iter().any(|(path, _)| path.as_str() == "index.d.ts")
            && files.iter().any(|(path, _)| path.as_str() == "index.js") =>
        {
            (
                "package.json",
                "{\"name\":\"beacon\",\"version\":\"1.0.0\",\"types\":\"index.d.ts\",\"main\":\"index.js\"}",
            )
        }
        _ => (
            "Cargo.toml",
            "[package]\nname = \"beacon\"\nversion = \"1.0.0\"\n",
        ),
    }
}

/// One package of `files` in `shipped`, analyzed.
pub(super) fn analyzed(shipped: ShippedLanguage, files: Vec<(&str, &str)>) -> PackagePublication {
    package_analysis(shipped, files).publication().clone()
}
