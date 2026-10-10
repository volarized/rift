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
use rift_syntax::{ShippedLanguage, SyntaxFacts, SyntaxFactsParts, SyntaxLimits, SyntaxNames};

fn analyze(
    version: &str,
    shipped: ShippedLanguage,
    files: &[(&str, &str)],
    limits: SyntaxLimits,
    supplied: impl FnMut(&PackageSyntaxSource<'_>) -> Option<PackageSyntax>,
) -> Result<PackageAnalysis, RiftError> {
    analyze_with_library(version, shipped, files, limits, supplied, None)
}

fn analyze_with_library(
    version: &str,
    shipped: ShippedLanguage,
    files: &[(&str, &str)],
    limits: SyntaxLimits,
    supplied: impl FnMut(&PackageSyntaxSource<'_>) -> Option<PackageSyntax>,
    library: Option<&str>,
) -> Result<PackageAnalysis, RiftError> {
    let (manager, registry) = match shipped {
        ShippedLanguage::Python => ("pypi", "pypi.org"),
        ShippedLanguage::TypeScript => ("npm", "npmjs.org"),
        _ => ("cargo", "crates.io"),
    };
    let package = PackageIdentity {
        manager: manager.to_owned(),
        registry: registry.to_owned(),
        name: "beacon".to_owned(),
        version: version.to_owned(),
    };
    let owner = package.owner().expect("fixture owner");
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
    let (metadata_name, mut metadata_text) = match shipped {
        ShippedLanguage::Rust => (
            "Cargo.toml",
            format!("[package]\nname='beacon'\nversion='{version}'\n"),
        ),
        ShippedLanguage::Python => (
            "setup.py",
            "from setuptools import setup\nsetup(py_modules=['module'])\n".to_owned(),
        ),
        ShippedLanguage::TypeScript => (
            "package.json",
            format!(
                "{{\"name\":\"beacon\",\"version\":\"{version}\",\"main\":\"index.js\",\"types\":\"index.d.ts\"}}"
            ),
        ),
        _ => ("Cargo.toml", String::new()),
    };
    if let Some(library) = library {
        assert_eq!(shipped, ShippedLanguage::Rust);
        metadata_text.push_str("[lib]\npath='");
        metadata_text.push_str(library);
        metadata_text.push_str("'\n");
    }
    let metadata_path = ProjectPath::new(metadata_name).expect("metadata path");
    let metadata = [PackageSource::new(&metadata_path, &metadata_text)];
    let context = if metadata_text.is_empty() {
        &[][..]
    } else {
        &metadata[..]
    };
    let roots = if shipped == ShippedLanguage::Python {
        vec![rift_analysis::PackageImportRoot::new(
            None,
            vec!["module".to_owned()],
            rift_analysis::PackageImportRootOrigin::PyModules,
        )?]
    } else {
        Vec::new()
    };
    let bytes = files
        .iter()
        .map(|(_, text)| u64::try_from(text.len()).expect("fixture bytes"))
        .sum::<u64>()
        + u64::try_from(metadata_text.len()).expect("metadata bytes");
    let input = ExactPackageInput::new(
        &owner,
        &language,
        &origin,
        &sources,
        ExactPackageLimits::new(
            u32::try_from(files.len() + context.len()).expect("fixture count"),
            bytes,
        )
        .with_syntax(limits),
    )?
    .with_framework_context(context, &[])?
    .with_import_roots(&roots)?;
    PackageAnalyzer::analyze_with_syntax(input, 1, supplied)
}

fn declaration_at<'analysis>(
    analysis: &'analysis PackageAnalysis,
    path: &str,
    qualified_name: &str,
) -> &'analysis rift_protocol::index::PackageSymbol {
    let held = analysis
        .files()
        .iter()
        .find(|held| held.file().path().as_str() == path)
        .expect("captured file");
    let syntax = held
        .file()
        .syntax()
        .symbols()
        .iter()
        .find(|syntax| syntax.qualified_name == qualified_name)
        .expect("captured declaration");
    let unit = analysis
        .publication()
        .units
        .iter()
        .find(|unit| unit.path.0 == path)
        .expect("published physical unit");
    analysis
        .publication()
        .declarations
        .iter()
        .find(|binding| {
            binding.unit == unit.unit
                && binding.range.start == syntax.item_range.start
                && binding.range.end == syntax.item_range.end
        })
        .expect("published physical binding")
}

fn object_at<'analysis>(
    analysis: &'analysis PackageAnalysis,
    path: &str,
    qualified_name: &str,
) -> &'analysis rift_protocol::read::Symbol {
    let declaration = declaration_at(analysis, path, qualified_name);
    let mut objects = analysis
        .publication()
        .objects
        .iter()
        .filter(|object| object.id.as_ref() == Some(&declaration.symbol));
    let object = objects.next().expect("established logical object");
    assert!(objects.next().is_none(), "one logical object per ID");
    object
}

fn binding_signatures<'analysis>(
    analysis: &'analysis PackageAnalysis,
    path: &str,
    qualified_name: &str,
) -> Vec<&'analysis str> {
    let declaration = declaration_at(analysis, path, qualified_name);
    let object = object_at(analysis, path, qualified_name);
    declaration
        .signature_indices
        .iter()
        .map(|index| {
            object
                .signatures
                .get(usize::try_from(*index).expect("portable signature index"))
                .expect("binding signature belongs to its object")
                .display
                .as_str()
        })
        .collect()
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

fn checked_restored(syntax: &PackageSyntax, text: &str) -> PackageSyntax {
    let facts = syntax.facts();
    let names = SyntaxNames::new(facts.language()).expect("recorded provider");
    let mut symbols = facts.symbols().to_vec();
    for symbol in &mut symbols {
        let kind = symbol.kind.to_owned();
        symbol.kind = names.symbol_kind(&kind).expect("restored provider kind");
        symbol.node_kind = symbol.node_kind.map(|kind| {
            let captured = kind.to_owned();
            names.node_kind(&captured).expect("restored grammar kind")
        });
    }
    let digest = rift_core::FileDigest::of(text.as_bytes());
    assert_eq!(syntax.identity().source_digest, digest);
    let restored = SyntaxFacts::from_parts(
        text,
        syntax.identity().limits,
        SyntaxFactsParts {
            origin: facts.origin(),
            language: facts.language().clone(),
            symbols,
            has_errors: facts.has_errors(),
            left_out_declarations: facts.left_out_declaration_count(),
            markdown_facts: facts.markdown_facts().cloned(),
            export_bindings: facts.export_bindings().map(<[_]>::to_vec),
            source_digest: digest,
        },
    )
    .expect("checked restored facts");
    assert_eq!(&restored, facts.as_ref());
    PackageSyntax::new(syntax.identity().clone(), Arc::new(restored))
}

const PACKAGE_PROVIDER_CASES: [(ShippedLanguage, &str, &str); 16] = [
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
    (
        ShippedLanguage::Html,
        "client.html",
        "<main id=\"client\"><script>function open() { return 1; }</script><style>.client { color: blue; }</style></main>",
    ),
    (
        ShippedLanguage::HtmlAngular,
        "client.html",
        "@if (ready) { <main>{{ client }}</main> }",
    ),
    (
        ShippedLanguage::Css,
        "client.css",
        ".client { --client-color: blue; color: var(--client-color); }",
    ),
    (
        ShippedLanguage::Vue,
        "client.vue",
        "<script lang=\"ts\">export function open(): number { return 1; }</script><template><main>{{ open() }}</main></template><style>.client { color: blue; }</style>",
    ),
    (
        ShippedLanguage::Svelte,
        "client.svelte",
        "<script lang=\"ts\">export function open(): number { return 1; }</script><main>{open()}</main><style>.client { color: blue; }</style>",
    ),
    (
        ShippedLanguage::C,
        "client.h",
        "#include \"base.h\"\nstruct Client { int port; };\nint open_client(void) { return 1; }\n",
    ),
    (
        ShippedLanguage::Cpp,
        "client.H",
        "namespace client { class Client { public: int open() { return 1; } }; }",
    ),
    (
        ShippedLanguage::Cython,
        "client.pxd",
        "include \"base.pxi\"\ncdef int open_client(int port)\n",
    ),
    (
        ShippedLanguage::Cython,
        "client.pyx",
        "include \"base.pxi\"\ncdef class Client:\n    cpdef int open(self):\n        return 1\n",
    ),
    (
        ShippedLanguage::Cython,
        "client.pxi",
        "cdef int client_port = 8080\n",
    ),
    (
        ShippedLanguage::Jsonc,
        "client.jsonc",
        "// client\n{\"port\": 8080}",
    ),
];

#[test]
fn every_package_provider_keeps_canonical_facts_and_reports_actual_calls() {
    for (shipped, path, text) in PACKAGE_PROVIDER_CASES {
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
                    assert_eq!(source.text(), text);
                } else {
                    assert_eq!(source.text(), files[1].1);
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
        let restored: Vec<_> = retained
            .iter()
            .zip(files)
            .map(|(syntax, (_, source))| checked_restored(syntax, source))
            .collect();
        let restored_reused = analyze(
            "1.0.0",
            shipped,
            &files,
            SyntaxLimits::default(),
            |source| lookup(source, &restored),
        )
        .expect("restored supplied package");
        assert_eq!(canonical(&restored_reused), canonical(&fresh), "{path}");
        assert_eq!(restored_reused.syntax_work().provider_calls, 0, "{path}");
        assert_eq!(restored_reused.syntax_work().reused_files, 2, "{path}");
        assert!(Arc::ptr_eq(
            reused.files()[0].file().syntax_facts(),
            retained[1].facts()
        ));
    }
}

#[test]
fn changed_version_moved_file_and_changed_source_rebuild_current_identities() {
    let first_files = [
        ("src/first.rs", "mod changed;\npub fn open() {}\n"),
        ("src/changed.rs", "pub fn close() {}\n"),
    ];
    let mut retained = Vec::new();
    analyze_with_library(
        "1.0.0",
        ShippedLanguage::Rust,
        &first_files,
        SyntaxLimits::default(),
        |source| capture(source, &mut retained),
        Some("src/first.rs"),
    )
    .expect("first package");
    let second_files = [
        ("src/moved.rs", first_files[0].1),
        ("src/changed.rs", "pub fn close(value: u32) {}\n"),
    ];
    let reused = analyze_with_library(
        "2.0.0",
        ShippedLanguage::Rust,
        &second_files,
        SyntaxLimits::default(),
        |source| lookup(source, &retained),
        Some("src/moved.rs"),
    )
    .expect("second package");
    let fresh = analyze_with_library(
        "2.0.0",
        ShippedLanguage::Rust,
        &second_files,
        SyntaxLimits::default(),
        |_| None,
        Some("src/moved.rs"),
    )
    .expect("fresh second package");
    assert_eq!(canonical(&reused), canonical(&fresh));
    assert_eq!(reused.syntax_work().provider_calls, 1);
    assert_eq!(reused.syntax_work().reused_files, 1);
    assert!(!reused.publication().declarations.is_empty());
    assert_eq!(
        object_at(&reused, "src/moved.rs", "open")
            .id
            .as_ref()
            .expect("root object")
            .0,
        "rift://symbol/cargo/crates.io/beacon@2.0.0/rust/beacon/open"
    );
    assert_eq!(
        object_at(&reused, "src/changed.rs", "close")
            .id
            .as_ref()
            .expect("module object")
            .0,
        "rift://symbol/cargo/crates.io/beacon@2.0.0/rust/beacon/changed/close"
    );
    for symbol in &reused.publication().declarations {
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
        first.publication().objects.len(),
        reused.publication().objects.len()
    );
    assert_eq!(
        declaration_at(&reused, "module.py", "Client.open").symbol,
        declaration_at(&reused, "module.pyi", "Client.open").symbol
    );
    assert_eq!(
        binding_signatures(&reused, "module.pyi", "Client.open"),
        ["def open(self) -> str:"]
    );
    assert_eq!(
        binding_signatures(&reused, "module.py", "Client.open"),
        ["def open(self):"]
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
    assert_eq!(
        declaration_at(&reused, "index.js", "open").symbol,
        declaration_at(&reused, "index.d.ts", "open").symbol
    );
    assert_eq!(
        binding_signatures(&reused, "index.d.ts", "open"),
        ["function open(): string"]
    );
    assert_eq!(
        binding_signatures(&reused, "index.js", "open"),
        ["function open()"]
    );
}

#[test]
fn explicit_package_dialect_refuses_facts_from_default_extension_provider() {
    for (default, selected, path, text) in [
        (
            ShippedLanguage::C,
            ShippedLanguage::Cpp,
            "client.h",
            "struct Client { int port; };",
        ),
        (
            ShippedLanguage::Html,
            ShippedLanguage::HtmlAngular,
            "client.html",
            "<main>{{ client }}</main>",
        ),
    ] {
        let files = [(path, text)];
        let mut retained = Vec::new();
        analyze(
            "1.0.0",
            default,
            &files,
            SyntaxLimits::default(),
            |source| capture(source, &mut retained),
        )
        .expect("default provider");
        let changed = analyze(
            "1.0.0",
            selected,
            &files,
            SyntaxLimits::default(),
            |source| {
                assert_eq!(source.identity().language, selected.language());
                Some(PackageSyntax::new(
                    source.identity().clone(),
                    Arc::clone(retained[0].facts()),
                ))
            },
        )
        .expect("current explicit provider");
        let fresh = analyze("1.0.0", selected, &files, SyntaxLimits::default(), |_| None)
            .expect("fresh explicit provider");
        assert_eq!(canonical(&changed), canonical(&fresh));
        assert_eq!(changed.syntax_work().provider_calls, 1);
        assert_eq!(changed.syntax_work().reused_files, 0);
        assert_eq!(
            changed.files()[0].file().syntax().language(),
            &selected.language()
        );
    }
}
