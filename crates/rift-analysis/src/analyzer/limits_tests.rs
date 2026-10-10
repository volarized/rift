use std::fmt::Write as _;

use rift_core::ProjectPath;
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_syntax::{ShippedLanguage, SyntaxLimits};

use super::fixture::{identity, language, origin};
use super::{PackageAnalysis, PackageAnalyzer};
use crate::{ExactPackageInput, ExactPackageLimits, PackageSource};
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
    assert_eq!(publication.symbols.len(), 1);
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
        ("RIFT_PACKAGE_SYMBOLS", "4"),
        ("RIFT_PACKAGE_DOCUMENTS", "6"),
        ("RIFT_PACKAGE_WARNINGS", "8"),
        ("RIFT_PACKAGE_IDENTIFIER_TERMS", "2"),
        ("RIFT_PACKAGE_RETAINED_SOURCE", "64b"),
        ("RIFT_PACKAGE_RETAINED_TOTAL", "256b"),
        ("RIFT_SOURCE_RELATIONSHIPS", "1"),
    ]);
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some("[package]\nsymbols = 1\nretained_source = \"1b\"\n"),
        &environment,
    )
    .expect("environment configuration");
    let limits =
        ExactPackageLimits::from_configuration(accepted.configuration()).expect("accepted limits");
    let result = analyze(limits, &source(2), &source(2)).expect("configured package");
    assert_eq!(result.publication().symbols.len(), 4);
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
            .symbols
            .iter()
            .all(|symbol| symbol.source_complete)
    );
    assert!(result.publication().warnings.is_empty());
    assert_eq!(limits.relationships_max(), 1);
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
    assert!(
        ExactPackageLimits::new(2, 1024)
            .with_relationships(0)
            .is_err()
    );
    assert!(
        ExactPackageLimits::new(2, 1024)
            .with_relationships(
                usize::try_from(rift_protocol::source::SOURCE_RELATIONSHIPS_MAX)
                    .expect("bound fits usize")
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
fn test_relationship_configuration_preserves_declaration_only_package_analysis() {
    let package = identity();
    let owner = package.owner().expect("fixture owner");
    let language = language(ShippedLanguage::Rust);
    let origin = origin(&package);
    let path = ProjectPath::new("src/lib.rs").expect("source path");
    let files = [PackageSource::new(
        &path,
        "pub fn alpha() {}\npub fn beta() {}\npub fn callers() { alpha(); beta(); }\n",
    )];
    let analyze_bound = |relationships| {
        let mut configuration = WorkspaceConfiguration::default();
        configuration.source.relationships = relationships;
        let limits =
            ExactPackageLimits::from_configuration(&configuration).expect("relationship bounds");
        assert_eq!(limits.relationships_max() as u64, relationships);
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
            .expect("package input");
        PackageAnalyzer::analyze(input, 1).expect("package analysis")
    };
    let exact = analyze_bound(2);
    assert!(exact.semantics.relationships().is_complete());
    let one_over = analyze_bound(1);
    assert!(one_over.semantics.relationships().is_complete());
    assert_eq!(one_over.semantics.relationships().dropped_edges(), 0);
    assert_eq!(exact.publication().symbols, one_over.publication().symbols);
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
    let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)?;
    PackageAnalyzer::analyze(input, 1)
}

#[test]
fn test_declaration_limit_accepts_exact_count_and_refuses_one_more() {
    let limits = ExactPackageLimits::new(2, 1 << 20)
        .with_declarations(4)
        .expect("limit");
    let first = source(2);
    let second = source(2);
    let analysis = analyze(limits, &first, &second).expect("exact declaration count");
    assert_eq!(analysis.publication().symbols.len(), 4);
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
    assert_eq!(baseline.publication().symbols.len(), 3);
    let selected = limits
        .with_declarations(2)
        .expect("selected declaration bound");
    let error = analyze(selected, &first, &second).expect_err("selected declaration bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.package_declarations_exceeded"
    );

    let raised = selected
        .with_declarations(3)
        .expect("raised declaration bound");
    let analysis = analyze(raised, &first, &second).expect("larger package");
    assert_eq!(analysis.publication().symbols.len(), 3);
    assert!(analysis.publication().warnings.is_empty());
}

#[test]
fn test_configuration_and_environment_declaration_limits_reach_analysis() {
    let document = "[source]\nfiles = 2000\nworkspace_size = \"32mb\"\ndeclarations = 10000\n\
                    [providers.syntax]\nmax_file = \"8mb\"\nmax_nodes = 4000000\nmax_depth = 512\n";
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
        ("RIFT_SOURCE_DECLARATIONS", "10001"),
        ("RIFT_SOURCE_FILES", "3000"),
        ("RIFT_SOURCE_WORKSPACE_SIZE", "64mb"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_FILE", "16mb"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_NODES", "5000000"),
        ("RIFT_PROVIDERS_SYNTAX_MAX_DEPTH", "1024"),
    ]);
    let accepted = accept_configuration::<WorkspaceConfiguration>(Some(document), &environment)
        .expect("environment overrides");
    let limits = ExactPackageLimits::from_configuration(accepted.configuration()).expect("bounds");
    assert_eq!(limits.files_max(), 3000);
    assert_eq!(limits.bytes_max(), 64 << 20);
    assert_eq!(limits.declarations_max(), 10_001);
    assert_eq!(
        limits.syntax(),
        SyntaxLimits::new(16 << 20, 5_000_000, 1024).expect("syntax")
    );
    let analysis = analyze(limits, &first, &second).expect("environment raises declaration bound");
    assert_eq!(analysis.publication().symbols.len(), 10_001);
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
        Some(rift_protocol::configuration::ByteSize::from_bytes(3));
    let limits = ExactPackageLimits::from_configuration(&configuration).expect("limits");
    let analysis = analyze(limits, "# first", "# second").expect("three retained copies");
    assert_eq!(analysis.publication().units.len(), 2);
    assert_eq!(analysis.publication().documents.len(), 1);
    configuration.package.retained_total =
        Some(rift_protocol::configuration::ByteSize::from_bytes(2));
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
