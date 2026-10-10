use super::*;
use rift_protocol::documentation::{
    DocumentationContentIdentity, DocumentationSource, DocumentationSourceFormat,
    DocumentationSourceIdentity,
};
use rift_protocol::read::{ProjectPath, SourceKind, SourceLocationKind, SymbolOrigin};

fn source(path: &str, text: &str, format: DocumentationSourceFormat) -> DocumentationSource {
    DocumentationSource {
        identity: DocumentationContentIdentity {
            source: DocumentationSourceIdentity::Project {
                path: ProjectPath(path.to_owned()),
            },
            cell: None,
        },
        revision: content_digest(b"revision"),
        content_digest: content_digest(text.as_bytes()),
        origin: SymbolOrigin {
            location: Some(SourceLocationKind::Project),
            package: None,
            runtime: None,
            source_kind: SourceKind::Authored,
        },
        format,
        media_type: match format {
            DocumentationSourceFormat::RestructuredText => "text/x-rst",
            _ => "text/markdown",
        }
        .to_owned(),
        selection: rift_protocol::documentation::DocumentationSelectionReason::Workspace,
        byte_length: text.len() as u64,
        language: None,
        physical_ranges: Vec::new(),
        license: None,
    }
}

fn configured_collection(
    text: &str,
    configuration: &rift_protocol::documentation::DocumentationConfiguration,
    previous: Option<&DocumentationCollection>,
) -> DocumentationCollection {
    configured_collection_result(text, configuration, previous).expect("collection")
}

fn configured_collection_result(
    text: &str,
    configuration: &rift_protocol::documentation::DocumentationConfiguration,
    previous: Option<&DocumentationCollection>,
) -> Result<DocumentationCollection, rift_error::RiftError> {
    let limits = DocumentationLimits::from_configuration(configuration).expect("accepted bounds");
    let input = DocumentationInput::with_limits(
        source("guide.md", text, DocumentationSourceFormat::Markdown),
        text,
        &limits,
    )
    .expect("accepted source");
    let sources = DocumentationSourceSet::with_limits(vec![input], &limits).expect("source set");
    collect_documentation_incremental(previous, &sources, &[])
}

#[test]
fn configuration_changes_invalidate_documentation_extraction() {
    let text = "# Guide\n\nFirst paragraph.\n\nSecond paragraph.\n";
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let first = configured_collection(text, &configuration, None);
    assert!(first.index().blocks.len() > 1);
    let lowered = rift_protocol::documentation::DocumentationConfiguration {
        max_blocks: 1,
        max_warnings: 1,
        ..configuration
    };
    let next = configured_collection(text, &lowered, Some(&first));
    let cold = configured_collection(text, &lowered, None);
    assert_eq!(next.index(), cold.index());
    assert!(next.index().blocks.len() <= 1);
    assert!(next.index().warnings.len() <= 1);
    assert_eq!(next.index().coverage.omitted, 1);
}

#[test]
fn documentation_text_bounds_can_raise_above_default() {
    let name = "x".repeat(rift_protocol::documentation::DOCUMENTATION_TEXT_BYTES_MAX as usize + 1);
    let text = format!("# {name}\n");
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let error = configured_collection_result(&text, &configuration, None)
        .expect_err("default heading text bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.documentation_order_invalid"
    );
    let raised = rift_protocol::documentation::DocumentationConfiguration {
        max_text: rift_protocol::configuration::ByteSize::from_bytes(name.len() as u64),
        ..configuration
    };
    let next = configured_collection(&text, &raised, None);
    assert_eq!(next.index().coverage.omitted, 0);
    assert!(!next.index().blocks.is_empty());
    let cold = configured_collection(&text, &raised, None);
    assert_eq!(next.index(), cold.index());
}

#[test]
fn documentation_language_text_preserves_default_and_accepts_raising() {
    let language =
        "x".repeat(rift_protocol::documentation::DOCUMENTATION_TEXT_BYTES_MAX as usize + 1);
    let text = format!("```{language}\nbody\n```\n");
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let error = configured_collection_result(&text, &configuration, None)
        .expect_err("default language text bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.documentation_format_invalid"
    );
    let raised = rift_protocol::documentation::DocumentationConfiguration {
        max_text: rift_protocol::configuration::ByteSize::from_bytes(language.len() as u64),
        ..configuration
    };
    let next = configured_collection(&text, &raised, None);
    assert_eq!(next.index().coverage.omitted, 0);
    assert!(
        next.index()
            .blocks
            .iter()
            .any(|block| block.language.as_deref() == Some(language.as_str()))
    );
}

#[test]
fn documentation_link_text_preserves_default_refusal_and_accepts_raising() {
    let authored = format!(
        "https://example.test/{}",
        "x".repeat(rift_protocol::documentation::DOCUMENTATION_TEXT_BYTES_MAX as usize)
    );
    let text = format!("[guide]({authored})\n");
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let error = configured_collection_result(&text, &configuration, None)
        .expect_err("default link text bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.documentation_range_invalid"
    );
    let raised = rift_protocol::documentation::DocumentationConfiguration {
        max_text: rift_protocol::configuration::ByteSize::from_bytes(authored.len() as u64),
        ..configuration
    };
    let next = configured_collection(&text, &raised, None);
    assert_eq!(next.index().coverage.omitted, 0);
    assert_eq!(next.index().links.len(), 1);
    assert_eq!(next.index().links[0].authored, authored);
}

#[test]
fn final_publication_counts_keep_the_selected_reference_bound() {
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let collection = configured_collection(
        "[one](https://example.test/one) [two](https://example.test/two)\n",
        &configuration,
        None,
    );
    assert_eq!(collection.index().links.len(), 2);
    let selected = rift_protocol::documentation::DocumentationConfiguration {
        max_references: 1,
        ..configuration
    };
    let limits = DocumentationLimits::from_configuration(&selected).expect("selected bounds");
    let error = DocumentationCollection::new_with_limits(collection.index().clone(), Some(limits))
        .expect_err("final reference bound");
    assert_eq!(
        error.slug().as_str(),
        "rift.analysis.documentation_limit_exceeded"
    );
    assert!(
        error
            .context()
            .any(|(key, value)| key == "field" && value == "links")
    );
}

#[test]
fn accepted_documentation_source_and_aggregate_bounds_are_checked() {
    let text = "body";
    let configuration = rift_protocol::documentation::DocumentationConfiguration {
        max_sources: 1,
        max_file: rift_protocol::configuration::ByteSize::from_bytes(4),
        max_total: rift_protocol::configuration::ByteSize::from_bytes(4),
        ..Default::default()
    };
    let limits = DocumentationLimits::from_configuration(&configuration).expect("accepted bounds");
    assert!(limits.check_source_count(1).is_ok());
    assert!(limits.check_source_count(2).is_err());
    let first = DocumentationInput::with_limits(
        source("first.md", text, DocumentationSourceFormat::Markdown),
        text,
        &limits,
    )
    .expect("exact source bound");
    assert!(DocumentationSourceSet::with_limits(vec![first.clone()], &limits).is_ok());
    let over = "body!";
    assert!(
        DocumentationInput::with_limits(
            source("over.md", over, DocumentationSourceFormat::Markdown),
            over,
            &limits,
        )
        .is_err()
    );
    let second = DocumentationInput::with_limits(
        source("second.md", text, DocumentationSourceFormat::Markdown),
        text,
        &limits,
    )
    .expect("second source");
    let aggregate = DocumentationLimits::from_configuration(
        &rift_protocol::documentation::DocumentationConfiguration {
            max_sources: 2,
            ..configuration
        },
    )
    .expect("accepted count");
    assert!(DocumentationSourceSet::with_limits(vec![first, second], &aggregate).is_err());
}

#[test]
fn accepted_notebook_parser_bounds_are_checked() {
    let text = r#"{"cells":[],"metadata":{}}"#;
    let identity = source("guide.ipynb", text, DocumentationSourceFormat::Notebook).identity;
    assert!(notebook::decode_notebook(text, &identity).is_ok());
    for configuration in [
        rift_protocol::documentation::DocumentationConfiguration {
            max_nodes: 1,
            ..Default::default()
        },
        rift_protocol::documentation::DocumentationConfiguration {
            max_depth: 1,
            ..Default::default()
        },
    ] {
        let limits =
            DocumentationLimits::from_configuration(&configuration).expect("accepted parser bound");
        assert!(notebook::decode_notebook_with_limits(text, &identity, &limits).is_err());
    }
}

#[test]
fn documentation_file_bytes_can_raise_above_default() {
    let text =
        "x".repeat(rift_protocol::documentation::DOCUMENTATION_SOURCE_BYTES_MAX as usize + 1);
    let facts = source("guide.md", &text, DocumentationSourceFormat::Markdown);
    assert!(DocumentationInput::new(facts.clone(), &text).is_err());
    let configuration = rift_protocol::documentation::DocumentationConfiguration {
        max_file: rift_protocol::configuration::ByteSize::from_bytes(text.len() as u64),
        ..Default::default()
    };
    let limits = DocumentationLimits::from_configuration(&configuration).expect("raised bound");
    let input = DocumentationInput::with_limits(facts, &text, &limits).expect("accepted source");
    let sources = DocumentationSourceSet::with_limits(vec![input], &limits).expect("source set");
    let collection = collect_documentation(&sources, &[]).expect("collection");
    assert_eq!(collection.index().coverage.omitted, 0);
    assert_eq!(collection.index().sources[0].byte_length, text.len() as u64);
}

#[test]
fn documentation_layer_bounds_accept_exact_and_refuse_one_over() {
    let configuration = rift_protocol::documentation::DocumentationConfiguration::default();
    let collection = configured_collection("# Guide\n\nParagraph.\n", &configuration, None);
    let count = u32::try_from(collection.index().blocks.len()).expect("block count");
    assert!(count > 1);
    let exact = rift_protocol::documentation::DocumentationConfiguration {
        max_layer_blocks: count,
        ..configuration.clone()
    };
    let limits = DocumentationLimits::from_configuration(&exact).expect("exact bounds");
    assert!(DocumentationLayer::borrowed_with_limits(&[&collection], &limits).is_ok());
    let lower = rift_protocol::documentation::DocumentationConfiguration {
        max_layer_blocks: count - 1,
        ..configuration.clone()
    };
    let limits = DocumentationLimits::from_configuration(&lower).expect("lowered block bound");
    assert!(DocumentationLayer::borrowed_with_limits(&[&collection], &limits).is_err());
    let lower = rift_protocol::documentation::DocumentationConfiguration {
        max_mappings: 1,
        ..configuration
    };
    let limits = DocumentationLimits::from_configuration(&lower).expect("lowered mapping bound");
    assert!(DocumentationLayer::borrowed_with_limits(&[&collection], &limits).is_err());
}
