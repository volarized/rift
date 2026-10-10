//! Publication fixtures for established Python placement and unresolved facts.

use super::*;
use crate::{
    ExactPackageInput, ExactPackageLimits, PackageImportRoot, PackageImportRootOrigin,
    PackageSource,
};
use rift_core::ProjectPath;

mod aliases;
mod graph;

fn publication(
    files: &[(&str, &str)],
    prefix: Option<&str>,
    modules: &[&str],
) -> PackagePublication {
    let package = rift_protocol::read::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "click".to_owned(),
        version: "8.3.3".to_owned(),
    };
    let owner = package.owner().expect("canonical package");
    let origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Dependency { package }),
        rift_core::SourceKind::Authored,
    )
    .expect("authored package");
    let paths = files
        .iter()
        .map(|(path, _)| ProjectPath::new((*path).to_owned()).expect("physical path"))
        .collect::<Vec<_>>();
    let sources = paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect::<Vec<_>>();
    let roots = if modules.is_empty() {
        Vec::new()
    } else {
        vec![
            PackageImportRoot::new(
                prefix.map(|path| ProjectPath::new(path.to_owned()).expect("import prefix")),
                modules.iter().map(|name| (*name).to_owned()).collect(),
                if prefix.is_some() {
                    PackageImportRootOrigin::Flit
                } else {
                    PackageImportRootOrigin::Wheel
                },
            )
            .expect("accepted import roots"),
        ]
    };
    let language = Language {
        name: "python".to_owned(),
        dialect: None,
    };
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(
            u32::try_from(sources.len()).expect("file count"),
            files
                .iter()
                .map(|(_, text)| u64::try_from(text.len()).expect("source length"))
                .sum(),
        ),
    )
    .expect("bounded input")
    .with_import_roots(&roots)
    .expect("accepted placement");
    PackageAnalyzer::analyze(input, 1)
        .expect("normalized publication")
        .publication()
        .clone()
}

#[test]
fn same_module_keeps_implementation_and_stub_bindings_on_one_object() {
    let result = publication(
        &[
            (
                "src/click/core.py",
                "def echo(value):\n    \"\"\"Return value.\"\"\"\n    return value\n",
            ),
            ("src/click/core.pyi", "def echo(value: int) -> int: ...\n"),
        ],
        Some("src"),
        &["click"],
    );
    let id = "rift://symbol/pypi/pypi.org/click@8.3.3/python/click/core/echo";
    assert_eq!(result.format_revision, 3);
    assert_eq!(
        result.identity_format,
        rift_protocol::identity::SYMBOL_IDENTITY_FORMAT_REVISION
    );
    let objects = result
        .objects
        .iter()
        .filter(|object| object.id.as_ref().is_some_and(|actual| actual.0 == id))
        .collect::<Vec<_>>();
    assert_eq!(objects.len(), 1);
    let bindings = result
        .declarations
        .iter()
        .filter(|binding| binding.symbol.0 == id)
        .collect::<Vec<_>>();
    assert_eq!(bindings.len(), 2);
    assert!(
        bindings
            .iter()
            .any(|binding| binding.unit.0.ends_with("/src/click/core.py"))
    );
    assert!(
        bindings
            .iter()
            .any(|binding| binding.unit.0.ends_with("/src/click/core.pyi"))
    );
    for binding in bindings {
        assert!(binding.signature_indices.iter().all(|index| {
            usize::try_from(*index).is_ok_and(|index| index < objects[0].signatures.len())
        }));
        assert!(binding.documentation_indices.iter().all(|index| {
            usize::try_from(*index).is_ok_and(|index| index < objects[0].documentation.len())
        }));
        assert_eq!(
            binding.origin.package,
            Some(rift_protocol::read::PackageIdentity {
                manager: "pypi".to_owned(),
                registry: "pypi.org".to_owned(),
                name: "click".to_owned(),
                version: "8.3.3".to_owned(),
            })
        );
    }
    assert!(!result.coverage[0].applicability_complete);
}

#[test]
fn unproved_namespace_keeps_object_facts_and_refuses_directory_binding() {
    let result = publication(
        &[(
            "src/click/core.py",
            "def echo(value):\n    \"\"\"Return value.\"\"\"\n    return value\n",
        )],
        None,
        &[],
    );
    let object = result
        .objects
        .iter()
        .find(|object| object.name == "echo")
        .expect("retained facts");
    assert!(object.id.is_none());
    assert!(!object.signatures.is_empty());
    assert!(
        object
            .documentation
            .iter()
            .any(|documentation| documentation.text.contains("Return value."))
    );
    assert!(result.declarations.is_empty());
    assert!(!result.coverage[0].identity_complete);
    assert!(result.warnings.iter().any(|warning| matches!(warning, PackageAnalysisWarning::IdentityUnresolved { qualified_name, unit, .. }
        if qualified_name == "echo" && unit.as_ref().is_some_and(|unit| unit.0.ends_with("/src/click/core.py")))));
}

#[test]
fn malformed_and_omitted_declarations_cannot_prove_complete_coverage() {
    let omitted = format!(
        "def {}(): pass\n",
        "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1)
    );
    for source in ["def echo(".to_owned(), omitted] {
        let result = publication(&[("click/core.py", &source)], None, &["click"]);
        assert_eq!(result.units.len(), 1);
        assert_eq!(result.units[0].source, source);
        assert!(result.units[0].source_complete);
        let coverage = &result.coverage[0];
        assert!(!coverage.inventory_complete);
        assert!(!coverage.identity_complete);
        assert!(!coverage.applicability_complete);
    }
}

#[test]
fn coverage_retains_each_actual_selected_language() {
    let result = publication(
        &[
            ("click/core.py", "def echo(value): return value\n"),
            (
                "index.js",
                "export function format(value) { return value; }\n",
            ),
        ],
        None,
        &["click"],
    );
    let languages = result
        .coverage
        .iter()
        .map(|coverage| coverage.language.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(languages, ["javascript", "python"]);
    assert!(
        result
            .objects
            .iter()
            .any(|object| object.language.name == "javascript")
    );
    assert!(
        result
            .objects
            .iter()
            .any(|object| object.language.name == "python")
    );
    assert!(
        result
            .coverage
            .iter()
            .all(|coverage| coverage.inventory_complete)
    );
    assert!(
        result
            .coverage
            .iter()
            .all(|coverage| !coverage.applicability_complete)
    );
}
#[test]
fn logical_runtime_object_keeps_released_physical_binding() {
    let package = rift_protocol::read::PackageIdentity {
        manager: "cargo".into(),
        registry: "crates.io".into(),
        name: "runtime-stubs".into(),
        version: "1.0.0".into(),
    };
    let physical_owner = package.owner().expect("physical package owner");
    let physical_origin = rift_core::ContributionOrigin::new(
        Some(rift_core::SourceLocation::Dependency {
            package: package.clone(),
        }),
        rift_core::SourceKind::Generated,
    )
    .expect("physical generated source");
    let unit = rift_core::SourceUnitId::for_owner(physical_owner, "src/lib.rs")
        .expect("original package unit");
    let path = rift_core::ProjectPath::new("core/src/lib.rs").expect("analysis path");
    let source = "pub fn open() {}\n";
    let files = [crate::PackageSource::new(&path, source)
        .with_source_unit(&unit, &physical_origin)
        .expect("physical association")];
    let runtime = rift_protocol::read::RuntimeIdentity {
        runtime: "rust".into(),
        version: "1.98.1".into(),
    };
    let owner = runtime.owner().expect("logical runtime owner");
    let logical_origin = rift_core::ContributionOrigin::new(
        Some(rift_core::SourceLocation::Stdlib {
            runtime: Some(runtime.clone()),
        }),
        rift_core::SourceKind::Authored,
    )
    .expect("logical runtime origin");
    let language = rift_protocol::read::Language::from_identity_segment("rust").expect("language");
    let manifest_path = rift_core::ProjectPath::new("core/Cargo.toml").expect("manifest path");
    let metadata = [crate::PackageSource::new(
        &manifest_path,
        "[package]\nname = \"core\"\nversion = \"0.0.0\"\n",
    )];
    let input = crate::ExactPackageInput::new(
        &owner,
        &language,
        &logical_origin,
        &files,
        crate::ExactPackageLimits::new(2, 1024),
    )
    .expect("bounded runtime input")
    .with_framework_context(&metadata, &[])
    .expect("captured crate target");
    let analysis = crate::PackageAnalyzer::analyze(input, 1).expect("runtime analysis");
    let publication = analysis.publication();
    let object = publication
        .objects
        .iter()
        .find(|object| object.name == "open")
        .expect("logical object");
    let id = object.id.as_ref().expect("established runtime identity");
    let parsed =
        rift_protocol::identity::SymbolIdentity::parse(id.as_str()).expect("canonical identity");
    assert_eq!(parsed.owner(), &owner);
    assert_eq!(object.origin.runtime, Some(runtime));
    assert!(object.origin.package.is_none());
    assert_eq!(
        object.origin.source_kind,
        rift_protocol::read::SourceKind::Generated
    );
    let declaration = publication
        .declarations
        .iter()
        .find(|declaration| &declaration.symbol == id)
        .expect("physical binding");
    assert_eq!(declaration.unit.0, unit.to_string());
    assert_eq!(declaration.origin.package.as_ref(), Some(&package));
    assert!(declaration.origin.runtime.is_none());
    assert_eq!(declaration.source, source.trim_end());
    assert!(!declaration.digest.0.is_empty());
    assert_eq!(publication.units[0].path.0, "src/lib.rs");
    assert_eq!(publication.units[0].source, source);
}
