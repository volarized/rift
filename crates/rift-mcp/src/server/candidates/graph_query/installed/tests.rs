use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot, PackageSource,
};
use rift_core::{ContributionOrigin, PackageIdentity, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::read::RuntimeIdentity;

use super::declaration_at;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn installed_declarations_use_original_namespace_and_engine_position() -> TestResult {
    let cases = [
        (
            "cargo",
            "crates.io",
            "example-crate",
            "rust",
            "src/lib.rs",
            "pub struct Item;\nimpl Item { pub fn push(&self) {} }\n",
            "push",
            "example_crate/Item/push",
            "[package]\nname='example-crate'\nversion='1.0.0'\n",
        ),
        (
            "pypi",
            "pypi.org",
            "greeting",
            "python",
            "greeting/core.pyi",
            "class Base:\n    def get(self) -> str: ...\n",
            "get",
            "greeting/core/Base/get",
            "",
        ),
        (
            "npm",
            "npmjs.org",
            "greeting",
            "typescript",
            "index.d.ts",
            "export declare class Base { get(): string; }\n",
            "get",
            "greeting/Base/get",
            r#"{"name":"greeting","version":"1.0.0","types":"index.d.ts"}"#,
        ),
    ];
    for (manager, registry, name, language, path, text, target, suffix, manifest) in cases {
        let package = PackageIdentity {
            manager: manager.to_owned(),
            registry: registry.to_owned(),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
        };
        let owner = package.owner()?;
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency { package }),
            SourceKind::Authored,
        )?;
        let language = rift_protocol::read::Language {
            name: language.to_owned(),
            dialect: None,
        };
        let path = ProjectPath::new(path)?;
        let files = [PackageSource::new(&path, text)];
        let manifest_path = ProjectPath::new(if manager == "cargo" {
            "Cargo.toml"
        } else {
            "package.json"
        })?;
        let context = if manifest.is_empty() {
            Vec::new()
        } else {
            vec![PackageSource::new(&manifest_path, manifest)]
        };
        let import_roots = if manager == "pypi" {
            vec![super::python_import_root(&ProjectPath::new("greeting")?)?]
        } else {
            Vec::new()
        };
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(16, 65536),
        )?
        .with_framework_context(&context, &[])?
        .with_import_roots(&import_roots)?;
        let analysis = PackageAnalyzer::analyze(input, 1)?;
        let offset = u64::try_from(text.find(target).ok_or("target position")?)?;
        let selected = declaration_at(&analysis, &path, offset)?;
        assert_eq!(
            selected.symbol.as_str(),
            format!(
                "rift://symbol/{manager}/{registry}/{name}/{}/{suffix}",
                language.name
            )
        );
        assert!(selected.range.start <= offset && offset < selected.range.end);
    }
    Ok(())
}

#[test]
fn captured_runtime_stub_uses_exact_runtime_owner() -> TestResult {
    let runtime = RuntimeIdentity {
        runtime: "cpython".to_owned(),
        version: "3.14.7".to_owned(),
    };
    let owner = runtime.owner()?;
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Stdlib {
            runtime: Some(runtime),
        }),
        SourceKind::Authored,
    )?;
    let language = rift_protocol::read::Language {
        name: "python".to_owned(),
        dialect: None,
    };
    let path = ProjectPath::new("builtins.pyi")?;
    let text = "class list:\n    def append(self, value: object) -> None: ...\n";
    let files = [PackageSource::new(&path, text)];
    let roots = [PackageImportRoot::runtime(owner.clone(), None)?];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(4, 4096),
    )?
    .with_import_roots(&roots)?;
    let analysis = PackageAnalyzer::analyze(input, 1)?;
    let selected = declaration_at(
        &analysis,
        &path,
        u64::try_from(text.find("append").ok_or("append")?)?,
    )?;
    assert_eq!(
        selected.symbol.as_str(),
        "rift://symbol/stdlib/cpython/python/builtins/list/append"
    );
    assert_eq!(
        analysis.publication().owner,
        SymbolOwner::Runtime {
            runtime: "cpython".to_owned()
        }
    );
    Ok(())
}

#[test]
fn an_unnamed_declaration_never_selects_the_named_enclosing_module() -> TestResult {
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        registry: "npmjs.org".to_owned(),
        name: "greeting".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner()?;
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )?;
    let language = rift_protocol::read::Language {
        name: "typescript".to_owned(),
        dialect: None,
    };
    let path = ProjectPath::new("internal.d.ts")?;
    // A file no entry names is the module of its path. A name two members share and the
    // declarations of a script keep no identity.
    for (text, named) in [
        (
            "export declare class Hidden { get(): string; }\n",
            Some("greeting/internal.d.ts/Hidden/get"),
        ),
        (
            "export declare class Hidden { static get(): string; get(): string; }\n",
            None,
        ),
        ("declare class Hidden { get(): string; }\n", None),
    ] {
        let files = [PackageSource::new(&path, text)];
        let manifest = ProjectPath::new("package.json")?;
        let context = [PackageSource::new(
            &manifest,
            r#"{"name":"greeting","version":"1.0.0","types":"index.d.ts"}"#,
        )];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(4, 4096),
        )?
        .with_framework_context(&context, &[])?;
        let analysis = PackageAnalyzer::analyze(input, 1)?;
        let found = declaration_at(
            &analysis,
            &path,
            u64::try_from(text.find("get").ok_or("get")?)?,
        );
        let selected = found.ok().map(|found| {
            SymbolIdentity::parse(found.symbol.as_str())
                .map(|identity| identity.qualified_path().join("/"))
        });
        assert_eq!(selected.transpose()?.as_deref(), named, "{text}");
    }
    Ok(())
}

#[test]
fn an_unknown_position_inside_a_named_module_names_no_target() -> TestResult {
    let package = PackageIdentity {
        manager: "pypi".to_owned(),
        registry: "pypi.org".to_owned(),
        name: "greeting".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let owner = package.owner()?;
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency { package }),
        SourceKind::Authored,
    )?;
    let language = rift_protocol::read::Language {
        name: "python".to_owned(),
        dialect: None,
    };
    let path = ProjectPath::new("greeting/core.py")?;
    let text = "def valid(): pass\n\n# no declaration at this position\n";
    let files = [PackageSource::new(&path, text)];
    let roots = [super::python_import_root(&ProjectPath::new("greeting")?)?];
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(4, 4096),
    )?
    .with_import_roots(&roots)?;
    let analysis = PackageAnalyzer::analyze(input, 1)?;
    let offset = u64::try_from(text.find("no declaration").ok_or("comment")?)?;
    assert!(
        analysis
            .publication()
            .declarations
            .iter()
            .any(|declaration| declaration.range.start <= offset && offset < declaration.range.end)
    );
    assert!(declaration_at(&analysis, &path, offset).is_err());
    Ok(())
}
