//! Package analysis accepts validated syntax without retaining old placement.
#![cfg(feature = "collector")]

use std::sync::Arc;

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalysis, PackageAnalyzer, PackageSource,
    PackageSyntax, PackageSyntaxSource,
};
use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
use rift_error::RiftError;
use rift_protocol::canonical::canonical_json;
use rift_protocol::read::PackageIdentity;
use rift_syntax::{ShippedLanguage, SyntaxLimits};

fn analyze(
    version: &str,
    shipped: ShippedLanguage,
    files: &[(&str, &str)],
    limits: SyntaxLimits,
    supplied: impl FnMut(&PackageSyntaxSource<'_>) -> Option<PackageSyntax>,
) -> Result<PackageAnalysis, RiftError> {
    let package = PackageIdentity {
        manager: "cargo".to_owned(),
        name: "beacon".to_owned(),
        version: version.to_owned(),
    };
    let language = shipped.language();
    let origin = ContributionOrigin::new(
        Some(SourceLocation::Dependency {
            package: package.clone(),
        }),
        SourceKind::Authored,
    )?;
    let paths: Vec<_> = files
        .iter()
        .map(|(path, _)| ProjectPath::new(*path).expect("fixture path"))
        .collect();
    let sources: Vec<_> = paths
        .iter()
        .zip(files)
        .map(|(path, (_, text))| PackageSource::new(path, text))
        .collect();
    let bytes = files
        .iter()
        .map(|(_, text)| u64::try_from(text.len()).expect("fixture bytes"))
        .sum();
    let input = ExactPackageInput::new(
        &package,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(u32::try_from(files.len()).expect("fixture count"), bytes)
            .with_syntax(limits),
    )?;
    PackageAnalyzer::analyze_with_syntax(input, 1, supplied)
}

fn canonical(analysis: &PackageAnalysis) -> String {
    canonical_json(analysis.publication()).expect("publication canonical JSON")
}

fn capture(
    source: &PackageSyntaxSource<'_>,
    retained: &mut Vec<PackageSyntax>,
) -> Option<PackageSyntax> {
    if !source.has_provider() {
        return None;
    }
    let parsed = source.parse().expect("fixture parses");
    retained.push(parsed.clone());
    Some(parsed)
}

fn lookup(source: &PackageSyntaxSource<'_>, retained: &[PackageSyntax]) -> Option<PackageSyntax> {
    retained
        .iter()
        .find(|syntax| syntax.identity() == source.identity())
        .cloned()
}

#[test]
fn every_package_provider_keeps_canonical_facts_and_reports_actual_calls() {
    let cases = [
        (
            ShippedLanguage::Rust,
            "src/lib.rs",
            "/// Opens a file.\npub fn open() {}\n",
        ),
        (
            ShippedLanguage::Python,
            "module.py",
            "def open():\n    return 1\n",
        ),
        (
            ShippedLanguage::JavaScript,
            "index.js",
            "export function open() {}\n",
        ),
        (
            ShippedLanguage::TypeScript,
            "index.ts",
            "export function open(): void {}\n",
        ),
        (
            ShippedLanguage::TypeScriptTsx,
            "index.tsx",
            "export function open(): null { return null; }\n",
        ),
    ];
    for (shipped, path, text) in cases {
        let files = [(path, text), ("README.md", "# Beacon\n\nOpen a file.\n")];
        let mut retained = Vec::new();
        let first = analyze(
            "1.0.0",
            shipped,
            &files,
            SyntaxLimits::default(),
            |source| {
                if source.path().as_str() == path {
                    assert_eq!(source.identity().language, shipped.language());
                }
                capture(source, &mut retained)
            },
        )
        .expect("first package");
        assert_eq!(first.syntax_work().provider_calls, 2);
        assert_eq!(first.syntax_work().reused_files, 0);
        let reused = analyze(
            "1.0.0",
            shipped,
            &files,
            SyntaxLimits::default(),
            |source| lookup(source, &retained),
        )
        .expect("reused package");
        let fresh = analyze("1.0.0", shipped, &files, SyntaxLimits::default(), |_| None)
            .expect("fresh package");
        assert_eq!(canonical(&first), canonical(&fresh), "{path}");
        assert_eq!(canonical(&reused), canonical(&fresh), "{path}");
        assert_eq!(reused.syntax_work().provider_calls, 0, "{path}");
        assert_eq!(reused.syntax_work().reused_files, 2, "{path}");
        assert_eq!(fresh.syntax_work().provider_calls, 2, "{path}");
        assert!(Arc::ptr_eq(
            reused.files()[0].file().syntax_facts(),
            retained[1].facts()
        ));
    }
}

#[test]
fn changed_version_moved_file_and_changed_source_rebuild_current_identities() {
    let first_files = [
        ("src/first.rs", "pub fn open() {}\n"),
        ("src/changed.rs", "pub fn close() {}\n"),
    ];
    let mut retained = Vec::new();
    analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &first_files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect("first package");
    let second_files = [
        ("src/moved.rs", first_files[0].1),
        ("src/changed.rs", "pub fn close(value: u32) {}\n"),
    ];
    let reused = analyze(
        "2.0.0",
        ShippedLanguage::Rust,
        &second_files,
        SyntaxLimits::default(),
        |source| lookup(source, &retained),
    )
    .expect("second package");
    let fresh = analyze(
        "2.0.0",
        ShippedLanguage::Rust,
        &second_files,
        SyntaxLimits::default(),
        |_| None,
    )
    .expect("fresh second package");
    assert_eq!(canonical(&reused), canonical(&fresh));
    assert_eq!(reused.syntax_work().provider_calls, 1);
    assert_eq!(reused.syntax_work().reused_files, 1);
    for symbol in &reused.publication().symbols {
        assert!(
            symbol.symbol.0.contains("beacon@2.0.0/"),
            "{}",
            symbol.symbol.0
        );
        assert!(!symbol.symbol.0.contains("first.rs"), "{}", symbol.symbol.0);
    }
    let moved = reused
        .files()
        .iter()
        .find(|file| file.file().path().as_str() == "src/moved.rs")
        .expect("moved file");
    assert!(Arc::ptr_eq(
        moved.file().syntax_facts(),
        retained[0].facts()
    ));
}

#[test]
fn changed_stub_rebuilds_joins_over_reused_implementation() {
    let first_files = [
        (
            "module.py",
            "class Client:\n    def open(self):\n        return 1\n",
        ),
        (
            "module.pyi",
            "class Client:\n    def open(self) -> int: ...\n",
        ),
    ];
    let mut retained = Vec::new();
    let first = analyze(
        "1.0.0",
        ShippedLanguage::Python,
        &first_files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect("first package");
    let second_files = [
        ("module.py", first_files[0].1),
        (
            "module.pyi",
            "class Client:\n    def open(self) -> str: ...\n    def close(self) -> None: ...\n",
        ),
    ];
    let reused = analyze(
        "2.0.0",
        ShippedLanguage::Python,
        &second_files,
        SyntaxLimits::default(),
        |source| lookup(source, &retained),
    )
    .expect("second package");
    let fresh = analyze(
        "2.0.0",
        ShippedLanguage::Python,
        &second_files,
        SyntaxLimits::default(),
        |_| None,
    )
    .expect("fresh package");
    assert_eq!(canonical(&reused), canonical(&fresh));
    assert_eq!(reused.syntax_work().provider_calls, 1);
    assert_eq!(reused.syntax_work().reused_files, 1);
    assert_ne!(
        first.publication().symbols.len(),
        reused.publication().symbols.len()
    );
    let open = reused
        .publication()
        .symbols
        .iter()
        .find(|symbol| {
            symbol.qualified_name == "Client.open" && symbol.unit.0.ends_with("/module.py")
        })
        .expect("joined method");
    assert!(
        open.signature
            .as_ref()
            .expect("joined signature")
            .display
            .contains("-> str")
    );
}

#[test]
fn incompatible_recorded_inputs_fall_back_to_current_provider() {
    let files = [("src/lib.rs", "pub fn open() {}\n")];
    let mut retained = Vec::new();
    let fresh = analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect("first package");
    let valid = &retained[0];
    let mut identities = Vec::new();
    let mut identity = valid.identity().clone();
    identity.source_digest = rift_core::FileDigest::of(b"different bytes");
    identities.push(identity);
    let mut identity = valid.identity().clone();
    identity.language = ShippedLanguage::Python.language();
    identities.push(identity);
    let mut identity = valid.identity().clone();
    identity.language.dialect = Some("tsx".to_owned());
    identities.push(identity);
    let mut identity = valid.identity().clone();
    identity.analyzer_digest = "0".repeat(64);
    identities.push(identity);
    let mut identity = valid.identity().clone();
    identity.analyzer_digest.truncate(8);
    identities.push(identity);
    let limits = valid.identity().limits;
    for limits in [
        SyntaxLimits::new(
            limits.source_bytes_max() + 1,
            limits.syntax_nodes_max(),
            limits.syntax_depth_max(),
        )
        .expect("bounds"),
        SyntaxLimits::new(
            limits.source_bytes_max(),
            limits.syntax_nodes_max() + 1,
            limits.syntax_depth_max(),
        )
        .expect("bounds"),
        SyntaxLimits::new(
            limits.source_bytes_max(),
            limits.syntax_nodes_max(),
            limits.syntax_depth_max() + 1,
        )
        .expect("bounds"),
    ] {
        let mut identity = valid.identity().clone();
        identity.limits = limits;
        identities.push(identity);
    }
    for identity in identities {
        let bad = PackageSyntax::new(identity, Arc::clone(valid.facts()));
        let result = analyze(
            "1.0.0",
            ShippedLanguage::Rust,
            &files,
            SyntaxLimits::default(),
            |_| Some(bad.clone()),
        )
        .expect("fallback package");
        assert_eq!(canonical(&result), canonical(&fresh));
        assert_eq!(result.syntax_work().provider_calls, 1);
        assert_eq!(result.syntax_work().reused_files, 0);
    }
}

#[test]
fn supplied_identity_cannot_replace_facts_source_or_language_witness() {
    let files = [("src/lib.rs", "pub fn open() {}\n")];
    let mut unrelated = Vec::new();
    analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &[("src/other.rs", "pub fn close() {}\n")],
        SyntaxLimits::default(),
        |source| capture(source, &mut unrelated),
    )
    .expect("other bytes");
    analyze(
        "1.0.0",
        ShippedLanguage::Python,
        &[("module.py", files[0].1)],
        SyntaxLimits::default(),
        |source| capture(source, &mut unrelated),
    )
    .expect("other language");
    for facts in unrelated {
        let reused = analyze(
            "1.0.0",
            ShippedLanguage::Rust,
            &files,
            SyntaxLimits::default(),
            |source| {
                Some(PackageSyntax::new(
                    source.identity().clone(),
                    Arc::clone(facts.facts()),
                ))
            },
        )
        .expect("fallback package");
        let fresh = analyze(
            "1.0.0",
            ShippedLanguage::Rust,
            &files,
            SyntaxLimits::default(),
            |_| None,
        )
        .expect("fresh package");
        assert_eq!(canonical(&reused), canonical(&fresh));
        assert_eq!(reused.syntax_work().provider_calls, 1);
        assert_eq!(reused.syntax_work().reused_files, 0);
    }
}

#[test]
fn tighter_syntax_bounds_preserve_current_provider_refusal() {
    let files = [("src/lib.rs", "pub fn open() {}\n")];
    let mut retained = Vec::new();
    analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect("first package");
    for limits in [
        SyntaxLimits::new(1, 250_000, 512).expect("bounds"),
        SyntaxLimits::new(4 << 20, 1, 512).expect("bounds"),
        SyntaxLimits::new(4 << 20, 250_000, 1).expect("bounds"),
    ] {
        let reused = analyze("1.0.0", ShippedLanguage::Rust, &files, limits, |_| {
            Some(retained[0].clone())
        })
        .expect_err("changed bounds refuse source");
        let fresh = analyze("1.0.0", ShippedLanguage::Rust, &files, limits, |_| None)
            .expect_err("fresh bounds refuse source");
        assert_eq!(reused.slug(), fresh.slug());
        assert_eq!(reused.to_string(), fresh.to_string());
    }
}

#[test]
fn non_syntax_documentation_is_excluded_from_provider_counts() {
    let files = [
        ("guide.txt", "Open a file.\n"),
        ("guide.rst", "Beacon\n======\n\nOpen a file.\n"),
        (
            "guide.ipynb",
            "{\"cells\":[],\"metadata\":{},\"nbformat\":4,\"nbformat_minor\":5}",
        ),
    ];
    let result = analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |source| {
            assert!(!source.has_provider());
            None
        },
    )
    .expect("documentation package");
    assert_eq!(result.syntax_work().provider_calls, 0);
    assert_eq!(result.syntax_work().reused_files, 0);
    assert_eq!(result.publication().units.len(), files.len());
}

#[test]
fn successful_file_work_survives_later_source_failure() {
    let mut retained = Vec::new();
    let files = [
        ("src/lib.rs", "pub fn open() {}\n"),
        ("guide.unknown", "source"),
    ];
    let refused = analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect_err("unsupported later file");
    assert_eq!(
        refused.slug().as_str(),
        "rift.analysis.package_syntax_unavailable"
    );
    assert_eq!(retained.len(), 1);
    let recovered = analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &files[..1],
        SyntaxLimits::default(),
        |source| lookup(source, &retained),
    )
    .expect("retained work");
    assert_eq!(recovered.syntax_work().provider_calls, 0);
    assert_eq!(recovered.syntax_work().reused_files, 1);
}

#[test]
fn hook_parser_calls_are_counted_even_when_facts_are_supplied() {
    let result = analyze(
        "1.0.0",
        ShippedLanguage::Rust,
        &[("src/lib.rs", "pub fn open() {}\n")],
        SyntaxLimits::default(),
        |source| {
            source.parse().expect("first parse");
            Some(source.parse().expect("second parse"))
        },
    )
    .expect("package");
    assert_eq!(result.syntax_work().provider_calls, 2);
    assert_eq!(result.syntax_work().reused_files, 0);
}

#[path = "package_syntax/process_restore.rs"]
mod process_restore;

#[test]
fn supplied_identity_cannot_replace_facts_syntax_bounds() {
    let files = [("src/lib.rs", "pub fn open() {}\n")];
    let limits = SyntaxLimits::default();
    let raised = SyntaxLimits::new(
        limits.source_bytes_max(),
        limits.syntax_nodes_max() + 1,
        limits.syntax_depth_max(),
    )
    .expect("bounds");
    let mut retained = Vec::new();
    analyze("1.0.0", ShippedLanguage::Rust, &files, raised, |source| {
        capture(source, &mut retained)
    })
    .expect("raised-bound facts");
    let result = analyze("1.0.0", ShippedLanguage::Rust, &files, limits, |source| {
        Some(PackageSyntax::new(
            source.identity().clone(),
            Arc::clone(retained[0].facts()),
        ))
    })
    .expect("fallback package");
    let fresh =
        analyze("1.0.0", ShippedLanguage::Rust, &files, limits, |_| None).expect("fresh package");
    assert_eq!(canonical(&result), canonical(&fresh));
    assert_eq!(result.syntax_work().provider_calls, 1);
    assert_eq!(result.syntax_work().reused_files, 0);
}

#[test]
fn changed_typescript_companion_keeps_current_joined_signatures() {
    let first_files = [
        ("index.js", "export function open() { return 1; }\n"),
        ("index.d.ts", "export function open(): number;\n"),
    ];
    let mut retained = Vec::new();
    analyze(
        "1.0.0",
        ShippedLanguage::TypeScript,
        &first_files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
    )
    .expect("first package");
    let second_files = [
        ("index.js", first_files[0].1),
        ("index.d.ts", "export function open(): string;\n"),
    ];
    let reused = analyze(
        "2.0.0",
        ShippedLanguage::TypeScript,
        &second_files,
        SyntaxLimits::default(),
        |source| lookup(source, &retained),
    )
    .expect("second package");
    let fresh = analyze(
        "2.0.0",
        ShippedLanguage::TypeScript,
        &second_files,
        SyntaxLimits::default(),
        |_| None,
    )
    .expect("fresh package");
    assert_eq!(canonical(&reused), canonical(&fresh));
    assert_eq!(reused.syntax_work().provider_calls, 1);
    assert_eq!(reused.syntax_work().reused_files, 1);
    let open = reused
        .publication()
        .symbols
        .iter()
        .find(|symbol| symbol.name == "open" && symbol.unit.0.ends_with("/index.js"))
        .expect("joined implementation");
    assert!(
        open.signature
            .as_ref()
            .expect("joined signature")
            .display
            .contains("string")
    );
}
