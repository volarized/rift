//! Publication fixtures for established Python placement and unresolved facts.

use super::*;
use crate::{
    ExactPackageInput, ExactPackageLimits, PackageImportRoot, PackageImportRootOrigin,
    PackageSource,
};
use rift_core::ProjectPath;

mod aliases;
mod annotations;
mod commonjs_literals;
mod declaration_parameters;
mod facts;
mod forwarded_names;
mod graph;
mod initializer_argument_properties;
mod initializers;
mod inline_import_types;
mod internal_modules;
mod mixed_shorthand;
mod module_imports;
mod overload_parameters;
mod returned_callables;
mod rust_guard;
mod rust_implementations;
mod rust_local_bindings;
mod rust_root_imports;
mod stub_wheels;

fn publication(
    files: &[(&str, &str)],
    prefix: Option<&str>,
    modules: &[&str],
) -> PackagePublication {
    publication_with_configuration(
        files,
        prefix,
        modules,
        rift_protocol::configuration::PackageConfiguration::default(),
    )
}

fn publication_with_configuration(
    files: &[(&str, &str)],
    prefix: Option<&str>,
    modules: &[&str],
    configuration: rift_protocol::configuration::PackageConfiguration,
) -> PackagePublication {
    python_publication_with_limits(files, prefix, modules, configuration, None)
}

fn python_publication_with_limits(
    files: &[(&str, &str)],
    prefix: Option<&str>,
    modules: &[&str],
    configuration: rift_protocol::configuration::PackageConfiguration,
    edges_max: Option<usize>,
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
    let limits = ExactPackageLimits::new(
        u32::try_from(sources.len()).expect("file count"),
        files
            .iter()
            .map(|(_, text)| u64::try_from(text.len()).expect("source length"))
            .sum(),
    )
    .with_publication(configuration)
    .expect("validated publication bounds");
    let limits = edges_max.map_or(limits, |edges| {
        limits.with_edges(edges).expect("validated edge bound")
    });
    let input = ExactPackageInput::new(&owner, &language, &origin, &sources, limits)
        .expect("bounded input")
        .with_import_roots(&roots)
        .expect("accepted placement");
    PackageAnalyzer::analyze(input, 1)
        .expect("normalized publication")
        .publication()
        .clone()
}

#[test]
fn declaration_bound_keeps_original_units_and_reports_incomplete_coverage() {
    let files = [
        (
            "src/click/core.py",
            "def echo(value):\n    \"\"\"Return value.\"\"\"\n    return value\n",
        ),
        ("src/click/core.pyi", "def echo(value: int) -> int: ...\n"),
    ];
    let result = publication_with_configuration(
        &files,
        Some("src"),
        &["click"],
        rift_protocol::configuration::PackageConfiguration {
            symbols: 1,
            ..rift_protocol::configuration::PackageConfiguration::default()
        },
    );
    let id = "rift://symbol/pypi/pypi.org/click/python/click/core";
    assert_eq!(result.objects.len(), 1);
    assert_eq!(
        result.objects[0].id.as_ref().map(SymbolId::as_str),
        Some(id)
    );
    assert_eq!(result.declarations.len(), 1);
    let binding = &result.declarations[0];
    assert_eq!(binding.symbol.as_str(), id);
    assert!(binding.unit.0.ends_with("/src/click/core.py"));
    let start = usize::try_from(binding.range.start).expect("source start");
    let end = usize::try_from(binding.range.end).expect("source end");
    assert_eq!(binding.source, files[0].1[start..end]);
    assert!(binding.source_complete);
    assert!(!binding.digest.0.is_empty());
    assert_eq!(
        binding.origin.package,
        Some(rift_protocol::read::PackageIdentity {
            manager: "pypi".to_owned(),
            registry: "pypi.org".to_owned(),
            name: "click".to_owned(),
            version: "8.3.3".to_owned(),
        })
    );
    assert_eq!(result.units.len(), files.len());
    for (path, source) in files {
        let unit = result
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("original source unit");
        assert_eq!(unit.source, source);
        assert!(unit.source_complete);
    }
    assert_eq!(
        result
            .warnings
            .iter()
            .filter(|warning| matches!(warning,
            PackageAnalysisWarning::PublicationTruncated { collection, bound }
                if collection == "declarations" && *bound == 1))
            .count(),
        1
    );
    assert!(result.coverage[0].inventory_complete);
    assert!(!result.coverage[0].identity_complete);
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
    let id = "rift://symbol/pypi/pypi.org/click/python/click/core/echo";
    assert_eq!(result.format_revision, PACKAGE_PUBLICATION_FORMAT_REVISION);
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
    let name = "x".repeat(rift_core::PROVIDER_SYMBOL_ID_BYTES_MAX + 1);
    let refused = format!("def {name}(): pass\n");
    for source in ["def echo(".to_owned(), refused] {
        let result = publication(&[("click/core.py", &source)], None, &["click"]);
        assert_eq!(result.units.len(), 1);
        assert_eq!(result.units[0].source, source);
        assert!(result.units[0].source_complete);
        let coverage = &result.coverage[0];
        assert!(coverage.inventory_complete);
        assert!(!coverage.identity_complete);
        assert!(!coverage.applicability_complete);
        if source.contains(&name) {
            let objects = result
                .objects
                .iter()
                .filter(|object| object.name == name)
                .collect::<Vec<_>>();
            assert_eq!(objects.len(), 1);
            assert!(objects[0].id.is_none());
            assert_eq!(result.declarations.len(), 1);
            assert_eq!(
                result.declarations[0].symbol.as_str(),
                "rift://symbol/pypi/pypi.org/click/python/click/core"
            );
            let range = result
                .warnings
                .iter()
                .find_map(|warning| match warning {
                    rift_protocol::index::PackageAnalysisWarning::ObjectUnavailable {
                        unit: Some(unit),
                        range: Some(range),
                        qualified_name,
                        field,
                        bound,
                    } if unit == &result.units[0].unit && qualified_name == &name => {
                        assert_eq!(field, "name");
                        assert_eq!(
                            *bound,
                            u64::try_from(rift_protocol::read::SYMBOL_NAME_CHARACTERS_MAX)
                                .expect("public name bound")
                        );
                        Some(range)
                    }
                    _ => None,
                })
                .expect("retained object public refusal");
            let start = usize::try_from(range.start).expect("range start");
            let end = usize::try_from(range.end).expect("range end");
            assert_eq!(source.get(start..end), Some(source.trim_end()));
        }
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

#[test]
fn a_selected_python_script_has_file_declarations_and_no_import_module() {
    use rift_core::SourceUnitId;

    const TEXT: &str = "#!/usr/bin/env python
from demo.example import sample

def greet():
    return 'hello'

def invoke():
    return greet()

def package_call():
    return sample()
";
    let package = rift_protocol::read::PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "demo".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner().expect("package owner");
    let origin = ContributionOrigin::new(
        Some(rift_core::SourceLocation::Dependency { package }),
        rift_core::SourceKind::Authored,
    )
    .expect("authored source");
    let root = ProjectPath::new("demo/__init__.py").expect("package root");
    let module = ProjectPath::new("demo/example.py").expect("package module");
    let script = ProjectPath::new("bin/demo.py").expect("analysis path");
    let outside = ProjectPath::new("bin/other.py").expect("unselected module");
    let unit = SourceUnitId::for_owner(owner.clone(), "bin/demo").expect("physical script");
    let sources = [
        PackageSource::new(&root, ""),
        PackageSource::new(
            &module,
            "from bin.demo import greet

def sample():
    return 1

def importer():
    return greet()
",
        ),
        PackageSource::new(&script, TEXT)
            .with_source_unit(&unit, &origin)
            .expect("script placement"),
        PackageSource::new(
            &outside,
            "def other():
    return 2
",
        ),
    ];
    let roots = [PackageImportRoot::new(
        None,
        vec!["demo".to_owned()],
        PackageImportRootOrigin::Wheel,
    )
    .expect("import root")];
    let language = ShippedLanguage::Python.language();
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(4, 1 << 20),
    )
    .expect("package input")
    .with_import_roots(&roots)
    .expect("selected import root");
    let analysis = PackageAnalyzer::analyze(input, 1).expect("collected package");
    assert_archive_script_facts(analysis.publication(), TEXT);
}

fn assert_archive_script_facts(publication: &PackagePublication, text: &str) {
    let physical = publication
        .units
        .iter()
        .find(|record| record.path.0 == "bin/demo")
        .expect("original script source");
    assert_eq!(physical.source, text);
    assert_eq!(
        physical.unit.as_str(),
        "rift://source/pypi/pypi.org/demo/bin/demo"
    );
    let declarations = publication
        .declarations
        .iter()
        .filter(|row| row.unit == physical.unit)
        .collect::<Vec<_>>();
    assert_eq!(declarations.len(), 4);
    let greet = declarations
        .iter()
        .find(|row| row.symbol.as_str().ends_with("/greet"))
        .expect("script declaration");
    let identity = SymbolIdentity::parse(greet.symbol.as_str()).expect("file-based identity");
    assert_eq!(identity.qualified_path(), ["./bin/demo", "greet"]);
    assert_eq!(
        greet.range.start,
        u64::try_from(text.find("def greet").expect("written declaration")).expect("byte offset")
    );
    let local_calls = publication
        .edges
        .iter()
        .filter(|edge| edge.kinds == [EdgeKind::Calls] && edge.to == greet.symbol)
        .collect::<Vec<_>>();
    assert_eq!(local_calls.len(), 1);
    assert!(local_calls[0].from.as_str().ends_with("/invoke"));
    assert!(publication.edges.iter().any(|edge| {
        edge.kinds == [EdgeKind::Calls]
            && edge.from.as_str().ends_with("/package_call")
            && edge.to.as_str().ends_with("/demo/example/sample")
    }));
    assert!(
        !publication
            .edges
            .iter()
            .any(|edge| { edge.kinds == [EdgeKind::Imports] && edge.to == greet.symbol })
    );
    let outside_unit = publication
        .units
        .iter()
        .find(|record| record.path.0 == "bin/other.py")
        .expect("other source retained");
    assert!(
        !publication
            .declarations
            .iter()
            .any(|row| row.unit == outside_unit.unit)
    );
}

#[test]
fn a_selected_python_script_under_an_import_root_retains_its_written_path() {
    const BODY: &str = "def leaf() -> int:\n    return 1\n\n\
        def execute() -> int:\n    return leaf()\n";
    let publication = publication(
        &[
            ("src/runner/__init__.py", ""),
            ("src/runner/hooks/__init__.py", ""),
            ("src/runner/hooks/hook-run.py", BODY),
            ("src/runner/hooks/hook_run.py", BODY),
            ("src/runner/hooks/hook-run.extra.py", BODY),
            ("src/runner/hooks/hook-run.pyi", BODY),
            ("src/other/hook-run.py", BODY),
        ],
        Some("src"),
        &["runner"],
    );
    for (path, owner) in [
        (
            "src/runner/hooks/hook-run.py",
            "./src/runner/hooks/hook-run.py",
        ),
        ("src/runner/hooks/hook_run.py", "runner"),
        (
            "src/runner/hooks/hook-run.extra.py",
            "./src/runner/hooks/hook-run.extra.py",
        ),
    ] {
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("selected source");
        let declared = |name: &str| {
            let row = publication
                .declarations
                .iter()
                .find(|row| row.unit == unit.unit && row.symbol.as_str().ends_with(name))
                .expect("written function declaration");
            let identity = SymbolIdentity::parse(row.symbol.as_str()).expect("function identity");
            assert_eq!(identity.qualified_path()[0], owner);
            &row.symbol
        };
        let (leaf, execute) = (declared("/leaf"), declared("/execute"));
        assert!(publication.edges.iter().any(|edge| {
            edge.kinds == [EdgeKind::Calls]
                && &edge.from == execute
                && &edge.to == leaf
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.unit == unit.unit)
        }));
    }
    for path in ["src/runner/hooks/hook-run.pyi", "src/other/hook-run.py"] {
        let unit = publication
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("retained source");
        assert!(
            !publication
                .declarations
                .iter()
                .any(|row| row.unit == unit.unit)
        );
    }
}

#[test]
fn unaliased_python_imports_agree_on_their_package_root() {
    use rift_protocol::read::EdgeKind;

    const CHECKED: &str = concat!(
        "import demo\nimport demo.parts\nimport demo.other\n",
        "def read(value: demo.parts.Item):\n",
        "    demo.top()\n    return demo.parts.Item()\n",
        "def shadowed(demo):\n    return demo.parts.Item()\n",
    );
    const ALIASED: &str =
        "import demo\nimport demo.parts as demo\ndef read():\n    return demo.top()\n";
    const ASSIGNED: &str =
        "import demo\nimport demo.parts\ndemo = 1\ndef read():\n    return demo.top()\n";
    let files = [
        ("demo/__init__.py", "def top(): return 1\n"),
        ("demo/parts.py", "class Item: pass\n"),
        ("demo/other.py", "def other(): return 2\n"),
        ("demo/checked.py", CHECKED),
        ("demo/aliased.py", ALIASED),
        ("demo/assigned.py", ASSIGNED),
    ];
    let result = publication(&files, None, &["demo"]);
    let targets = |path: &str, kind: EdgeKind| {
        let unit = &result
            .units
            .iter()
            .find(|unit| unit.path.0 == path)
            .expect("source")
            .unit;
        result.edges.iter().filter(move |edge| {
            edge.kinds == [kind]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| &site.unit == unit)
        })
    };
    let expected = ["/demo/top", "/demo/parts/Item"];
    let calls = targets("demo/checked.py", EdgeKind::Calls).collect::<Vec<_>>();
    assert_eq!(calls.len(), expected.len());
    for suffix in expected {
        assert!(calls.iter().any(|edge| edge.to.as_str().ends_with(suffix)));
    }
    assert!(
        targets("demo/checked.py", EdgeKind::HasType)
            .any(|edge| edge.to.as_str().ends_with("/demo/parts/Item"))
    );
    for path in ["demo/aliased.py", "demo/assigned.py"] {
        assert!(
            !targets(path, EdgeKind::Calls).any(|edge| edge.to.as_str().ends_with("/demo/top"))
        );
    }
}

#[test]
fn python_qualified_member_calls_keep_class_and_runtime_reads() {
    use rift_protocol::read::EdgeKind;

    const SOURCE: &str = concat!(
        "import demo.parents\nimport demo.other\nimport time\nimport time\n",
        "class Tone(demo.parents.Base):\n    pass\n",
        "def build():\n    return Tone.make()\n",
        "def read_clock():\n    return time.time()\n",
        "def shadowed(Tone, time):\n    return Tone.make(), time.time()\n",
    );
    let files = [
        ("demo/__init__.py", ""),
        (
            "demo/parents.py",
            "class Base:\n    @classmethod\n    def make(cls): return cls()\n",
        ),
        ("demo/other.py", "def other(): return None\n"),
        ("demo/child.py", SOURCE),
    ];
    let result = publication(&files, None, &["demo"]);
    let unit = &result
        .units
        .iter()
        .find(|file| file.path.0 == "demo/child.py")
        .expect("physical source")
        .unit;
    for (written, qualifier, callee, name) in [
        (
            "Tone.make()",
            "/demo/child/Tone",
            "/demo/parents/Base/make",
            "Tone",
        ),
        (
            "time.time()",
            "/cpython/python/time",
            "/cpython/python/time/time",
            "time",
        ),
    ] {
        let start =
            u64::try_from(SOURCE.find(written).expect("written call")).expect("call offset");
        let width = u64::try_from(name.len()).expect("qualifier length");
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Reads]
                    && edge.to.as_str().ends_with(qualifier)
                    && edge.occurrence.as_ref().is_some_and(|site| {
                        &site.unit == unit
                            && site.range.start == start
                            && site.range.end == start + width
                    })
            }),
            "{qualifier}"
        );
        assert!(
            result.edges.iter().any(|edge| {
                edge.kinds == [EdgeKind::Calls]
                    && edge.to.as_str().ends_with(callee)
                    && edge
                        .occurrence
                        .as_ref()
                        .is_some_and(|site| &site.unit == unit && site.range.start == start)
            }),
            "{callee}"
        );
        let shadow =
            u64::try_from(SOURCE.rfind(written).expect("shadowed call")).expect("shadowed offset");
        assert!(result.edges.iter().all(|edge| {
            !(edge.to.as_str().ends_with(qualifier) || edge.to.as_str().ends_with(callee))
                || edge
                    .occurrence
                    .as_ref()
                    .is_none_or(|site| &site.unit != unit || site.range.start != shadow)
        }));
    }
}

#[test]
fn python_property_accessors_retain_each_written_receiver() {
    use rift_protocol::read::EdgeKind;

    let source = concat!(
        "class Box:\n",
        "    def __init__(self, initial: int):\n        self._amount = initial\n",
        "    @property\n    def amount(self) -> int:\n        return self._amount\n",
        "    @amount.setter\n    def amount(self, value: int):\n        self._amount = value\n",
        "    @amount.deleter\n    def amount(self):\n        del self._amount\n",
        "    def control(self, checked: int):\n        self._amount = checked\n",
    );
    let files = [("demo/__init__.py", source)];
    let result = publication(&files, None, &["demo"]);
    let property_receiver = |id: &str| id.ends_with("/Box/amount/self");
    let offsets = |needle: &str| -> Vec<u64> {
        source
            .match_indices(needle)
            .map(|(offset, text)| {
                u64::try_from(offset + text.find("self").expect("written receiver"))
                    .expect("receiver range")
            })
            .collect()
    };
    let declarations = |suffix: &str| -> Vec<u64> {
        let mut starts = result
            .declarations
            .iter()
            .filter(|entry| entry.symbol.as_str().ends_with(suffix))
            .map(|entry| entry.range.start)
            .collect::<Vec<_>>();
        starts.sort_unstable();
        starts
    };
    assert_eq!(declarations("/Box/amount/self"), offsets("def amount(self"));
    for receiver in result
        .declarations
        .iter()
        .filter(|entry| property_receiver(entry.symbol.as_str()))
    {
        assert_eq!(receiver.source, "self");
        assert_eq!(receiver.range.end - receiver.range.start, 4);
    }
    assert_eq!(declarations("/Box/amount").len(), 3);
    assert_eq!(declarations("/Box/amount/value").len(), 1);
    assert_eq!(
        declarations("/Box/control/self"),
        offsets("def control(self")
    );
    let mut reads = result
        .edges
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Reads] && edge.to.as_str().ends_with("/Box/amount/self")
        })
        .map(|edge| edge.occurrence.as_ref().expect("authored read").range.start)
        .collect::<Vec<_>>();
    reads.sort_unstable();
    assert!(reads.contains(&offsets("return self._amount")[0]));

    assert_python_property_receiver_priority(&files, &result, property_receiver);

    for refused in [
        source.replace("@amount.setter", "@other.setter"),
        source.replace("@amount.setter", "@amount.setter()"),
        source.replace("class Box:", "property = custom\nclass Box:"),
        source.replace("@amount.setter", "amount = replacement\n    @amount.setter"),
        source.replace("@amount.setter", "del amount\n    @amount.setter"),
        source.replace("class Box:", "del property\nclass Box:"),
        source.replace("@amount.deleter", "@amount.setter"),
    ] {
        let result = publication(&[("demo/__init__.py", &refused)], None, &["demo"]);
        assert!(
            result
                .declarations
                .iter()
                .all(|entry| { !entry.symbol.as_str().ends_with("/Box/amount/self") })
        );
        assert!(
            result
                .declarations
                .iter()
                .any(|entry| { entry.symbol.as_str().ends_with("/Box/control/self") })
        );
    }
}

fn assert_python_property_receiver_priority(
    files: &[(&str, &str)],
    result: &PackagePublication,
    property_receiver: impl Fn(&str) -> bool,
) {
    use rift_protocol::configuration::PackageConfiguration;

    let prior_declarations = result
        .declarations
        .iter()
        .filter(|entry| !property_receiver(entry.symbol.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let prior_edges = result
        .edges
        .iter()
        .filter(|edge| !property_receiver(edge.to.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        files,
        None,
        &["demo"],
        PackageConfiguration::default(),
        Some(prior_edges.len()),
    );
    assert_eq!(limited.declarations, result.declarations);
    assert_eq!(limited.edges, prior_edges);
    let limited = publication_with_configuration(
        files,
        None,
        &["demo"],
        PackageConfiguration {
            symbols: u32::try_from(prior_declarations.len()).expect("fixture count"),
            ..PackageConfiguration::default()
        },
    );
    assert_eq!(limited.declarations, prior_declarations);
    assert_eq!(limited.edges, prior_edges);
}

#[test]
fn python_annotations_keep_their_checked_root_module_tokens() {
    use rift_protocol::read::EdgeKind;

    const SOURCE: &str = concat!(
        "import demo.members\n",
        "class Child(demo.members.Item):\n    pass\n",
        "def read(value: demo.members.Item) -> demo.members.Item:\n",
        "    return demo.members.Item()\n",
        "def shadowed(demo):\n    item: demo.members.Item\n    return item\n",
    );
    const ROOT: &str = "rift://symbol/pypi/pypi.org/click/python/demo";
    const TYPE: &str = "rift://symbol/pypi/pypi.org/click/python/demo/members/Item";
    let files = [
        ("demo/__init__.py", ""),
        ("demo/members.py", "class Item: pass\n"),
        ("demo/reader.py", SOURCE),
    ];
    let sites = SOURCE
        .match_indices("demo.members.Item")
        .map(|(start, _)| u64::try_from(start).expect("source offset"))
        .collect::<Vec<_>>();
    let complete = publication(&files, None, &["demo"]);
    let unit = &complete
        .units
        .iter()
        .find(|unit| unit.path.0 == "demo/reader.py")
        .expect("checked reader source")
        .unit;
    let mut roots = complete
        .edges
        .iter()
        .filter_map(|edge| {
            let occurrence = edge.occurrence.as_ref()?;
            (edge.to.as_str() == ROOT
                && edge.kinds == [EdgeKind::Reads]
                && &occurrence.unit == unit)
                .then_some(&occurrence.range)
        })
        .collect::<Vec<_>>();
    roots.sort_by_key(|range| range.start);
    assert_eq!(roots.len(), 4);
    for (range, start) in roots.into_iter().zip(&sites[..4]) {
        assert_eq!((range.start, range.end), (*start, start + 4));
    }
    for kind in [EdgeKind::Calls, EdgeKind::HasType, EdgeKind::Extends] {
        assert!(
            complete
                .edges
                .iter()
                .any(|edge| edge.to.as_str() == TYPE && edge.kinds == [kind])
        );
    }
    let prior = complete
        .edges
        .iter()
        .filter(|edge| {
            edge.to.as_str() != ROOT
                || edge.kinds != [EdgeKind::Reads]
                || edge.occurrence.as_ref().is_none_or(|site| {
                    &site.unit != unit || !sites[..3].contains(&site.range.start)
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        &files,
        None,
        &["demo"],
        rift_protocol::configuration::PackageConfiguration::default(),
        Some(prior.len() + 2),
    );
    assert!(prior.iter().all(|edge| limited.edges.contains(edge)));
    assert!(limited.edges.len() < complete.edges.len());
    assert_eq!(limited.declarations, complete.declarations);
}

#[test]
fn python_annotations_keep_checked_single_module_aliases() {
    use rift_protocol::configuration::PackageConfiguration;
    use rift_protocol::read::EdgeKind;

    const SOURCE: &str = concat!(
        "from . import helpers as members\n",
        "from . import helpers as ambiguous\n",
        "from . import other as ambiguous\n",
        "class Child(members.Item):\n    pass\n",
        "def read(value: members.Item) -> members.Item:\n",
        "    return members.Item()\n",
        "def shadowed(members):\n    item: members.Item\n    return item\n",
        "def unresolved(value: ambiguous.Item, callback: members.make):\n    pass\n",
    );
    let files = [
        ("demo/__init__.py", ""),
        (
            "demo/helpers.py",
            "class Item: pass\ndef make(): return Item()\n",
        ),
        ("demo/other.py", "class Item: pass\n"),
        ("demo/reader.py", SOURCE),
    ];
    let complete = publication(&files, None, &["demo"]);
    let unit = &complete
        .units
        .iter()
        .find(|unit| unit.path.0 == "demo/reader.py")
        .expect("reader source")
        .unit;
    let sites = SOURCE
        .match_indices("members.Item")
        .map(|(start, _)| u64::try_from(start).expect("source offset"))
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 5);
    let module_read = |edge: &rift_protocol::index::StoredEdge| {
        edge.kinds == [EdgeKind::Reads]
            && edge.to.as_str().ends_with("/demo/helpers")
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| &site.unit == unit)
    };
    let mut offsets = complete
        .edges
        .iter()
        .filter(|edge| module_read(edge))
        .map(|edge| {
            let range = &edge.occurrence.as_ref().expect("physical read").range;
            assert_eq!(range.end - range.start, 7);
            range.start
        })
        .collect::<Vec<_>>();
    offsets.sort_unstable();
    assert_eq!(offsets, sites[..4]);
    let ambiguous = u64::try_from(SOURCE.find("ambiguous.Item").expect("written alias"))
        .expect("source offset");
    assert!(complete.edges.iter().all(|edge| {
        edge.occurrence.as_ref().is_none_or(|site| {
            &site.unit != unit || site.range.start != ambiguous || edge.kinds != [EdgeKind::Reads]
        })
    }));
    for kind in [EdgeKind::Calls, EdgeKind::HasType, EdgeKind::Extends] {
        assert!(complete.edges.iter().any(|edge| {
            edge.to.as_str().ends_with("/demo/helpers/Item") && edge.kinds == [kind]
        }));
    }
    let prior = complete
        .edges
        .iter()
        .filter(|edge| {
            !module_read(edge)
                || edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| !sites[..3].contains(&site.range.start))
        })
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        &files,
        None,
        &["demo"],
        PackageConfiguration::default(),
        Some(prior.len()),
    );
    let incumbent = prior
        .into_iter()
        .filter(|edge| !module_read(edge))
        .collect::<Vec<_>>();
    assert_eq!(limited.declarations, complete.declarations);
    assert_eq!(limited.edges, incumbent);
}

#[test]
fn python_wildcard_imports_keep_the_written_module_and_star() {
    use rift_protocol::configuration::PackageConfiguration;
    use rift_protocol::read::EdgeKind;

    let files = [
        ("demo/__init__.py", ""),
        ("demo/leaf.py", "class Item: pass\n"),
        ("demo/wild.py", "from .leaf import *\n"),
        ("demo/named.py", "from .leaf import Item\n"),
        ("demo/absent.py", "from .missing import *\n"),
        ("demo/relative.py", "from . import *\n"),
    ];
    let complete = publication(&files, None, &["demo"]);
    let target = |edge: &rift_protocol::index::StoredEdge, module: &str, name: &str| {
        edge.kinds == [EdgeKind::Imports]
            && edge.to.as_str().ends_with(name)
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.unit.0.ends_with(module))
    };
    let module_ranges = |path| {
        let mut ranges = complete
            .edges
            .iter()
            .filter(|edge| target(edge, path, "/demo/leaf"))
            .map(|edge| {
                let range = &edge.occurrence.as_ref().expect("written import").range;
                (range.start, range.end)
            })
            .collect::<Vec<_>>();
        ranges.sort_unstable();
        ranges
    };
    assert_eq!(module_ranges("/demo/wild.py"), [(5, 10), (18, 19)]);
    assert_eq!(module_ranges("/demo/named.py"), [(5, 10)]);
    assert!(complete.edges.iter().any(|edge| {
        target(edge, "/demo/named.py", "/demo/leaf/Item")
            && edge
                .occurrence
                .as_ref()
                .is_some_and(|site| site.range.start == 18 && site.range.end == 22)
    }));
    assert!(complete.edges.iter().all(|edge| {
        edge.kinds != [EdgeKind::Imports]
            || edge.occurrence.as_ref().is_none_or(|site| {
                !site.unit.0.ends_with("/demo/absent.py")
                    && (!site.unit.0.ends_with("/demo/relative.py") || site.range.start != 5)
            })
    }));

    let prior = complete
        .edges
        .iter()
        .filter(|edge| {
            !target(edge, "/demo/wild.py", "/demo/leaf")
                || edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| site.range.start != 5)
        })
        .cloned()
        .collect::<Vec<_>>();
    let limited = python_publication_with_limits(
        &files,
        None,
        &["demo"],
        PackageConfiguration::default(),
        Some(prior.len()),
    );
    let tight_incumbents = prior
        .iter()
        .filter(|edge| {
            edge.kinds == [EdgeKind::Contains] || target(edge, "/demo/named.py", "/demo/leaf/Item")
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(limited.declarations, complete.declarations);
    assert_eq!(limited.edges, tight_incumbents);
    let limited = python_publication_with_limits(
        &files,
        None,
        &["demo"],
        PackageConfiguration::default(),
        Some(prior.len() + 1),
    );
    assert!(prior.iter().all(|edge| limited.edges.contains(edge)));
}

const LOOP_READ_SOURCE: &str = concat!(
    "def chosen(): return 0\n",
    "def direct(source):\n    local = source\n    return local\n",
    "def via_loop(source):\n    for chosen in source:\n        return chosen\n",
    "    else:\n        return chosen\n",
    "def tupled(source):\n    for left, right in source:\n        return left, right\n",
    "async def async_loop(source):\n    async for item in source:\n        return item\n",
    "def shadowed(source):\n    for chosen in source:\n",
    "        def nested(chosen):\n            return chosen\n",
    "        return nested(1), chosen\n",
    "def changed(source):\n    for chosen in source:\n",
    "        chosen = 0\n        return chosen\n",
    "def iterable():\n    for chosen in chosen:\n        return chosen\n",
);

#[test]
fn python_loop_bodies_read_their_written_targets() {
    use rift_protocol::read::EdgeKind;

    let source = LOOP_READ_SOURCE;
    let result = publication(&[("demo/__init__.py", source)], None, &["demo"]);
    let range = |context: &str, name: &str| {
        let offset = source.find(context).expect("written context")
            + context.rfind(name).expect("written name");
        (
            u64::try_from(offset).expect("source offset"),
            u64::try_from(offset + name.len()).expect("source offset"),
        )
    };
    let reads_at = |context, name| {
        let range = range(context, name);
        result.edges.iter().filter(move |edge| {
            edge.kinds == [EdgeKind::Reads]
                && edge
                    .occurrence
                    .as_ref()
                    .is_some_and(|site| (site.range.start, site.range.end) == range)
        })
    };
    for (suffix, declaration, use_context, name) in [
        (
            "/via_loop/chosen",
            "def via_loop(source):\n    for chosen",
            "in source:\n        return chosen\n    else:",
            "chosen",
        ),
        (
            "/tupled/left",
            "for left, right",
            "return left, right",
            "left",
        ),
        (
            "/tupled/right",
            "for left, right",
            "return left, right",
            "right",
        ),
        ("/async_loop/item", "async for item", "return item", "item"),
        (
            "/shadowed/chosen",
            "def shadowed(source):\n    for chosen",
            "return nested(1), chosen",
            "chosen",
        ),
    ] {
        let declaration = result
            .declarations
            .iter()
            .find(|entry| {
                entry.symbol.as_str().ends_with(suffix)
                    && (entry.range.start, entry.range.end) == range(declaration, name)
            })
            .expect("exact loop declaration");
        let [read] = reads_at(use_context, name).collect::<Vec<_>>()[..] else {
            panic!("one exact Read for {suffix}");
        };
        assert_eq!(read.to, declaration.symbol);
    }
    for (suffix, context, name) in [
        ("/direct/local", "return local", "local"),
        (
            "/via_loop/source",
            "def via_loop(source):\n    for chosen in source",
            "source",
        ),
        (
            "/shadowed/nested/chosen",
            "def nested(chosen):\n            return chosen",
            "chosen",
        ),
    ] {
        assert!(
            reads_at(context, name).any(|edge| edge.to.as_str().ends_with(suffix)),
            "{suffix}"
        );
    }
    for (suffix, context) in [
        ("/via_loop/chosen", "else:\n        return chosen"),
        ("/changed/chosen", "chosen = 0\n        return chosen"),
        ("/iterable/chosen", "for chosen in chosen"),
    ] {
        assert!(
            reads_at(context, "chosen").all(|edge| !edge.to.as_str().ends_with(suffix)),
            "{suffix}"
        );
    }
}
