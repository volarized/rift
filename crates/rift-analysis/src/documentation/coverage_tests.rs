use super::*;
use rift_protocol::documentation::{
    DocumentationBlockKind, DocumentationChunk, DocumentationContentIdentity, DocumentationDigest,
    DocumentationHeading, DocumentationLinkResolution, DocumentationSource,
    DocumentationSourceFormat, DocumentationSourceIdentity, DocumentationStage,
    DocumentationTarget, DocumentationWarning, DocumentationWarningKind,
};
use rift_protocol::read::{
    ProjectPath, SourceKind, SourceLocationKind, SymbolId, SymbolOrigin, TextRange,
};

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

fn collect(path: &str, text: &str, format: DocumentationSourceFormat) -> DocumentationCollection {
    let input = DocumentationInput::new(source(path, text, format), text).expect("valid source");
    let sources = DocumentationSourceSet::new(vec![input]).expect("valid source set");
    collect_documentation(&sources, &[]).expect("valid collection")
}

fn candidate() -> rift_protocol::documentation::DocumentationIndex {
    collect(
        "guide.md",
        "# Guide\n\n[Next](next.md)\n\nLast paragraph.\n",
        DocumentationSourceFormat::Markdown,
    )
    .into_index()
}

#[test]
fn markdown_collection_maps_list_table_and_block_quote() {
    let text = concat!(
        "- first\n- second\n\n",
        "| name | value |\n| --- | --- |\n| a | b |\n\n",
        "> quoted text\n",
    );
    let collection = collect("guide.md", text, DocumentationSourceFormat::Markdown);
    let addressed = collection
        .index()
        .blocks
        .iter()
        .map(|block| {
            let start = usize::try_from(block.range.start).expect("range start");
            let end = usize::try_from(block.range.end).expect("range end");
            &text[start..end]
        })
        .collect::<Vec<_>>();

    for content in ["- first", "| name | value |", "> quoted text"] {
        assert!(
            addressed.iter().any(|block| block.contains(content)),
            "missing structural block content {content}: {addressed:?}"
        );
    }
}

#[test]
fn rst_inline_target_resolves_authored_reference() {
    let text = "See `Inline target`_.\n\n_`Inline target`\n";
    let collection = collect(
        "guide.rst",
        text,
        DocumentationSourceFormat::RestructuredText,
    );
    let link = collection
        .index()
        .links
        .iter()
        .find(|link| link.authored == "#Inline target")
        .expect("inline target link");

    assert!(matches!(
        &link.resolution,
        DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Source { .. }
        }
    ));
}

#[test]
fn authored_local_destinations_classify_invalid_encoding_query_and_escape() {
    let text = concat!(
        "[fragment](#%FF) ",
        "[path](bad%FF.md) ",
        "[query](next.md?view=full) ",
        "[escape](../../outside.md)\n",
    );
    let collection = collect("docs/guide.md", text, DocumentationSourceFormat::Markdown);

    for authored in ["#%FF", "bad%FF.md", "next.md?view=full", "../../outside.md"] {
        assert!(collection.index().links.iter().any(|link| {
            link.authored == authored
                && link.resolution
                    == DocumentationLinkResolution::Unresolved {
                        reason: rift_protocol::documentation::DocumentationUnresolvedReason::Invalid,
                    }
        }), "missing invalid destination {authored}");
    }
}

#[test]
fn link_resolution_rejects_invalid_utf8_and_marks_duplicate_fragments_ambiguous() {
    let index = candidate();
    let source = index.sources[0].clone();
    let block = index.blocks[1].clone();
    let mut link = index.links[0].clone();
    link.authored = "#%FF".to_owned();
    resolve_links(
        std::slice::from_ref(&source),
        std::slice::from_ref(&block),
        std::slice::from_mut(&mut link),
        &[],
    )
    .expect("invalid authored fragment remains unresolved");
    assert_eq!(
        link.resolution,
        DocumentationLinkResolution::Unresolved {
            reason: rift_protocol::documentation::DocumentationUnresolvedReason::Invalid,
        }
    );

    let duplicate_fragments = ["same", "same"].map(|name| DocumentationFragment {
        source: source.identity.clone(),
        name: name.to_owned(),
        range: TextRange { start: 0, end: 1 },
    });
    link.authored = "#same".to_owned();
    resolve_links(
        std::slice::from_ref(&source),
        std::slice::from_ref(&block),
        std::slice::from_mut(&mut link),
        &duplicate_fragments,
    )
    .expect("duplicate fragments remain unresolved");
    assert_eq!(
        link.resolution,
        DocumentationLinkResolution::Unresolved {
            reason: rift_protocol::documentation::DocumentationUnresolvedReason::Ambiguous,
        }
    );
}

#[test]
fn initial_publication_reports_added_source_blocks_and_links() {
    let collection = DocumentationCollection::new(candidate()).expect("valid publication");
    let changes = collection.changes_from(None);

    assert_eq!(changes.sources.added.len(), 1);
    assert!(!changes.blocks.added.is_empty());
    assert!(!changes.links.added.is_empty());
    let unchanged = collection.changes_from(Some(&collection));
    assert_eq!(unchanged.sources, DocumentationSourceChanges::default());
    assert_eq!(unchanged.blocks, DocumentationRecordChanges::default());
    assert_eq!(unchanged.links, DocumentationLinkChanges::default());
}

#[test]
fn malformed_publication_facts_are_refused_at_boundary() {
    let mut cases = invalid_source_and_block_cases();
    cases.extend(invalid_chunk_cases());
    cases.extend(invalid_link_and_order_cases());
    for case in cases {
        let mut index = if case.references {
            reference_candidate()
        } else {
            candidate()
        };
        (case.mutate)(&mut index);
        let error = DocumentationCollection::new(index).expect_err(case.name);
        assert_eq!(error.fault().violation(), case.expected, "{}", case.name);
    }
}

type InvalidMutation = fn(&mut rift_protocol::documentation::DocumentationIndex);
type InvalidPublicationFacts = (&'static str, InvalidMutation, super::DocumentationViolation);

struct InvalidPublicationCase {
    name: &'static str,
    mutate: InvalidMutation,
    expected: super::DocumentationViolation,
    references: bool,
}

fn invalid_source_and_block_cases() -> Vec<InvalidPublicationCase> {
    use super::DocumentationViolation as Violation;
    let cases: [InvalidPublicationFacts; 8] = [
        (
            "selection digest",
            |index| index.selection_digest = content_digest(b"wrong"),
            Violation::Digest,
        ),
        (
            "block order",
            |index| index.blocks.swap(0, 1),
            Violation::Order,
        ),
        (
            "block source",
            |index| {
                index.blocks[0].source.source = DocumentationSourceIdentity::Project {
                    path: ProjectPath("missing.md".to_owned()),
                }
            },
            Violation::MissingTarget,
        ),
        (
            "block identity",
            |index| index.blocks[0].identity = DocumentationDigest("bad".to_owned()),
            Violation::Digest,
        ),
        (
            "block symbol",
            |index| index.blocks[0].symbol = Some(SymbolId("invalid".to_owned())),
            Violation::Identity,
        ),
        (
            "block language",
            |index| index.blocks[0].language = Some("rust".to_owned()),
            Violation::Format,
        ),
        (
            "heading order",
            |index| {
                index.blocks[0].heading_path = vec![DocumentationHeading {
                    level: 0,
                    name: "Guide".to_owned(),
                }];
            },
            Violation::Order,
        ),
        (
            "heading bound",
            |index| {
                index.blocks[0].heading_path = (1..=514)
                    .map(|level| DocumentationHeading {
                        level,
                        name: "Guide".to_owned(),
                    })
                    .collect();
            },
            Violation::LimitExceeded,
        ),
    ];
    cases
        .into_iter()
        .map(|(name, mutate, expected)| InvalidPublicationCase {
            name,
            mutate,
            expected,
            references: false,
        })
        .collect()
}

fn invalid_chunk_cases() -> Vec<InvalidPublicationCase> {
    use super::DocumentationViolation as Violation;
    let cases: [InvalidPublicationFacts; 2] = [
        (
            "chunk range",
            |index| {
                index.blocks[0].chunks = vec![DocumentationChunk {
                    identity: "guide.md#0".to_owned(),
                    range: TextRange {
                        start: 0,
                        end: u64::MAX,
                    },
                }];
            },
            Violation::Range,
        ),
        (
            "chunk bound",
            |index| {
                let chunk = DocumentationChunk {
                    identity: "guide.md#0".to_owned(),
                    range: TextRange { start: 0, end: 1 },
                };
                index.blocks[0].chunks =
                    vec![
                        chunk;
                        rift_protocol::documentation::DOCUMENTATION_BLOCKS_MAX as usize + 1
                    ];
            },
            Violation::LimitExceeded,
        ),
    ];
    cases
        .into_iter()
        .map(|(name, mutate, expected)| InvalidPublicationCase {
            name,
            mutate,
            expected,
            references: false,
        })
        .collect()
}

fn invalid_link_and_order_cases() -> Vec<InvalidPublicationCase> {
    use super::DocumentationViolation as Violation;
    let cases: [InvalidPublicationFacts; 5] = [
        (
            "target block",
            |index| {
                index.links[0].resolution = DocumentationLinkResolution::Resolved {
                    target: DocumentationTarget::Block {
                        identity: content_digest(b"missing block"),
                    },
                };
            },
            Violation::MissingTarget,
        ),
        (
            "target source",
            |index| {
                index.links[0].resolution = DocumentationLinkResolution::Resolved {
                    target: DocumentationTarget::Source {
                        source: DocumentationContentIdentity {
                            source: DocumentationSourceIdentity::Project {
                                path: ProjectPath("missing.md".to_owned()),
                            },
                            cell: None,
                        },
                        range: TextRange { start: 0, end: 1 },
                    },
                };
            },
            Violation::MissingTarget,
        ),
        (
            "target range",
            |index| {
                let source = index.sources[0].identity.clone();
                index.links[0].resolution = DocumentationLinkResolution::Resolved {
                    target: DocumentationTarget::Source {
                        source,
                        range: TextRange {
                            start: 0,
                            end: u64::MAX,
                        },
                    },
                };
            },
            Violation::Range,
        ),
        (
            "warning source",
            |index| {
                index.warnings.push(DocumentationWarning {
                    source: DocumentationContentIdentity {
                        source: DocumentationSourceIdentity::Project {
                            path: ProjectPath("missing.md".to_owned()),
                        },
                        cell: None,
                    },
                    stage: DocumentationStage::Index,
                    kind: DocumentationWarningKind::LimitExceeded,
                    count: 1,
                });
            },
            Violation::MissingTarget,
        ),
        (
            "reference order",
            |index| index.references.swap(0, 1),
            Violation::Order,
        ),
    ];
    cases
        .into_iter()
        .map(|(name, mutate, expected)| InvalidPublicationCase {
            name,
            mutate,
            expected,
            references: name == "reference order",
        })
        .collect()
}

fn reference_candidate() -> rift_protocol::documentation::DocumentationIndex {
    let text = "Use `Compass` and `Beacon`.\n";
    let owner = source(
        "lib.rs",
        "pub struct Compass; pub struct Beacon;",
        DocumentationSourceFormat::AttachedComment,
    )
    .identity;
    let rust = rift_protocol::read::Language::from_identity_segment("rust").expect("Rust");
    let compass = SymbolId(rift_core::symbol_identity("rust", "lib.rs", "Compass"));
    let beacon = SymbolId(rift_core::symbol_identity("rust", "lib.rs", "Beacon"));
    let declarations = [
        DocumentationDeclaration::new(
            &compass,
            &rust,
            "Compass",
            "Compass",
            &owner,
            TextRange { start: 0, end: 19 },
        )
        .expect("Compass declaration"),
        DocumentationDeclaration::new(
            &beacon,
            &rust,
            "Beacon",
            "Beacon",
            &owner,
            TextRange { start: 20, end: 38 },
        )
        .expect("Beacon declaration"),
    ];
    let input = DocumentationInput::new(
        source("guide.md", text, DocumentationSourceFormat::Markdown),
        text,
    )
    .expect("source");
    let sources = DocumentationSourceSet::new(vec![input]).expect("source set");
    collect_documentation(&sources, &declarations)
        .expect("references")
        .into_index()
}

#[test]
fn block_kind_fixture_has_prose_and_code_metadata() {
    let collection = collect(
        "guide.md",
        "Text.\n\n```rust\nfn main() {}\n```\n",
        DocumentationSourceFormat::Markdown,
    );
    assert!(
        collection
            .index()
            .blocks
            .iter()
            .any(|block| block.kind == DocumentationBlockKind::Prose)
    );
    assert!(collection.index().blocks.iter().any(|block| {
        block.kind == DocumentationBlockKind::Code && block.language.as_deref() == Some("rust")
    }));
}
