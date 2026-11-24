//! A file without tokens leaves the rest of its package analyzed.

#![cfg(feature = "collector")]

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::read::{Language, PackageIdentity};

#[test]
fn python_packages_keep_modules_without_tokens() {
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "pkg".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("package owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let language = Language::from_identity_segment("python").expect("Python");
    let initial = ProjectPath::new("pkg/__init__.py").expect("package module");
    let empty = ProjectPath::new("pkg/a.py").expect("module");
    let metadata_path = ProjectPath::new("pyproject.toml").expect("metadata");
    let metadata = [PackageSource::new(
        &metadata_path,
        "[build-system]\nrequires = ['flit_core']\nbuild-backend = 'flit_core.buildapi'\n[project]\nname = 'pkg'\nversion = '1.0.0'\n",
    )];
    let roots =
        [
            PackageImportRoot::new(None, vec!["pkg".to_owned()], PackageImportRootOrigin::Flit)
                .expect("import root"),
        ];
    let mut expected = None;
    for text in ["", "\n", "   \n\n", "\r\n", "# c\n"] {
        let sources = [
            PackageSource::new(&initial, "x = 1\n"),
            PackageSource::new(&empty, text),
        ];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(16, 1 << 20),
        )
        .expect("package input")
        .with_framework_context(&metadata, &[])
        .expect("package metadata")
        .with_import_roots(&roots)
        .expect("import roots");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("the package analyzes");
        let publication = analysis.publication();
        let units = publication
            .units
            .iter()
            .map(|unit| (unit.path.0.as_str(), unit.source.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            units,
            [("pkg/__init__.py", "x = 1\n"), ("pkg/a.py", text)],
            "{text:?}"
        );
        let mut declared = publication
            .declarations
            .iter()
            .map(|declaration| {
                let unit = publication
                    .units
                    .iter()
                    .find(|unit| unit.unit == declaration.unit)
                    .expect("declaration unit");
                assert!(
                    declaration.range.end <= u64::try_from(unit.source.len()).expect("length"),
                    "{text:?}"
                );
                (unit.path.0.clone(), declaration.symbol.0.clone())
            })
            .collect::<Vec<_>>();
        declared.sort();
        for (path, identity) in [
            (
                "pkg/__init__.py",
                "rift://symbol/pypi/pypi.org/pkg/python/pkg",
            ),
            ("pkg/a.py", "rift://symbol/pypi/pypi.org/pkg/python/pkg/a"),
        ] {
            assert!(
                declared
                    .iter()
                    .any(|(unit, symbol)| unit == path && symbol == identity),
                "{text:?}: {declared:?}"
            );
        }
        assert_eq!(
            &declared,
            expected.get_or_insert(declared.clone()),
            "{text:?}"
        );
    }
}
