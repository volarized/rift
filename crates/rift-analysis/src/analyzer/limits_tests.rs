use std::fmt::Write as _;

use rift_core::ProjectPath;
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_syntax::{ShippedLanguage, SyntaxLimits};

use super::fixture::{identity, language, origin};
use super::{PackageAnalysis, PackageAnalyzer};
use crate::{
    ExactPackageInput, ExactPackageLimits, PackageImportRoot, PackageImportRootOrigin,
    PackageSource,
};
use rift_error::RiftError;

#[test]
fn test_package_publication_configuration_reaches_record_bounds() {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some("[package]\nunits = 2\nsymbols = 1\ndocuments = 1\nwarnings = 1\nidentifier_terms = 1\nretained_source = \"8b\"\nretained_total = \"64b\"\n"),
        &ConfigurationEnvironment::default(),
    ).expect("package configuration");
    let limits =
        ExactPackageLimits::from_configuration(accepted.configuration()).expect("accepted limits");
    let result = analyze(
        limits,
        "def alpha_beta_gamma(): pass\n",
        "def delta_epsilon(): pass\n",
    )
    .expect("bounded publication");
    let publication = result.publication();
    assert_eq!(publication.units.len(), 2);
    assert_eq!(publication.declarations.len(), 1);
    assert_eq!(publication.documents.len(), 1);
    assert_eq!(publication.warnings.len(), 1);
    assert!(publication.units.iter().all(|unit| unit.source.len() <= 8));
    assert!(
        publication
            .documents
            .iter()
            .all(|document| document.identifier_terms.len() <= 1)
    );
    let value = serde_json::to_value(publication).expect("publication");
    let schema = serde_json::to_value(schemars::schema_for!(
        rift_protocol::index::PackagePublication
    ))
    .expect("schema");
    jsonschema::validator_for(&schema)
        .expect("validator")
        .validate(&value)
        .expect("supported publication");
}

#[test]
fn test_package_environment_overrides_reach_publication_and_retention() {
    let environment = ConfigurationEnvironment::from_variables([
        ("RIFT_PACKAGE_UNITS", "2"),
        ("RIFT_PACKAGE_SYMBOLS", "6"),
        ("RIFT_PACKAGE_DOCUMENTS", "6"),
        ("RIFT_PACKAGE_WARNINGS", "8"),
        ("RIFT_PACKAGE_IDENTIFIER_TERMS", "2"),
        ("RIFT_PACKAGE_RETAINED_SOURCE", "64b"),
        ("RIFT_PACKAGE_RETAINED_TOTAL", "512b"),
        ("RIFT_SOURCE_EDGES", "1"),
    ]);
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some("[package]\nsymbols = 1\nretained_source = \"1b\"\n"),
        &environment,
    )
    .expect("environment configuration");
    let limits =
        ExactPackageLimits::from_configuration(accepted.configuration()).expect("accepted limits");
    let result = analyze(limits, &source(2), &source(2)).expect("configured package");
    assert_eq!(result.publication().declarations.len(), 6);
    assert_eq!(result.publication().documents.len(), 6);
    assert!(
        result
            .publication()
            .units
            .iter()
            .all(|unit| unit.source_complete)
    );
    assert!(
        result
            .publication()
            .declarations
            .iter()
            .all(|symbol| symbol.source_complete)
    );
    assert!(result.publication().warnings.is_empty());
    assert_eq!(limits.edges_max(), 1);
}

#[test]
fn test_package_publication_limits_validate_supported_capacity_and_ordering() {
    use rift_protocol::configuration::{ByteSize, PackageConfiguration};
    use rift_protocol::index::{PACKAGE_SYMBOLS_CEILING, PACKAGE_UNITS_CEILING};
    let supported = PackageConfiguration {
        units: PACKAGE_UNITS_CEILING,
        symbols: PACKAGE_SYMBOLS_CEILING,
        ..PackageConfiguration::default()
    };
    ExactPackageLimits::new(PACKAGE_UNITS_CEILING, u64::MAX)
        .with_publication(supported)
        .expect("supported capacity");
    for invalid in [
        PackageConfiguration {
            units: 0,
            ..supported
        },
        PackageConfiguration {
            symbols: PACKAGE_SYMBOLS_CEILING + 1,
            ..supported
        },
        PackageConfiguration {
            retained_source: ByteSize::from_bytes(2),
            retained_total: Some(ByteSize::from_bytes(1)),
            ..supported
        },
    ] {
        assert!(
            ExactPackageLimits::new(2, 1024)
                .with_publication(invalid)
                .is_err()
        );
    }
    assert!(ExactPackageLimits::new(2, 1024).with_edges(0).is_err());
    assert!(
        ExactPackageLimits::new(2, 1024)
            .with_edges(
                usize::try_from(rift_protocol::source::SOURCE_EDGES_MAX).expect("bound fits usize")
                    + 1
            )
            .is_err()
    );
    let one = ExactPackageLimits::new(2, 1024)
        .with_publication(PackageConfiguration {
            units: 1,
            ..PackageConfiguration::default()
        })
        .expect("one unit");
    let error = analyze(one, &source(1), &source(1)).expect_err("one-over input units");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_input_too_many_files"
    );
}

#[test]
fn test_edge_configuration_preserves_declaration_only_package_analysis() {
    let package = identity();
    let owner = package.owner().expect("fixture owner");
    let language = language(ShippedLanguage::Rust);
    let origin = origin(&package);
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let files = [PackageSource::new(
        &path,
        "pub fn alpha() {}\npub fn beta() {}\npub fn callers() { alpha(); beta(); }\n",
    )];
    let analyze_bound = |edges| {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.edges = edges;
        let limits = ExactPackageLimits::from_configuration(&configuration).expect("edge bounds");
        assert_eq!(limits.edges_max() as u64, edges);
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
            .expect("package input");
        PackageAnalyzer::analyze(input, 1).expect("package analysis")
    };
    // Static resolution ends on the work budget these edge bounds leave it.
    let exact = analyze_bound(2);
    assert!(!exact.semantics.edges().is_complete());
    let one_over = analyze_bound(1);
    assert!(!one_over.semantics.edges().is_complete());
    assert_eq!(one_over.semantics.edges().dropped_edges(), 0);
    assert_eq!(
        exact.publication().declarations,
        one_over.publication().declarations
    );
}

#[test]
fn supplemental_python_read_keeps_primary_package_calls_at_the_edge_cap() {
    let variants = |returned| {
        let calls = "    callee()\n".repeat(128);
        format!(
            "def callee(): pass\n\ndef alpha():\n    callee()\n    return {returned}\n\ndef zeta():\n{calls}"
        )
    };
    let baseline = variants("None  ");
    let expanded = variants("callee");
    let limits = || ExactPackageLimits::new(2, 1 << 20);
    let full = analyze(limits(), &baseline, "").expect("baseline package");
    let count = full.semantics.edges().edges().count();
    assert!(
        full.semantics
            .edges()
            .edges()
            .filter(|edge| edge.kind() == rift_protocol::read::EdgeKind::Calls)
            .count()
            >= 129
    );
    let limited = |source, edges| {
        let limits = limits().with_edges(edges).expect("exact edge cap");
        analyze(limits, source, "").expect("bounded package")
    };
    let before = limited(&baseline, count);
    let after = limited(&expanded, count);
    assert!(before.semantics.edges().is_complete());
    assert_eq!(
        before.semantics.edges().edges().collect::<Vec<_>>(),
        after.semantics.edges().edges().collect::<Vec<_>>()
    );
    assert_eq!(after.semantics.edges().dropped_edges(), 1);
    assert!(after.publication().warnings.iter().any(|warning| matches!(
        warning,
        rift_protocol::index::PackageAnalysisWarning::PublicationTruncated { collection, .. }
            if collection == "edges"
    )));
    let room = limited(&expanded, count + 1);
    assert!(room.semantics.edges().is_complete());
    assert!(room.semantics.edges().edges().any(|edge| {
        edge.kind() == rift_protocol::read::EdgeKind::Reads
            && edge.from().as_str().ends_with("/alpha")
            && edge.to().as_str().ends_with("/callee")
    }));
}

#[test]
fn supplemental_python_from_module_keeps_primary_calls_at_the_edge_cap() {
    let calls = "    callee()\n".repeat(128);
    let importer = format!("from .b import callee\n\ndef zeta():\n{calls}");
    let provider = "def callee(): pass\n";
    let limits = || ExactPackageLimits::new(2, 1 << 20);
    let full = analyze(limits(), &importer, provider).expect("fully admitted imports and calls");
    let edges = full.semantics.edges();
    let qualifier = |edge: &crate::Edge| {
        edge.kind() == rift_protocol::read::EdgeKind::Imports
            && edge.to().as_str().ends_with("/pkg/b")
            && edge.occurrence().unit().key().as_str() == "pkg/a.py"
            && edge.occurrence().range().start() == 5
            && edge.occurrence().range().end() == 7
    };
    assert!(edges.is_complete());
    assert_eq!(edges.edges().filter(|edge| qualifier(edge)).count(), 1);
    let expected = edges
        .edges()
        .filter(|edge| !qualifier(edge))
        .collect::<Vec<_>>();
    assert_eq!(
        expected
            .iter()
            .filter(|edge| edge.kind() == rift_protocol::read::EdgeKind::Calls)
            .count(),
        128
    );
    let bounded = analyze(
        limits()
            .with_edges(expected.len())
            .expect("preserved primary edge capacity"),
        &importer,
        provider,
    )
    .expect("bounded qualifier package");
    assert_eq!(
        bounded.semantics.edges().edges().collect::<Vec<_>>(),
        expected
    );
    assert_eq!(bounded.semantics.edges().dropped_edges(), 1);
}

fn source(count: u32) -> String {
    (0..count).fold(String::new(), |mut text, index| {
        writeln!(text, "def f{index}(): pass").expect("string write");
        text
    })
}

fn analyze(
    limits: ExactPackageLimits,
    first: &str,
    second: &str,
) -> Result<PackageAnalysis, RiftError> {
    let package = identity();
    let owner = package.owner().expect("fixture owner");
    let language = language(ShippedLanguage::Python);
    let origin = origin(&package);
    let first_path = ProjectPath::new("pkg/a.py").expect("path");
    let second_path = ProjectPath::new("pkg/b.py").expect("path");
    let files = [
        PackageSource::new(&first_path, first),
        PackageSource::new(&second_path, second),
    ];
    let roots = [PackageImportRoot::new(
        None,
        vec!["pkg".to_owned()],
        PackageImportRootOrigin::Wheel,
    )?];
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)?
        .with_import_roots(&roots)?;
    PackageAnalyzer::analyze(input, 1)
}

#[test]
fn test_declaration_limit_accepts_exact_count_and_refuses_one_more() {
    let limits = ExactPackageLimits::new(2, 1 << 20)
        .with_declarations(6)
        .expect("limit");
    let first = source(2);
    let second = source(2);
    let analysis = analyze(limits, &first, &second).expect("exact declaration count");
    assert_eq!(analysis.publication().declarations.len(), 6);
    assert!(analysis.publication().warnings.is_empty());

    let error = analyze(limits, &first, &source(3)).expect_err("one more declaration");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_declarations_exceeded"
    );
    assert!(
        error
            .context()
            .any(|(key, value)| key == "path" && value == "pkg/b.py")
    );
}

#[test]
fn test_declaration_limit_preserves_default_and_uses_configured_bound() {
    // Issue #561: raising syntax bounds alone cannot raise the package declaration bound.
    let syntax = SyntaxLimits::new(8 << 20, 4_000_000, 512).expect("syntax bounds");
    let limits = ExactPackageLimits::new(2, 16 << 20).with_syntax(syntax);
    assert_eq!(limits.declarations_max(), 100_000);
    let first = source(1);
    let second = source(2);
    let source_max = rift_protocol::index::PACKAGE_SOURCE_BYTES_MAX as usize;
    assert!(first.len() <= source_max && second.len() <= source_max);
    let baseline = analyze(limits, &first, &second).expect("default declaration bound");
    assert_eq!(baseline.publication().declarations.len(), 5);
    let selected = limits
        .with_declarations(4)
        .expect("selected declaration bound");
    let error = analyze(selected, &first, &second).expect_err("selected declaration bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_declarations_exceeded"
    );

    let raised = selected
        .with_declarations(5)
        .expect("raised declaration bound");
    let analysis = analyze(raised, &first, &second).expect("larger package");
    assert_eq!(analysis.publication().declarations.len(), 5);
    assert!(analysis.publication().warnings.is_empty());
}

#[test]
fn test_configuration_and_environment_declaration_limits_reach_analysis() {
    let document = "[source]\nfiles = 2000\nworkspace_size = \"32mb\"\ndeclarations = 10000\n\
                    [providers.syntax]\nmax_file = \"8mb\"\nmax_syntax_elements = 4000000\nmax_depth = 512\n";
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some(document),
        &ConfigurationEnvironment::default(),
    )
    .expect("configuration");
    let limits = ExactPackageLimits::from_configuration(accepted.configuration()).expect("bounds");
    assert_eq!(limits.files_max(), 2000);
    assert_eq!(limits.bytes_max(), 32 << 20);
    assert_eq!(limits.declarations_max(), 10_000);
    let first = source(5_000);
    let second = source(5_001);
    let error = analyze(limits, &first, &second).expect_err("configured declaration bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_declarations_exceeded"
    );

    let environment = ConfigurationEnvironment::from_variables([
        ("RIFT_SOURCE_DECLARATIONS", "10003"),
        ("RIFT_SOURCE_FILES", "3000"),
        ("RIFT_SOURCE_WORKSPACE_SIZE", "64mb"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_FILE", "16mb"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_SYNTAX_ELEMENTS", "5000000"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_DEPTH", "1024"),
    ]);
    let accepted = accept_configuration::<WorkspaceConfiguration>(Some(document), &environment)
        .expect("environment overrides");
    let limits = ExactPackageLimits::from_configuration(accepted.configuration()).expect("bounds");
    assert_eq!(limits.files_max(), 3000);
    assert_eq!(limits.bytes_max(), 64 << 20);
    assert_eq!(limits.declarations_max(), 10_003);
    assert_eq!(
        limits.syntax(),
        SyntaxLimits::new(16 << 20, 5_000_000, 1024).expect("syntax")
    );
    let analysis = analyze(limits, &first, &second).expect("environment raises declaration bound");
    assert_eq!(analysis.publication().declarations.len(), 10_003);
    assert!(analysis.publication().warnings.is_empty());
}

#[test]
fn test_declaration_limit_rejects_zero_and_accepts_fixed_width_maximum() {
    let limits = ExactPackageLimits::new(2, 1 << 20);
    let error = limits
        .with_declarations(0)
        .expect_err("zero declaration bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.provider.publication_zero_limit"
    );
    assert_eq!(
        limits
            .with_declarations(u32::MAX)
            .expect("positive bound")
            .declarations_max(),
        u32::MAX as usize
    );
}

#[test]
fn test_configuration_limit_validation_precedes_analysis() {
    for field in ["files", "workspace_size", "declarations"] {
        let mut configuration = WorkspaceConfiguration::default();
        match field {
            "files" => configuration.source.files = u64::MAX,
            "workspace_size" => {
                configuration.source.workspace_size =
                    rift_protocol::configuration::ByteSize::from_bytes(0);
            }
            _ => configuration.source.declarations = 0,
        }
        let error =
            ExactPackageLimits::from_configuration(&configuration).expect_err("invalid bound");
        assert_eq!(
            error.slug().as_str(),
            "rift.core.configuration_limit_out_of_range"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "field" && value == format!("source.{field}"))
        );
    }
    let environment = ConfigurationEnvironment::from_variables([("RIFT_SOURCE_DECLARATIONS", "0")]);
    let accepted =
        accept_configuration::<WorkspaceConfiguration>(None, &environment).expect("integer shape");
    assert!(ExactPackageLimits::from_configuration(accepted.configuration()).is_err());
}

#[test]
fn test_retained_total_counts_only_published_document_copies() {
    let mut configuration = rift_protocol::configuration::WorkspaceConfiguration::default();
    configuration.package.documents = 1;
    configuration.package.retained_source = rift_protocol::configuration::ByteSize::from_bytes(1);
    configuration.package.retained_total =
        Some(rift_protocol::configuration::ByteSize::from_bytes(5));
    let limits = ExactPackageLimits::from_configuration(&configuration).expect("limits");
    let analysis = analyze(limits, "# first", "# second").expect("five retained copies");
    assert_eq!(analysis.publication().units.len(), 2);
    assert_eq!(analysis.publication().documents.len(), 1);
    configuration.package.retained_total =
        Some(rift_protocol::configuration::ByteSize::from_bytes(4));
    let limits = ExactPackageLimits::from_configuration(&configuration).expect("limits");
    assert_eq!(
        analyze(limits, "# first", "# second")
            .expect_err("aggregate exceeded")
            .slug()
            .as_str(),
        "rift.analysis.package_retained_source_bytes_exceeded"
    );
}

#[test]
fn test_notebook_document_bound_keeps_cells_and_counts_only_published_source() {
    let notebook = r#"{"cells":[{"cell_type":"markdown","id":"first","source":"First."},{"cell_type":"markdown","id":"second","source":"Second."}],"metadata":{}}"#;
    let package = identity();
    let owner = package.owner().expect("fixture owner");
    let language = language(ShippedLanguage::Python);
    let origin = origin(&package);
    let path = ProjectPath::new("notebooks/guide.ipynb").expect("notebook path");
    let files = [PackageSource::new(&path, notebook)];
    let analyze_notebook = |limits| {
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)?;
        PackageAnalyzer::analyze(input, 1)
    };
    let default = analyze_notebook(ExactPackageLimits::new(1, notebook.len() as u64))
        .expect("default notebook publication");
    assert_eq!(default.publication().documents.len(), 2);
    assert!(default.publication().warnings.is_empty());

    let retained_bytes = notebook.len() + "First.".len();
    let configuration = format!(
        "[package]\ndocuments = 2\nretained_source = \"{}b\"\nretained_total = \"{retained_bytes}b\"\n",
        notebook.len(),
    );
    let environment = ConfigurationEnvironment::from_variables([("RIFT_PACKAGE_DOCUMENTS", "1")]);
    let accepted =
        accept_configuration::<WorkspaceConfiguration>(Some(&configuration), &environment)
            .expect("notebook collection configuration");
    let limits = ExactPackageLimits::from_configuration(accepted.configuration()).expect("limits");
    let bounded = analyze_notebook(limits).expect("only published notebook source is retained");
    let publication = bounded.publication();
    assert_eq!(publication.units, default.publication().units);
    assert_eq!(publication.documents.len(), 1);
    let first_document = default
        .publication()
        .documents
        .iter()
        .find(|document| document.file_content.as_deref() == Some("First."))
        .expect("default first notebook document");
    assert_eq!(&publication.documents[0], first_document);
    assert_eq!(
        publication.documents[0].file_content.as_deref(),
        Some("First.")
    );
    assert_eq!(publication.documentation.sources.len(), 2);
    assert_eq!(publication.documentation.coverage.omitted, 0);
    assert!(matches!(
        publication.warnings.as_slice(),
        [rift_protocol::index::PackageAnalysisWarning::PublicationTruncated {
            collection,
            bound: 1,
        }] if collection == "documents"
    ));

    let mut configuration = accepted.configuration().clone();
    configuration.package.documents = 2;
    let limits = ExactPackageLimits::from_configuration(&configuration).expect("two documents");
    assert_eq!(
        analyze_notebook(limits)
            .expect_err("second document exceeds retained total")
            .slug()
            .as_str(),
        "rift.analysis.package_retained_source_bytes_exceeded"
    );
}

// Issue #1065: a merged order holds every ancestor, so the orders of a chain end on their share.
#[test]
fn class_orders_past_their_share_keep_the_other_edges_and_report_it() {
    let classes = (1..2_000).fold(String::new(), |mut text, link| {
        use std::fmt::Write as _;
        writeln!(text, "class C{link}(C{}, M): pass", link - 1).expect("text line");
        text
    });
    let source = format!(
        "class Zed:\n    def m(self): pass\ndef sentinel(z: Zed):\n    z.m()\nclass M: pass\n\
         class C0: pass\n{classes}"
    );
    let sentinel = |source: &str| {
        let analysis =
            super::fixture::package_analysis(ShippedLanguage::Python, vec![("mod.py", source)]);
        let edges = analysis.semantics.edges();
        let called = edges.edges().any(|edge| {
            edge.from().as_str().ends_with("/sentinel") && edge.to().as_str().ends_with("/Zed/m")
        });
        (called, edges.is_complete(), edges.dropped_edges())
    };
    assert_eq!(sentinel(&source), (true, false, 0));
    let short = source
        .lines()
        .take(40)
        .fold(String::new(), |text, line| text + line + "\n");
    assert_eq!(sentinel(&short), (true, true, 0));
}

#[test]
fn supplemental_rust_macro_import_keeps_prior_edges_at_the_cap() {
    use rift_protocol::read::EdgeKind;

    let source = format!(
        "macro_rules! chosen {{ () => {{ 0u8 }}; }}\npub(crate) use chosen;\n\
         pub fn callee() {{}}\npub fn many() {{ {} }}",
        "callee(); ".repeat(128),
    );
    let package = identity();
    let owner = package.owner().expect("package owner");
    let language = language(ShippedLanguage::Rust);
    let origin = origin(&package);
    let root = ProjectPath::new("src/lib.rs").expect("library path");
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let files = [PackageSource::new(&root, &source)];
    let metadata = [PackageSource::new(
        &manifest,
        "[package]\nname='beacon'\nversion='1.0.0'\nedition='2024'\n",
    )];
    let limits = || ExactPackageLimits::new(2, 1 << 20);
    let collect = |limits| {
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
            .expect("bounded package input")
            .with_framework_context(&metadata, &[])
            .expect("captured Cargo manifest");
        PackageAnalyzer::analyze(input, 1).expect("bounded package")
    };
    let full = collect(limits());
    let edges = full.semantics.edges();
    let mut primary = Vec::new();
    let mut supplemental = 0;
    let mut root_additions = 0;
    for edge in edges.edges() {
        if edge.kind() == EdgeKind::Imports && edge.to().as_str().ends_with("/chosen") {
            supplemental += 1;
        } else if (edge.kind() == EdgeKind::Contains
            && edge.from().as_str() == "rift://symbol/cargo/crates.io/beacon/rust/beacon")
            || (edge.kind() == EdgeKind::Reads
                && edge.to().as_str() == "rift://symbol/cargo/crates.io/beacon/rust/beacon")
        {
            root_additions += 1;
        } else {
            primary.push(edge);
        }
    }
    assert_eq!(supplemental, 1);
    assert_eq!(
        primary
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .count(),
        128
    );
    let limited = collect(
        limits()
            .with_edges(primary.len())
            .expect("exact primary cap"),
    );
    assert_eq!(
        limited.semantics.edges().edges().collect::<Vec<_>>(),
        primary
    );
    assert_eq!(
        limited.semantics.edges().dropped_edges(),
        1 + root_additions
    );
}

#[test]
fn supplemental_rust_extern_crate_aliases_keep_prior_edges_at_the_cap() {
    use rift_protocol::read::EdgeKind;

    const ROOT: &str = "rift://symbol/cargo/crates.io/beacon/rust/beacon";
    let body = format!(
        "use std::fmt::Debug;\npub fn leaf() {{}}\n\
         pub fn alpha() {{ facade::leaf(); platform::thread::yield_now(); }}\n\
         pub fn zeta(later: fn()) {{ {}{} }}",
        "leaf(); ".repeat(128),
        "later(); ".repeat(8),
    );
    let (baseline, candidate) = rust_extern_capacity_sources(&body);
    let before = collect_rust_extern_capacity(&baseline, None);
    let incumbents = before
        .semantics
        .edges()
        .edges()
        .filter(|edge| edge.from().as_str() != ROOT || edge.kind() != EdgeKind::Contains)
        .collect::<Vec<_>>();
    let root_contains = before.semantics.edges().edges().count() - incumbents.len();
    assert_eq!(
        incumbents
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .count(),
        128
    );
    let capacity = incumbents.len();
    let bounded_before = collect_rust_extern_capacity(&baseline, Some(capacity));
    assert_eq!(
        bounded_before.semantics.edges().edges().collect::<Vec<_>>(),
        incumbents
    );

    let bounded_after = collect_rust_extern_capacity(&candidate, Some(capacity));
    assert_eq!(
        bounded_after.semantics.edges().edges().collect::<Vec<_>>(),
        incumbents
    );
    assert_eq!(
        bounded_after.semantics.edges().dropped_edges(),
        3 + root_contains as u64
    );

    let surplus = collect_rust_extern_capacity(&candidate, Some(capacity + 3 + root_contains));
    let edges = surplus.semantics.edges();
    assert_eq!(edges.dropped_edges(), 0);
    assert_eq!(edges.edges().count(), capacity + 3 + root_contains);
    assert!(
        incumbents
            .iter()
            .all(|edge| edges.edges().any(|row| row == *edge))
    );
    let alpha = format!("{ROOT}/alpha");
    let leaf = format!("{ROOT}/leaf");
    for (kind, from, to, authored) in [
        (
            EdgeKind::Imports,
            ROOT,
            "rift://symbol/stdlib/rust/rust/std",
            "std",
        ),
        (EdgeKind::Calls, &alpha, &leaf, "facade::leaf()"),
        (
            EdgeKind::Calls,
            &alpha,
            "rift://symbol/stdlib/rust/rust/std/thread/yield_now",
            "platform::thread::yield_now()",
        ),
    ] {
        let start = u64::try_from(candidate.find(authored).expect("written token"))
            .expect("written token offset");
        let end = start + u64::try_from(authored.len()).expect("written token length");
        assert!(edges.edges().any(|edge| {
            edge.kind() == kind
                && edge.from().as_str() == from
                && edge.to().as_str() == to
                && edge.occurrence().unit().key().as_str() == "src/lib.rs"
                && (
                    edge.occurrence().range().start(),
                    edge.occurrence().range().end(),
                ) == (start, end)
        }));
    }
}

#[test]
fn supplemental_rust_extern_crate_aliases_keep_prior_rows_at_the_work_cut() {
    use rift_protocol::read::EdgeKind;

    let body = format!(
        "use std::fmt::Debug;\npub mod nested {{ pub fn leaf() {{}} }}\n\
         pub fn alpha() {{ facade::nested::leaf(); platform::thread::yield_now(); }}\n\
         pub fn zeta(later: fn()) {{ {}{} }}",
        "nested::leaf(); ".repeat(128),
        "later(); ".repeat(8),
    );
    let (ordinary, authored) = rust_extern_capacity_sources(&body);
    let full = collect_rust_extern_capacity(&ordinary, None);
    let capacity = full.semantics.edges().edges().count();
    let before = collect_rust_extern_capacity(&ordinary, Some(capacity));
    let incumbent = before.semantics.edges().edges().collect::<Vec<_>>();
    assert!(incumbent.len() < capacity);
    assert_eq!(
        incumbent
            .iter()
            .filter(|edge| edge.kind() == EdgeKind::Calls)
            .count(),
        128
    );

    let after = collect_rust_extern_capacity(&authored, Some(capacity));
    assert_eq!(
        after.semantics.edges().edges().collect::<Vec<_>>(),
        incumbent
    );
}

fn rust_extern_capacity_sources(body: &str) -> (String, String) {
    const ALIASES: &str = "pub extern crate self as facade;\npub extern crate std as platform;\n";
    (
        format!("{}{body}", " ".repeat(ALIASES.len())),
        format!("{ALIASES}{body}"),
    )
}

fn collect_rust_extern_capacity(source: &str, capacity: Option<usize>) -> PackageAnalysis {
    let package = identity();
    let owner = package.owner().expect("fixture owner");
    let language = language(ShippedLanguage::Rust);
    let origin = origin(&package);
    let root = ProjectPath::new("src/lib.rs").expect("library path");
    let manifest = ProjectPath::new("Cargo.toml").expect("manifest path");
    let files = [PackageSource::new(&root, source)];
    let metadata = [PackageSource::new(
        &manifest,
        "[package]\nname = 'beacon'\nversion = '1.0.0'\nedition = '2024'\n",
    )];
    let limits = ExactPackageLimits::new(2, 1 << 20);
    let limits = capacity.map_or(limits, |maximum| {
        limits.with_edges(maximum).expect("exact edge cap")
    });
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
        .expect("bounded package input")
        .with_framework_context(&metadata, &[])
        .expect("captured Cargo manifest");
    PackageAnalyzer::analyze(input, 1).expect("bounded package")
}
