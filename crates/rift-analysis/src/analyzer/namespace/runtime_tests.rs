use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::{identity::SymbolOwner, read::RuntimeIdentity};

use crate::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};

fn runtime(version: &str) -> (SymbolOwner, ContributionOrigin) {
    python_runtime("python", version)
}

fn python_runtime(name: &str, version: &str) -> (SymbolOwner, ContributionOrigin) {
    let runtime = RuntimeIdentity {
        runtime: name.to_owned(),
        version: version.to_owned(),
    };
    let owner = runtime.owner().expect("exact runtime owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Stdlib {
            runtime: Some(runtime),
        }),
        SourceKind::Authored,
    )
    .expect("original runtime origin");
    (owner, origin)
}

#[test]
fn runtime_library_root_preserves_original_sources_and_logical_owner() {
    assert_runtime_library_root("python");
}

#[test]
fn cpython_library_root_preserves_original_sources_and_logical_owner() {
    assert_runtime_library_root("cpython");
}

fn assert_runtime_library_root(name: &str) {
    let (owner, origin) = python_runtime(name, "3.14.7");
    let language = rift_syntax::ShippedLanguage::Python.language();
    let path = ProjectPath::new("json/__init__.py").expect("original selected path");
    let text = "def dumps(obj):\n    return str(obj)\n\ndef loads(text):\n    return text\n";
    let files = [PackageSource::new(&path, text)];
    let roots = [PackageImportRoot::runtime(owner.clone(), None).expect("selected runtime layout")];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 1024),
    )
    .expect("selected source")
    .with_import_roots(&roots)
    .expect("same owner and original layout");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("runtime publication");
    let publication = analysis.publication();
    assert_eq!(publication.owner, owner);
    assert_eq!(publication.units[0].path.0, path.as_str());
    assert_eq!(publication.units[0].source, text);
    assert!(publication.units[0].source_complete);
    assert_eq!(publication.objects.len(), 2);
    for declaration_name in ["dumps", "loads"] {
        let object = publication
            .objects
            .iter()
            .find(|object| object.name == declaration_name)
            .expect("original declaration");
        assert_eq!(
            object.id.as_ref().expect("canonical runtime object").0,
            format!("rift://symbol/stdlib/{name}@3.14.7/python/json/{declaration_name}")
        );
        assert!(
            publication
                .declarations
                .iter()
                .any(|declaration| Some(&declaration.symbol) == object.id.as_ref())
        );
    }
}

#[test]
fn runtime_root_refuses_foreign_owner_language_and_unselected_prefix() {
    let (owner, origin) = runtime("3.14.7");
    let language = rift_syntax::ShippedLanguage::Python.language();
    let path = ProjectPath::new("json/__init__.py").expect("path");
    let files = [PackageSource::new(&path, "def dumps(obj): return obj\n")];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 1024),
    )
    .expect("selected source");
    for roots in [
        [PackageImportRoot::runtime(runtime("3.13.7").0, None).expect("different exact runtime")],
        [PackageImportRoot::runtime(
            owner.clone(),
            Some(ProjectPath::new("Lib").expect("different prefix")),
        )
        .expect("captured prefix")],
        [PackageImportRoot::runtime(
            owner.clone(),
            Some(ProjectPath::new("json2").expect("boundary prefix")),
        )
        .expect("captured prefix")],
    ] {
        let error = input
            .with_import_roots(&roots)
            .expect_err("owner or selected layout mismatch");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_identity_invalid"
        );
    }
    let roots = [PackageImportRoot::runtime(owner.clone(), None).expect("root")];
    let rust = rift_syntax::ShippedLanguage::Rust.language();
    assert!(
        ExactPackageInput::new(
            &owner,
            &rust,
            &origin,
            &files,
            ExactPackageLimits::new(1, 1024)
        )
        .expect("source input")
        .with_import_roots(&roots)
        .is_err()
    );
    for invalid in [
        SymbolOwner::Local,
        SymbolOwner::Package {
            manager: "pypi".to_owned(),
            registry: "pypi.org".to_owned(),
            name: "python".to_owned(),
            version: "3.14.7".to_owned(),
        },
        SymbolOwner::Runtime {
            runtime: "node".to_owned(),
            version: "26.11.1".to_owned(),
        },
    ] {
        assert!(PackageImportRoot::runtime(invalid, None).is_err());
    }
    assert!(
        PackageImportRoot::new(
            None,
            vec!["json".to_owned()],
            PackageImportRootOrigin::Runtime
        )
        .is_err()
    );
}

#[test]
fn python_runtime_names_cannot_transfer_layout_authority() {
    let language = rift_syntax::ShippedLanguage::Python.language();
    let path = ProjectPath::new("json/__init__.py").expect("original selected path");
    let files = [PackageSource::new(&path, "def dumps(obj): return obj\n")];
    for (name, other) in [("python", "cpython"), ("cpython", "python")] {
        let (owner, origin) = python_runtime(name, "3.14.7");
        let roots = [
            PackageImportRoot::runtime(python_runtime(other, "3.14.7").0, None)
                .expect("other runtime layout"),
        ];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(1, 1024),
        )
        .expect("original selected source");
        let error = input
            .with_import_roots(&roots)
            .expect_err("different runtime name cannot transfer authority");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_identity_invalid"
        );
    }
}

#[test]
fn unknown_runtime_layout_retains_original_declarations_without_identity() {
    let (owner, origin) = runtime("3.14.7");
    let language = rift_syntax::ShippedLanguage::Python.language();
    let path = ProjectPath::new("json/__init__.py").expect("path");
    let text = "def dumps(obj): return obj\ndef loads(text): return text\n";
    let files = [PackageSource::new(&path, text)];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 1024),
    )
    .expect("original source without layout authority");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("raw runtime publication");
    let publication = analysis.publication();
    assert_eq!(publication.units[0].source, text);
    assert_eq!(publication.objects.len(), 2);
    assert!(publication.objects.iter().all(|object| object.id.is_none()));
    assert!(publication.declarations.is_empty());
}

#[test]
fn runtime_layout_admits_more_than_package_module_bound_without_module_table() {
    let (owner, _) = runtime("3.14.7");
    let roots = [PackageImportRoot::runtime(
        owner,
        Some(ProjectPath::new("Lib").expect("original prefix")),
    )
    .expect("original runtime layout")];
    for index in 0..128 {
        assert_eq!(
            super::python_module(&format!("Lib/module_{index}/__init__.py"), &roots),
            Some(vec![format!("module_{index}")])
        );
    }
    assert_eq!(
        super::python_module("Library/json/__init__.py", &roots),
        None
    );
    assert_eq!(super::python_module("Lib/json/bad-name.py", &roots), None);
    assert_eq!(super::python_module("Lib/__init__.py", &roots), None);
}

#[test]
fn runtime_root_cannot_assign_same_source_bytes_to_package_or_local_owner() {
    let (owner, _) = runtime("3.14.7");
    let roots = [PackageImportRoot::runtime(owner, None).expect("selected runtime layout")];
    let language = rift_syntax::ShippedLanguage::Python.language();
    let path = ProjectPath::new("json/__init__.py").expect("original path");
    let files = [PackageSource::new(&path, "def dumps(obj): return obj\n")];
    let package = rift_protocol::read::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "python".to_owned(),
        version: "3.14.7".to_owned(),
    };
    let owner = package.owner().expect("different defining owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 1024),
    )
    .expect("same bytes under package owner");
    assert!(input.with_import_roots(&roots).is_err());
    assert!(
        crate::NamespaceInput::project(
            &SymbolOwner::Local,
            &language,
            &files,
            &[],
            &roots,
            ExactPackageLimits::new(1, 1024)
        )
        .is_err()
    );
    assert!(
        PackageImportRoot::runtime(
            SymbolOwner::Runtime {
                runtime: "python".to_owned(),
                version: "3.14".to_owned()
            },
            None
        )
        .is_err()
    );
    assert!(
        PackageImportRoot::runtime(
            runtime("3.14.7").0,
            Some(ProjectPath::new("").expect("captured root path"))
        )
        .is_err()
    );
}
