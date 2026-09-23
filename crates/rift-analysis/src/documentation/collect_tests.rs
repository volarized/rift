use super::*;
use rift_protocol::documentation::*;
use rift_protocol::read::{
    Digest, ProjectPath, SourceKind, SourceLocationKind, SymbolOrigin, TextRange,
};
use rift_syntax::{
    MarkdownSyntaxProvider, PythonSyntaxProvider, RustSyntaxProvider, SyntaxProvider, SyntaxSource,
};

fn source(path: &str, text: &str) -> DocumentationSource {
    let (format, media_type) = match path.rsplit('.').next().expect("extension") {
        "md" => (DocumentationSourceFormat::Markdown, "text/markdown"),
        "mdx" => (DocumentationSourceFormat::Mdx, "text/mdx"),
        "rst" => (DocumentationSourceFormat::RestructuredText, "text/x-rst"),
        "rs" | "py" => (DocumentationSourceFormat::AttachedComment, "text/markdown"),
        "txt" => (DocumentationSourceFormat::Text, "text/plain"),
        extension => panic!("unexpected fixture extension: {extension}"),
    };
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
        media_type: media_type.to_owned(),
        selection: if format == DocumentationSourceFormat::AttachedComment {
            DocumentationSelectionReason::AttachedComment
        } else {
            DocumentationSelectionReason::Workspace
        },
        byte_length: text.len() as u64,
        language: None,
        physical_ranges: Vec::new(),
        license: None,
    }
}

fn input<'a>(path: &str, text: &'a str) -> DocumentationInput<'a> {
    let chunks = crate::text_chunks(text, 16)
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| DocumentationChunk {
            identity: format!("{path}#{index}"),
            range: TextRange {
                start: chunk.byte_offset(),
                end: chunk.byte_offset() + chunk.content().len() as u64,
            },
        })
        .collect();
    DocumentationInput::new(source(path, text), text)
        .expect("source")
        .with_chunks(chunks)
        .expect("partition")
}

fn exact_text<'a>(text: &'a str, range: &TextRange) -> &'a str {
    let start = usize::try_from(range.start).expect("source range start fits");
    let end = usize::try_from(range.end).expect("source range end fits");
    &text[start..end]
}

fn collect(path: &str, text: &str) -> DocumentationCollection {
    collect_documentation(
        &DocumentationSourceSet::new(vec![input(path, text)]).expect("sources"),
        &[],
    )
    .expect("collection")
}

fn compass_collection(text: &str) -> (DocumentationCollection, rift_protocol::read::SymbolId) {
    let symbol =
        rift_protocol::read::SymbolId(rift_core::symbol_identity("rust", "lib.rs", "Compass"));
    let owner = source("lib.rs", "pub struct Compass;").identity;
    let language = rift_protocol::read::Language::from_identity_segment("rust").expect("language");
    let declaration = DocumentationDeclaration::new(
        &symbol,
        &language,
        "Compass",
        "Compass",
        &owner,
        TextRange { start: 0, end: 19 },
    )
    .expect("declaration");
    let sources = DocumentationSourceSet::new(vec![input("guide.md", text)]).expect("sources");
    let collection = collect_documentation(&sources, &[declaration]).expect("collection");
    (collection, symbol)
}

#[test]
fn authored_declaration_links_resolve_without_selecting_or_fetching_code() {
    let text = "Use [Compass](lib.rs#Compass), [address](rift://symbol/rust/lib.rs/Compass), and `Compass`.\n";
    let (collection, symbol) = compass_collection(text);
    assert_eq!(collection.index().references.len(), 3);
    assert_eq!(collection.index().sources.len(), 1);
    for link in &collection.index().links {
        assert!(
            matches!(&link.resolution, DocumentationLinkResolution::Resolved { target: DocumentationTarget::Symbol { symbol: target } } if *target == symbol)
        );
    }
    assert_eq!(
        collection
            .references_to(&symbol)
            .filter(|reference| reference.evidence == DocumentationReferenceEvidence::AuthoredLink)
            .count(),
        2
    );
    let (absent, _) = compass_collection("[Missing](lib.rs#Missing)\n");
    assert!(absent.index().references.is_empty());
    assert!(matches!(
        absent.index().links[0].resolution,
        DocumentationLinkResolution::Unresolved { .. }
    ));
}

#[test]
fn context_shares_excerpt_budget_and_reports_utf8_cut_and_missing_source() {
    let text = "αβγδ `Compass`.\n";
    let (collection, symbol) = compass_collection(text);
    let mut bytes_left = 7;
    let first =
        documentation_context_with_budget(&collection, &symbol, |_| Some(text), &mut bytes_left);
    assert_eq!(first.references[0].excerpt.as_deref(), Some("αβγ"));
    assert_eq!(bytes_left, 1);
    assert!(first.truncated);
    assert_eq!(
        first.warnings[0].kind,
        DocumentationWarningKind::LimitExceeded
    );
    let second =
        documentation_context_with_budget(&collection, &symbol, |_| Some(text), &mut bytes_left);
    assert!(second.references[0].excerpt.is_none());
    let missing = documentation_context(&collection, &symbol, |_| None);
    assert_eq!(missing.references.len(), 1);
    assert_eq!(
        missing.warnings[0].kind,
        DocumentationWarningKind::SourceUnavailable
    );
    validate_documentation_context(&first, &symbol).expect("valid bounded context");
    validate_documentation_context(&second, &symbol).expect("valid context without excerpt");
    validate_documentation_context(&missing, &symbol).expect("valid missing-source context");
}

#[test]
fn context_reports_captured_source_truncated_before_block_range() {
    let text = "`Compass` is documented here.\n";
    let (collection, symbol) = compass_collection(text);
    let context = documentation_context(&collection, &symbol, |_| Some("short"));

    assert_eq!(context.references.len(), 1);
    assert!(context.references[0].excerpt.is_none());
    assert_eq!(context.warnings.len(), 1);
    assert_eq!(
        context.warnings[0].kind,
        DocumentationWarningKind::SourceTruncated
    );
}

#[test]
fn attached_comment_without_matching_syntax_is_omitted_with_warning() {
    let text = "pub fn serve() {}\n";
    let collection = collect("src/lib.rs", text);

    assert_eq!(collection.index().coverage.selected, 1);
    assert_eq!(collection.index().coverage.parsed, 0);
    assert_eq!(collection.index().coverage.omitted, 1);
    assert!(collection.index().blocks.is_empty());
    assert!(collection.index().warnings.iter().any(|warning| {
        warning.kind == DocumentationWarningKind::UnsupportedFormat
            && warning.stage == DocumentationStage::Extract
    }));
}

#[test]
fn context_reference_bound_returns_deterministic_prefix_and_warning() {
    let text = "`Compass`\n\n".repeat(DOCUMENTATION_SYMBOL_REFERENCES_MAX as usize + 1);
    let (collection, symbol) = compass_collection(&text);
    let context = documentation_context(&collection, &symbol, |_| Some(&text));
    assert_eq!(
        context.references.len(),
        DOCUMENTATION_SYMBOL_REFERENCES_MAX as usize
    );
    assert!(context.truncated);
    assert_eq!(context.warnings.len(), 1);
    assert_eq!(
        context.warnings[0].kind,
        DocumentationWarningKind::LimitExceeded
    );
    let expected: Vec<_> = collection
        .references_to(&symbol)
        .take(DOCUMENTATION_SYMBOL_REFERENCES_MAX as usize)
        .map(|reference| &reference.identity)
        .collect();
    let observed: Vec<_> = context
        .references
        .iter()
        .map(|hit| &hit.reference.identity)
        .collect();
    assert_eq!(observed, expected);
    validate_documentation_context(&context, &symbol).expect("valid bounded reference context");
    let subset = DocumentationCollection::from_candidate_blocks(
        collection.index().documentation_revision.clone(),
        collection.index().sources.clone(),
        collection.index().blocks.clone(),
    )
    .expect("projection metadata");
    assert_eq!(subset.index().blocks, collection.index().blocks);
    assert!(subset.index().references.is_empty());
    assert!(
        DocumentationCollection::from_candidate_blocks(
            Digest("oldrev".to_owned()),
            collection.index().sources.clone(),
            collection.index().blocks.clone()
        )
        .is_err()
    );
}

#[test]
fn markdown_metadata_maps_exact_bytes_without_retaining_text() {
    for ending in ["\n", "\r\n"] {
        let text = format!(
            "# Guide{ending}{ending}Use `Beacon`.{ending}{ending}```rust{ending}let x = 1;{ending}```{ending}"
        );
        let collection = collect("README.md", &text);
        let index = collection.index();
        assert_eq!(index.coverage.parsed, 1);
        assert!(
            index
                .blocks
                .iter()
                .any(|block| block.kind == DocumentationBlockKind::Code
                    && block.language.as_deref() == Some("rust"))
        );
        for block in &index.blocks {
            let exact = exact_text(&text, &block.range);
            assert_eq!(content_digest(exact.as_bytes()), block.content_digest);
            assert!(!block.chunks.is_empty());
            assert_eq!(block.heading_path.last().expect("heading").name, "Guide");
        }
        assert_eq!(index.unresolved_references.len(), 1);
        assert_eq!(index.unresolved_references[0].authored, "Beacon");
        let encoded = serde_json::to_value(index).expect("JSON");
        assert!(encoded["blocks"][0].get("text").is_none());
    }
}

#[test]
fn supplied_markdown_syntax_produces_same_metadata_as_owned_parse() {
    let text = "# Notes\n\nOne paragraph.\n\n## Notes\nTwo.\n";
    let path = rift_core::ProjectPath::new("README.md").expect("path");
    let syntax = MarkdownSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text })
        .expect("syntax");
    let sources = DocumentationSourceSet::new(vec![
        input("README.md", text)
            .with_syntax(&syntax)
            .expect("matching syntax"),
    ])
    .expect("set");
    assert_eq!(
        collect_documentation(&sources, &[])
            .expect("collection")
            .index(),
        collect("README.md", text).index()
    );
}

#[test]
fn rst_references_resolve_local_targets_and_keep_missing_or_ambiguous_names() {
    let text = concat!(
        "See target_ and external_ and missing_ and repeated_.\n\n",
        ".. _target:\n\n",
        ".. _external: https://example.invalid/path\n\n",
        ".. _Repeated:\n\n",
        ".. _repeated:\n",
    );
    let collection = collect("README.rst", text);
    let links = &collection.index().links;
    let local = links
        .iter()
        .find(|link| link.authored == "#target")
        .unwrap_or_else(|| panic!("local target link missing: {links:#?}"));
    assert!(matches!(
        local.resolution,
        DocumentationLinkResolution::Resolved {
            target: DocumentationTarget::Source { .. }
        }
    ));
    assert!(links.iter().any(|link| {
        link.authored == "https://example.invalid/path"
            && link.resolution
                == DocumentationLinkResolution::Unresolved {
                    reason: DocumentationUnresolvedReason::External,
                }
    }));
    assert!(links.iter().any(|link| {
        link.authored == "missing"
            && link.resolution
                == DocumentationLinkResolution::Unresolved {
                    reason: DocumentationUnresolvedReason::Missing,
                }
    }));
    assert!(links.iter().any(|link| {
        link.authored == "repeated"
            && link.resolution
                == DocumentationLinkResolution::Unresolved {
                    reason: DocumentationUnresolvedReason::Ambiguous,
                }
    }));
}

#[test]
fn parser_bound_omits_one_source_and_collects_next_source() {
    let mut nested = String::new();
    for _ in 0..100_001 {
        nested.push_str("item.\n\n");
    }
    let rst = DocumentationInput::new(source("guide.rst", &nested), &nested).expect("RST source");
    let markdown = input("z-guide.md", "# Guide\n\nKept.\n");
    let sources = DocumentationSourceSet::new(vec![rst, markdown]).expect("sources");

    let collection = collect_documentation(&sources, &[]).expect("partial collection");

    assert_eq!(collection.index().coverage.selected, 2);
    assert_eq!(collection.index().coverage.parsed, 1);
    assert_eq!(collection.index().coverage.omitted, 1);
    assert_eq!(collection.index().blocks.len(), 2);
    assert!(collection.index().blocks.iter().all(|block| {
        block.source.source
            == DocumentationSourceIdentity::Project {
                path: ProjectPath("z-guide.md".to_owned()),
            }
    }));
    assert!(collection.index().warnings.iter().any(|warning| {
        warning.source.source
            == DocumentationSourceIdentity::Project {
                path: ProjectPath("guide.rst".to_owned()),
            }
            && warning.kind == DocumentationWarningKind::LimitExceeded
            && warning.stage == DocumentationStage::Extract
    }));
}

#[test]
fn markdown_syntax_bound_omits_source_and_keeps_following_source() {
    let mut nested = String::new();
    for _ in 0..100_001 {
        nested.push_str("item\n\n");
    }
    let markdown = DocumentationInput::new(source("guide.md", &nested), &nested)
        .expect("bounded source bytes");
    let next = input("z-guide.md", "# Guide\n\nKept.\n");
    let sources = DocumentationSourceSet::new(vec![markdown, next]).expect("sources");

    let collection = collect_documentation(&sources, &[]).expect("partial collection");

    assert_eq!(collection.index().coverage.selected, 2);
    assert_eq!(collection.index().coverage.parsed, 1);
    assert_eq!(collection.index().coverage.omitted, 1);
    assert_eq!(collection.index().blocks.len(), 2);
    assert!(collection.index().warnings.iter().any(|warning| {
        warning.source.source
            == DocumentationSourceIdentity::Project {
                path: ProjectPath("guide.md".to_owned()),
            }
            && warning.kind == DocumentationWarningKind::MalformedSource
            && warning.stage == DocumentationStage::Extract
    }));
}

#[test]
fn supplied_syntax_with_same_path_and_different_bytes_is_refused() {
    let text = "# Guide\n";
    let other_text = "# Other\n";
    let path = rift_core::ProjectPath::new("README.md").expect("path");
    let syntax = MarkdownSyntaxProvider::default()
        .analyze(SyntaxSource {
            path: &path,
            text: other_text,
        })
        .expect("syntax");
    let error = input("README.md", text)
        .with_syntax(&syntax)
        .expect_err("unrelated syntax facts");
    assert_eq!(error.fault().violation(), DocumentationViolation::Format);
}

#[test]
fn attached_comment_blocks_keep_original_bytes_and_exact_symbol() {
    let text = "/// Answers one request.\npub fn serve() {}\n";
    let path = rift_core::ProjectPath::new("src/lib.rs").expect("path");
    let syntax = RustSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text })
        .expect("Rust syntax");
    let symbol = &syntax.symbols()[0];
    let symbol_id = rift_protocol::read::SymbolId(rift_core::symbol_identity(
        &syntax.language().identity_segment(),
        path.as_str(),
        &symbol.qualified_name,
    ));
    let identity = DocumentationContentIdentity {
        source: DocumentationSourceIdentity::Project {
            path: ProjectPath("src/lib.rs".to_owned()),
        },
        cell: None,
    };
    let declaration = DocumentationDeclaration::new(
        &symbol_id,
        syntax.language(),
        &symbol.name,
        &symbol.qualified_name,
        &identity,
        TextRange {
            start: symbol.range.start,
            end: symbol.range.end,
        },
    )
    .expect("declaration");
    let sources = DocumentationSourceSet::new(vec![
        input("src/lib.rs", text)
            .with_syntax(&syntax)
            .expect("matching syntax"),
    ])
    .expect("sources");

    let collection = collect_documentation(&sources, &[declaration]).expect("collection");
    let block = collection
        .index()
        .blocks
        .first()
        .expect("attached comment block");
    assert_eq!(block.symbol.as_ref(), Some(&symbol_id));
    assert_eq!(exact_text(text, &block.range), "/// Answers one request.\n");
    assert!(!block.chunks.is_empty());
}

#[test]
fn python_docstring_blocks_keep_exact_content_range_and_symbol() {
    let text = "def serve():\n    \"\"\"Answers one request.\"\"\"\n    return True\n";
    let path = rift_core::ProjectPath::new("src/app.py").expect("path");
    let syntax = PythonSyntaxProvider::default()
        .analyze(SyntaxSource { path: &path, text })
        .expect("Python syntax");
    let symbol = syntax
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == "serve")
        .expect("function symbol");
    let symbol_id = rift_protocol::read::SymbolId(rift_core::symbol_identity(
        &syntax.language().identity_segment(),
        path.as_str(),
        &symbol.qualified_name,
    ));
    let identity = source("src/app.py", text).identity;
    let declaration = DocumentationDeclaration::new(
        &symbol_id,
        syntax.language(),
        &symbol.name,
        &symbol.qualified_name,
        &identity,
        TextRange {
            start: symbol.range.start,
            end: symbol.range.end,
        },
    )
    .expect("declaration");
    let sources = DocumentationSourceSet::new(vec![
        input("src/app.py", text)
            .with_syntax(&syntax)
            .expect("matching syntax"),
    ])
    .expect("sources");

    let collection = collect_documentation(&sources, &[declaration]).expect("collection");
    let block = collection.index().blocks.first().expect("docstring block");
    assert_eq!(block.symbol.as_ref(), Some(&symbol_id));
    assert_eq!(exact_text(text, &block.range), "Answers one request.");
}

#[test]
fn block_identity_survives_offset_and_content_changes() {
    let before = collect("README.md", "# Guide\n\nFirst.\n");
    let after = collect("README.md", "\n\n# Guide\n\nChanged.\n");
    assert_eq!(before.index().blocks.len(), after.index().blocks.len());
    for (before, after) in before.index().blocks.iter().zip(&after.index().blocks) {
        assert_eq!(before.identity, after.identity);
        assert_ne!(before.range, after.range);
    }
    let changes = after.changes_from(Some(&before));
    assert!(changes.blocks.added.is_empty());
    assert_eq!(changes.blocks.replaced.len(), before.index().blocks.len());
}

#[test]
fn plain_text_paragraphs_preserve_crlf_and_missing_final_newline() {
    let text = "First\r\ncontinued.\r\n\r\nSecond.";
    let collection = collect("guide.txt", text);
    let blocks = &collection.index().blocks;
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        exact_text(text, &blocks[0].range),
        "First\r\ncontinued.\r\n"
    );
    assert_eq!(exact_text(text, &blocks[1].range), "Second.");
    assert_eq!(blocks[1].line, 4);
}

#[test]
fn local_links_resolve_and_generated_fragments_remain_unresolved() {
    let text = "# Guide\n\n[Other](other.md) [heading](other.md#title) [remote](https://example.com) [ref][other]\n\n[other]: other.md\n";
    let sources = DocumentationSourceSet::new(vec![
        input("README.md", text),
        input("other.md", "# Title\n"),
    ])
    .expect("set");
    let collection = collect_documentation(&sources, &[]).expect("collection");
    let links = &collection.index().links;
    assert!(links.iter().any(|link| matches!(
        link.resolution,
        DocumentationLinkResolution::Unresolved {
            reason: DocumentationUnresolvedReason::Fragment
        }
    )));
    assert!(links.iter().any(|link| matches!(
        link.resolution,
        DocumentationLinkResolution::Unresolved {
            reason: DocumentationUnresolvedReason::External
        }
    )));
    assert!(
        links
            .iter()
            .filter(|link| link.authored == "other.md"
                && matches!(
                    link.resolution,
                    DocumentationLinkResolution::Resolved { .. }
                ))
            .count()
            >= 2
    );
}

#[test]
fn authored_destinations_refuse_invalid_paths_and_never_fetch_external_urls() {
    let text = concat!(
        "[outside](../../outside.md) ",
        "[query](next.md?view=full) ",
        "[backslash](folder\\file.md) ",
        "[control](bad%00path.md) ",
        "[remote](//example.invalid/guide)\n",
    );
    let collection = collect("docs/README.md", text);
    let links = &collection.index().links;

    for authored in [
        "../../outside.md",
        "next.md?view=full",
        "folder\\file.md",
        "bad%00path.md",
    ] {
        assert!(
            links.iter().any(|link| {
                link.authored == authored
                    && link.resolution
                        == DocumentationLinkResolution::Unresolved {
                            reason: DocumentationUnresolvedReason::Invalid,
                        }
            }),
            "expected invalid destination: {authored}; links={links:#?}"
        );
    }
    assert!(links.iter().any(|link| {
        link.authored == "//example.invalid/guide"
            && link.resolution
                == DocumentationLinkResolution::Unresolved {
                    reason: DocumentationUnresolvedReason::External,
                }
    }));
}

#[test]
fn mdx_omitted_construct_stays_out_of_metadata() {
    let text = "# Guide\n\nPlain.\n\n<Component />\n\n{value}\n\n```jsx\n<Component />\n```\n";
    let collection = collect("guide.mdx", text);
    assert!(
        collection
            .index()
            .warnings
            .iter()
            .any(|warning| warning.kind == DocumentationWarningKind::OmittedRange)
    );
    assert!(
        collection
            .index()
            .blocks
            .iter()
            .any(|block| block.kind == DocumentationBlockKind::Code)
    );
    assert!(
        !collection
            .index()
            .blocks
            .iter()
            .any(|block| block.kind == DocumentationBlockKind::Prose
                && exact_text(text, &block.range).contains("{value}"))
    );
}

#[test]
fn incomplete_or_overlapping_baseline_partition_is_refused() {
    for ranges in [
        vec![(0, 2)],
        vec![(0, 2), (1, 4)],
        vec![(1, 4)],
        vec![(0, 5)],
    ] {
        let chunks = ranges
            .into_iter()
            .enumerate()
            .map(|(index, (start, end))| DocumentationChunk {
                identity: format!("README.md#{index}"),
                range: TextRange { start, end },
            })
            .collect();
        assert!(
            DocumentationInput::new(source("README.md", "text"), "text")
                .expect("input")
                .with_chunks(chunks)
                .is_err()
        );
    }
}

#[test]
fn removal_publishes_without_stale_blocks() {
    let before = collect("README.md", "# Before\n\n`Missing`\n");
    let empty = collect_documentation(
        &DocumentationSourceSet::new(Vec::new()).expect("empty"),
        &[],
    )
    .expect("collection");
    assert_eq!(
        empty.changes_from(Some(&before)).blocks.removed.len(),
        before.index().blocks.len()
    );
    assert_eq!(empty.index().coverage, DocumentationCoverage::default());
    assert!(collect("empty.txt", "").index().blocks.is_empty());
}
