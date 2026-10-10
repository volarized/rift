//! Package extension selection and analysis preserve source spelling and bytes.

use rift_analysis::{
    DocumentationSelection, ExactPackageInput, ExactPackageLimits, PackageAnalyzer,
    PackageFileSelection, PackageLanguage, PackageSource,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_protocol::canonical::canonical_json;
use rift_protocol::documentation::DocumentationConfiguration;
use rift_protocol::read::PackageIdentity;
use rift_syntax::ShippedLanguage;

#[test]
fn test_package_source_extensions_accept_ascii_case_without_changing_source() {
    // Issue #602: selection and ordinary or supplied analysis share extension matching.
    let cases = [
        (
            "js",
            ShippedLanguage::JavaScript,
            "export function open() {}\n",
        ),
        (
            "jsx",
            ShippedLanguage::JavaScript,
            "export function open() { return <div />; }\n",
        ),
        (
            "ts",
            ShippedLanguage::TypeScript,
            "export function open(): void {}\n",
        ),
        (
            "tsx",
            ShippedLanguage::TypeScriptTsx,
            "export function open() { return <div />; }\n",
        ),
        (
            "mjs",
            ShippedLanguage::JavaScript,
            "export function open() {}\n",
        ),
        ("py", ShippedLanguage::Python, "def open():\n    return 1\n"),
    ];
    for (extension, shipped, text) in cases {
        for extension in [extension.to_owned(), extension.to_ascii_uppercase()] {
            let path = ProjectPath::new(format!("src/EXAMPLE.{extension}")).expect("source path");
            let language = shipped.language();
            let selection_language = if shipped == ShippedLanguage::Python {
                PackageLanguage::Python
            } else {
                PackageLanguage::TypeScript
            };
            let documentation = DocumentationSelection::new(&DocumentationConfiguration::default())
                .expect("documentation selection");
            let selection = PackageFileSelection::new(selection_language, &[], documentation)
                .expect("source selection");
            assert_eq!(
                selection.select([&path]).source(),
                std::slice::from_ref(&path)
            );
            let package = PackageIdentity {
                manager: "npm".to_owned(),
                registry: "npmjs.org".to_owned(),
                name: "beacon".to_owned(),
                version: "1.0.0".to_owned(),
            };
            let origin = ContributionOrigin::new(
                Some(SourceLocation::Dependency {
                    package: package.clone(),
                }),
                SourceKind::Authored,
            )
            .expect("package origin");
            let files = [PackageSource::new(&path, text)];
            let input = ExactPackageInput::new(
                &package,
                &language,
                &origin,
                &files,
                ExactPackageLimits::new(1, 1024),
            )
            .expect("bounded package input");
            assert_package_syntax(&input, &path, text, &language);
        }
    }
}

#[test]
fn test_unknown_package_extension_is_unselected_and_refused() {
    let path = ProjectPath::new("src/EXAMPLE.UNKNOWN").expect("source path");
    let documentation = DocumentationSelection::new(&DocumentationConfiguration::default())
        .expect("documentation selection");
    let selection = PackageFileSelection::new(PackageLanguage::TypeScript, &[], documentation)
        .expect("source selection");
    assert!(selection.select([&path]).source().is_empty());
    let package = PackageIdentity {
        manager: "npm".to_owned(),
        registry: "npmjs.org".to_owned(),
        name: "beacon".to_owned(),
        version: "1.0.0".to_owned(),
    };
    let language = ShippedLanguage::TypeScript.language();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )
    .expect("package origin");
    let files = [PackageSource::new(&path, "export function open() {}\n")];
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &files,
        ExactPackageLimits::new(1, 1024),
    )
    .expect("bounded package input");
    let ordinary = PackageAnalyzer::analyze(input, 1).expect_err("unknown extension");
    let supplied = PackageAnalyzer::analyze_with_syntax(input, 1, |source| {
        assert!(!source.has_provider());
        None
    })
    .expect_err("unknown extension");
    assert_eq!(
        ordinary.slug().as_str(),
        "rift.analysis.package_syntax_unavailable"
    );
    assert_eq!(ordinary.to_string(), supplied.to_string());
}

fn assert_package_syntax(
    input: &ExactPackageInput<'_>,
    path: &ProjectPath,
    text: &str,
    language: &rift_protocol::read::Language,
) {
    let fresh = PackageAnalyzer::analyze(*input, 1).expect("ordinary analysis");
    let mut syntax = None;
    let captured = PackageAnalyzer::analyze_with_syntax(*input, 1, |source| {
        assert!(source.has_provider());
        assert_eq!(&source.identity().language, language);
        assert_eq!(source.path(), path);
        assert_eq!(source.text(), text);
        syntax = Some(source.parse().expect("selected provider"));
        syntax.clone()
    })
    .expect("supplied analysis");
    let reused =
        PackageAnalyzer::analyze_with_syntax(*input, 1, |_| syntax.clone()).expect("reused syntax");
    let expected = canonical_json(fresh.publication()).expect("canonical publication");
    assert_eq!(
        canonical_json(captured.publication()).expect("captured publication"),
        expected
    );
    assert_eq!(
        canonical_json(reused.publication()).expect("reused publication"),
        expected
    );
    assert_eq!(reused.syntax_work().provider_calls, 0);
    assert_eq!(reused.syntax_work().reused_files, 1);
    assert_eq!(fresh.files()[0].file().path(), path);
    assert_eq!(fresh.files()[0].file().source(), text);
    assert_eq!(fresh.files()[0].file().syntax().language(), language);
    assert!(
        fresh
            .publication()
            .symbols
            .iter()
            .any(|symbol| symbol.name == "open")
    );
}
