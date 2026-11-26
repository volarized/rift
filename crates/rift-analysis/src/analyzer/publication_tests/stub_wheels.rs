//! Installed stub wheels keep their typed module names and physical source paths.

use rift_protocol::read::{EdgeKind, SymbolId, SymbolKind};

use super::publication;

const PACKAGE: &str = "rift://symbol/pypi/pypi.org/click/python";

#[test]
fn wheel_stub_directory_uses_the_admitted_logical_module_and_authored_units() {
    let files = [
        (
            "p-stubs/__init__.pyi",
            "from .core import run as run\ndef open() -> int: ...\n",
        ),
        ("p-stubs/core.pyi", "def run(value: str) -> bool: ...\n"),
        ("p-stubs/native.py", "def untouched(): pass\n"),
        ("p-stubs.pyi", "def flat(): ...\n"),
        ("other-stubs/__init__.pyi", "def unlisted(): ...\n"),
    ];
    let result = publication(&files, None, &["p"]);
    for (path, symbol) in [
        ("p-stubs/__init__.pyi", "p/open"),
        ("p-stubs/core.pyi", "p/core/run"),
    ] {
        let unit = result
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("physical stub");
        let id = format!("{PACKAGE}/{symbol}");
        assert!(
            result.declarations.iter().any(|declaration| {
                declaration.symbol.as_str() == id && declaration.unit == unit.unit
            }),
            "{path}"
        );
    }
    assert!(result.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Aliases]
            && edge.from.as_str() == format!("{PACKAGE}/p/run")
            && edge.to.as_str() == format!("{PACKAGE}/p/core/run")
    }));
    assert!(result.objects.iter().all(|object| {
        object.id.as_ref().is_none_or(|id| {
            !["-stubs", "/untouched", "/flat", "/unlisted"]
                .iter()
                .any(|name| id.as_str().contains(name))
        })
    }));
    let without_wheel_root = publication(&files, None, &[]);
    assert!(
        without_wheel_root
            .objects
            .iter()
            .all(|object| object.id.is_none())
    );
}

#[test]
fn wheel_stub_namespace_exposes_only_modules_with_physical_stubs() {
    let result = publication(
        &[("p-stubs/deep/__init__.pyi", "def item() -> int: ...\n")],
        None,
        &["p"],
    );
    let mut modules = result
        .objects
        .iter()
        .filter(|object| object.kinds.contains(&SymbolKind::Module))
        .filter_map(|object| object.id.as_ref().map(SymbolId::as_str))
        .collect::<Vec<_>>();
    modules.sort_unstable();
    assert_eq!(modules, [format!("{PACKAGE}/p/deep").as_str()]);
}

#[test]
fn captured_wheel_metadata_establishes_the_original_stub_paths() {
    use rift_protocol::read::{PackageIdentity, SourceLocation};

    use super::{
        ContributionOrigin, ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource,
        ProjectPath, ShippedLanguage, SourceKind,
    };

    const MATCHED: &str = "Metadata-Version: 2.4\nName: types-widget\nVersion: 1.0.0\n\n";
    const DECLARED: &str = "widget-stubs/__init__.pyi,,\ntypes_widget-1.0.0.dist-info/RECORD,,\n";

    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "types-widget".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("checked PyPI owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("physical authored stub");
    let language = ShippedLanguage::Python.language();
    let stub = ProjectPath::new("widget-stubs/__init__.pyi").expect("physical stub path");
    let metadata = ProjectPath::new("types_widget-1.0.0.dist-info/METADATA").expect("METADATA");
    let record = ProjectPath::new("types_widget-1.0.0.dist-info/RECORD").expect("RECORD");
    let top =
        ProjectPath::new("types_widget-1.0.0.dist-info/top_level.txt").expect("top-level file");
    let source = [PackageSource::new(&stub, "def run() -> int: ...\n")];
    for (metadata_text, recorded, listed, establishes) in [
        (MATCHED, DECLARED, "widget-stubs\n", true),
        (
            "Metadata-Version: 2.4\nName: types-other\nVersion: 1.0.0\n\n",
            DECLARED,
            "widget-stubs\n",
            false,
        ),
        (
            MATCHED,
            "other-stubs/__init__.pyi,,\n",
            "widget-stubs\n",
            false,
        ),
        (MATCHED, DECLARED, "other-stubs\n", false),
    ] {
        let context = [
            PackageSource::new(&metadata, metadata_text),
            PackageSource::new(&record, recorded),
            PackageSource::new(&top, listed),
        ];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &source,
            ExactPackageLimits::new(8, 8_192),
        )
        .expect("bounded physical files")
        .with_framework_context(&context, &[])
        .expect("captured metadata");
        let analysis = PackageAnalyzer::analyze(input, 1).expect("package analyzer");
        let publication = analysis.publication();
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == stub.as_str())
            .expect("unchanged stored stub path");
        let declared = publication.declarations.iter().any(|declaration| {
            declaration.symbol.as_str()
                == "rift://symbol/pypi/pypi.org/types-widget/python/widget/run"
                && declaration.unit == unit.unit
        });
        assert_eq!(declared, establishes);
    }
}

#[test]
fn a_wheel_script_remains_separate_from_inferred_stub_import_roots() {
    use rift_core::SourceUnitId;
    use rift_protocol::identity::SymbolIdentity;
    use rift_protocol::read::{PackageIdentity, SourceLocation};

    use super::{
        ContributionOrigin, ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageSource,
        ProjectPath, ShippedLanguage, SourceKind,
    };

    const METADATA: &str = "Metadata-Version: 2.4\nName: types-widget\nVersion: 1.0.0\n\n";
    const RECORD: &str = "widget-stubs/__init__.pyi,,\nwidget,,\n";
    const STUB: &str = "from widget import invoke\n\ndef run() -> int: ...\n";
    const SCRIPT: &str = "def local():\n    return 1\n\ndef invoke():\n    return local()\n";
    const ROOT: &str = "rift://symbol/pypi/pypi.org/types-widget/python/widget/run";

    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "types-widget".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("wheel owner");
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )
    .expect("authored wheel sources");
    let stub = ProjectPath::new("widget-stubs/__init__.pyi").expect("stub path");
    let script = ProjectPath::new("widget.py").expect("script analysis path");
    let physical = SourceUnitId::for_owner(owner.clone(), "widget").expect("script unit");
    let metadata = ProjectPath::new("types_widget-1.0.0.dist-info/METADATA").expect("METADATA");
    let record = ProjectPath::new("types_widget-1.0.0.dist-info/RECORD").expect("RECORD");
    let top_level =
        ProjectPath::new("types_widget-1.0.0.dist-info/top_level.txt").expect("top-level file");
    let sources = [
        PackageSource::new(&stub, STUB),
        PackageSource::new(&script, SCRIPT)
            .with_source_unit(&physical, &origin)
            .expect("physical archive script"),
    ];
    let context = [
        PackageSource::new(&metadata, METADATA),
        PackageSource::new(&record, RECORD),
        PackageSource::new(&top_level, "widget-stubs\n"),
    ];
    let language = ShippedLanguage::Python.language();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(8, 8192),
    )
    .expect("selected wheel files")
    .with_framework_context(&context, &[])
    .expect("captured wheel metadata");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("package analyzer");
    let publication = analysis.publication();
    let root = publication
        .declarations
        .iter()
        .find(|row| row.symbol.as_str() == ROOT)
        .expect("inferred wheel root declaration");
    let physical_identity = physical.to_string();
    let script = publication
        .declarations
        .iter()
        .find(|row| {
            row.unit.as_str() == physical_identity && row.symbol.as_str().ends_with("/invoke")
        })
        .expect("authored standalone script declaration");
    let script_id = SymbolIdentity::parse(script.symbol.as_str()).expect("script identity");
    assert_eq!(script_id.qualified_path(), ["./widget", "invoke"]);
    assert!(publication.units.iter().any(|unit| {
        unit.unit == script.unit && unit.path.0 == "widget" && unit.source == SCRIPT
    }));
    assert!(publication.units.iter().any(|unit| {
        unit.unit == root.unit && unit.path.0 == "widget-stubs/__init__.pyi" && unit.source == STUB
    }));
    let called = publication
        .declarations
        .iter()
        .find(|row| row.unit == script.unit && row.symbol.as_str().ends_with("/local"))
        .expect("local declared function of the script");
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls] && edge.from == script.symbol && edge.to == called.symbol
    }));
    assert!(
        !publication
            .edges
            .iter()
            .any(|edge| { edge.kinds == [EdgeKind::Imports] && edge.to == script.symbol })
    );
}

#[test]
fn a_captured_ordinary_python_root_prevents_stub_directory_projection() {
    const RUNTIME: &str = "def run() -> int:\n    return 7\n";
    const STUB: &str = "def run() -> str: ...\n";

    for runtime_path in ["p.py", "p.pyi", "p/__init__.py", "p/__init__.pyi"] {
        let result = publication(
            &[
                (runtime_path, RUNTIME),
                ("p-stubs/__init__.pyi", STUB),
                ("q-stubs/__init__.pyi", STUB),
                ("q_extra/__init__.py", RUNTIME),
                ("q/README.txt", "not Python source"),
            ],
            None,
            &["p", "q"],
        );
        for (module, expected_path) in [("p", runtime_path), ("q", "q-stubs/__init__.pyi")] {
            let id = format!("{PACKAGE}/{module}/run");
            let paths = result
                .declarations
                .iter()
                .filter(|row| row.symbol.as_str() == id)
                .map(|row| {
                    &result
                        .units
                        .iter()
                        .find(|unit| unit.unit == row.unit)
                        .expect("matched declaration unit")
                        .path
                        .0
                })
                .collect::<Vec<_>>();
            assert_eq!(paths, [expected_path]);
        }
        assert!(
            result
                .declarations
                .iter()
                .all(|row| !row.unit.0.ends_with("/p-stubs/__init__.pyi"))
        );
    }
}
