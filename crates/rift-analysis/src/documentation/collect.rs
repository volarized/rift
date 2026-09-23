//! Extracts range metadata from selected source owners and existing syntax facts.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use rift_core::line::{line_of, line_starts, lines_inclusive};
use rift_protocol::documentation::{
    DOCUMENTATION_BLOCKS_MAX, DOCUMENTATION_REFERENCES_MAX, DOCUMENTATION_WARNINGS_MAX,
    DocumentationBlock, DocumentationBlockKind, DocumentationCoverage, DocumentationDigest,
    DocumentationHeading, DocumentationIndex, DocumentationLink, DocumentationLinkResolution,
    DocumentationReference, DocumentationReferenceCandidate, DocumentationSourceFormat,
    DocumentationStage, DocumentationUnresolvedReason, DocumentationWarning,
    DocumentationWarningKind, NotebookCellKind,
};
use rift_protocol::index::PACKAGE_SYMBOLS_MAX;
use rift_protocol::read::TextRange;
use rift_syntax::{
    ByteRange, MarkdownBlockKind, MarkdownSyntaxProvider, SyntaxDocument, SyntaxLimits,
    SyntaxProvider, SyntaxSource,
};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::{canonical_digest, content_digest};
use super::input::{slice, source_file_path};
use super::{
    DocumentationCollection, DocumentationDeclaration, DocumentationInput, DocumentationSourceSet,
};

/// Collects metadata without acquiring bytes or retaining another text copy.
///
/// Existing Markdown syntax is reused when supplied. A caller without syntax facts lets
/// this pass own the one Markdown parse. Sources, blocks, links, and references have
/// protocol bounds; ordered range lookups avoid comparing every block with every chunk.
///
/// # Errors
///
/// Refuses malformed source facts, invalid parser ranges, or output exceeding a bound.
pub fn collect_documentation(
    sources: &DocumentationSourceSet<'_>,
    declarations: &[DocumentationDeclaration<'_>],
) -> Result<DocumentationCollection, DocumentationError> {
    collect_documentation_incremental(None, sources, declarations)
}

/// Collects metadata while reusing parser facts from one prior process-local build.
///
/// Resolution updates blocks whose input or recorded dependency changed.
/// Cached parser facts remain bounded by the source and collection limits.
///
/// # Errors
///
/// Refuses malformed source facts, invalid parser ranges, or output exceeding a bound.
pub fn collect_documentation_incremental(
    previous: Option<&DocumentationCollection>,
    sources: &DocumentationSourceSet<'_>,
    declarations: &[DocumentationDeclaration<'_>],
) -> Result<DocumentationCollection, DocumentationError> {
    if declarations.len() > PACKAGE_SYMBOLS_MAX as usize {
        return Err(refused(DocumentationViolation::LimitExceeded, "references"));
    }
    let attached = attached_declaration_facts(declarations);
    let (mut output, cache) = collect_source_facts(previous, sources, declarations, &attached)?;
    let records = sources
        .sources()
        .iter()
        .map(|input| input.source().clone())
        .collect::<Vec<_>>();
    let resolution_cache = resolve_collected(previous, declarations, &records, &mut output)?;
    build_collection(sources, output, cache, resolution_cache)
}

fn collect_source_facts(
    previous: Option<&DocumentationCollection>,
    sources: &DocumentationSourceSet<'_>,
    declarations: &[DocumentationDeclaration<'_>],
    attached: &BTreeMap<
        rift_protocol::documentation::DocumentationContentIdentity,
        Vec<(
            rift_protocol::read::SymbolId,
            rift_protocol::read::TextRange,
        )>,
    >,
) -> Result<(Collected, ExtractionCache), DocumentationError> {
    let mut output = Collected::default();
    let mut cache = ExtractionCache::default();
    for input in sources.sources() {
        let key = extraction_key(
            input,
            attached.get(&input.source().identity).map(Vec::as_slice),
        )?;
        let facts = previous
            .and_then(DocumentationCollection::extraction_cache)
            .and_then(|previous| previous.sources.get(&input.source().identity))
            .filter(|cached| cached.key == key)
            .map_or_else(
                || extract_source_facts(input, declarations),
                |cached| Ok(Arc::clone(&cached.facts)),
            )?;
        if merge_source(input, &mut output, &facts)
            && cache
                .warning_count
                .checked_add(facts.warnings.len())
                .is_some_and(|count| count <= DOCUMENTATION_WARNINGS_MAX as usize)
        {
            cache.warning_count += facts.warnings.len();
            cache
                .sources
                .insert(input.source().identity.clone(), CachedSource { key, facts });
        }
    }
    Ok((output, cache))
}

fn resolve_collected(
    previous: Option<&DocumentationCollection>,
    declarations: &[DocumentationDeclaration<'_>],
    records: &[rift_protocol::documentation::DocumentationSource],
    output: &mut Collected,
) -> Result<Option<super::resolution::ResolutionCache>, DocumentationError> {
    let input = super::resolution::ResolutionInput {
        sources: records,
        blocks: &output.blocks,
        resolvable_links: &output.links,
        fixed_unresolved_links: &output.unresolved_links,
        fragments: &output.fragments,
        candidates: &output.candidates,
        declarations,
    };
    let resolved = super::resolution::ResolutionCache::resolve(
        previous.and_then(DocumentationCollection::resolution_cache),
        &input,
    )?;
    let mut links = resolved.resolvable_links;
    links.extend(resolved.fixed_unresolved_links);
    let references = resolved.references;
    let unresolved_references = resolved.unresolved_references;
    let resolution_cache = resolved.next_cache;
    output.blocks.sort_by(|a, b| {
        (&a.source, a.range.start, a.kind, &a.identity).cmp(&(
            &b.source,
            b.range.start,
            b.kind,
            &b.identity,
        ))
    });
    output.links = links;
    output.references = references;
    output.unresolved_references = unresolved_references;
    output.unresolved_links.clear();
    output.fragments.clear();
    output.candidates.clear();
    Ok(resolution_cache)
}

fn build_collection(
    sources: &DocumentationSourceSet<'_>,
    output: Collected,
    cache: ExtractionCache,
    resolution_cache: Option<super::resolution::ResolutionCache>,
) -> Result<DocumentationCollection, DocumentationError> {
    let selected = u32::try_from(sources.sources().len())
        .map_err(|_| refused(DocumentationViolation::LimitExceeded, "sources"))?;
    let records = sources
        .sources()
        .iter()
        .map(|input| input.source().clone())
        .collect::<Vec<_>>();
    DocumentationCollection::new(DocumentationIndex {
        documentation_revision: super::documentation_revision(),
        selection_digest: sources.selection_digest().clone(),
        sources: records,
        blocks: output.blocks,
        links: output.links,
        references: output.references,
        unresolved_references: output.unresolved_references,
        coverage: DocumentationCoverage {
            selected,
            parsed: selected - output.omitted,
            omitted: output.omitted,
            truncated: 0,
        },
        warnings: output.warnings,
    })
    .map(|collection| {
        collection
            .with_extraction_cache(cache)
            .with_resolution_cache(resolution_cache)
    })
}

#[derive(Clone, Debug, Default)]
pub(super) struct Collected {
    blocks: Vec<DocumentationBlock>,
    links: Vec<DocumentationLink>,
    unresolved_links: Vec<DocumentationLink>,
    references: Vec<DocumentationReference>,
    unresolved_references: Vec<DocumentationReferenceCandidate>,
    fragments: Vec<super::DocumentationFragment>,
    candidates: Vec<DocumentationReferenceCandidate>,
    warnings: Vec<DocumentationWarning>,
    omitted: u32,
}

#[derive(Debug, Default)]
pub(super) struct ExtractionCache {
    pub(super) sources:
        BTreeMap<rift_protocol::documentation::DocumentationContentIdentity, CachedSource>,
    warning_count: usize,
}

#[derive(Debug)]
pub(super) struct CachedSource {
    pub(super) key: DocumentationDigest,
    pub(super) facts: Arc<Collected>,
}

type AttachedSyntaxFacts = Option<(
    String,
    Vec<(rift_protocol::read::SymbolId, Vec<(u64, u64)>)>,
)>;

fn extraction_key(
    input: &DocumentationInput<'_>,
    attached: Option<
        &[(
            rift_protocol::read::SymbolId,
            rift_protocol::read::TextRange,
        )],
    >,
) -> Result<DocumentationDigest, DocumentationError> {
    let attached = attached.unwrap_or_default();
    let syntax_facts = attached_syntax_facts(input, attached)?;
    canonical_digest(&(
        input.source(),
        input.chunks(),
        super::documentation_revision(),
        attached,
        syntax_facts,
    ))
}

fn attached_syntax_facts(
    input: &DocumentationInput<'_>,
    attached: &[(
        rift_protocol::read::SymbolId,
        rift_protocol::read::TextRange,
    )],
) -> Result<AttachedSyntaxFacts, DocumentationError> {
    let Some(syntax) = input.syntax() else {
        return Ok(None);
    };
    let accepted: std::collections::BTreeSet<_> =
        attached.iter().map(|(symbol, _)| symbol.clone()).collect();
    let path = super::references::declaration_path(&input.source().identity)?;
    let facts = syntax
        .symbols()
        .iter()
        .filter_map(|symbol| {
            let identity = rift_core::symbol_identity(
                &syntax.language().identity_segment(),
                path.as_str(),
                &symbol.qualified_name,
            );
            let identity = rift_protocol::read::SymbolId(identity);
            if !accepted.contains(&identity) {
                return None;
            }
            Some((
                identity,
                symbol
                    .documentation_ranges
                    .iter()
                    .map(|range| (range.start, range.end))
                    .collect(),
            ))
        })
        .collect();
    Ok(Some((syntax.language().identity_segment(), facts)))
}

fn attached_declaration_facts(
    declarations: &[DocumentationDeclaration<'_>],
) -> BTreeMap<
    rift_protocol::documentation::DocumentationContentIdentity,
    Vec<(
        rift_protocol::read::SymbolId,
        rift_protocol::read::TextRange,
    )>,
> {
    let mut attached = BTreeMap::new();
    for declaration in declarations {
        attached
            .entry(declaration.source().clone())
            .or_insert_with(Vec::new)
            .push((declaration.symbol().clone(), declaration.range().clone()));
    }
    for facts in attached.values_mut() {
        facts.sort_by(|left, right| {
            (&left.0, left.1.start, left.1.end).cmp(&(&right.0, right.1.start, right.1.end))
        });
    }
    attached
}

fn extract_source_facts(
    input: &DocumentationInput<'_>,
    declarations: &[DocumentationDeclaration<'_>],
) -> Result<Arc<Collected>, DocumentationError> {
    let mut facts = Collected::default();
    if let Err(error) = extract_source(input, declarations, &mut facts) {
        let Some(kind) = recoverable_source_error(input, &error) else {
            return Err(error);
        };
        facts = Collected::default();
        facts.omitted = 1;
        warn(input, &mut facts, kind, 1);
    }
    Ok(Arc::new(facts))
}

fn merge_source(input: &DocumentationInput<'_>, output: &mut Collected, facts: &Collected) -> bool {
    let fits = output.blocks.len().saturating_add(facts.blocks.len())
        <= DOCUMENTATION_BLOCKS_MAX as usize
        && output
            .links
            .len()
            .saturating_add(output.unresolved_links.len())
            .saturating_add(facts.links.len())
            .saturating_add(facts.unresolved_links.len())
            <= DOCUMENTATION_REFERENCES_MAX as usize
        && output.fragments.len().saturating_add(facts.fragments.len())
            <= DOCUMENTATION_REFERENCES_MAX as usize
        && output
            .candidates
            .len()
            .saturating_add(facts.candidates.len())
            <= DOCUMENTATION_REFERENCES_MAX as usize;
    if !fits {
        output.omitted = output.omitted.saturating_add(1);
        warn(input, output, DocumentationWarningKind::LimitExceeded, 1);
        return false;
    }
    output.blocks.extend(facts.blocks.iter().cloned());
    output.links.extend(facts.links.iter().cloned());
    output
        .unresolved_links
        .extend(facts.unresolved_links.iter().cloned());
    output.fragments.extend(facts.fragments.iter().cloned());
    output.candidates.extend(facts.candidates.iter().cloned());
    for warning in &facts.warnings {
        warn(input, output, warning.kind, warning.count);
    }
    output.omitted = output.omitted.saturating_add(facts.omitted);
    true
}

fn recoverable_source_error(
    input: &DocumentationInput<'_>,
    error: &DocumentationError,
) -> Option<DocumentationWarningKind> {
    let fault = error.fault();
    match fault.violation() {
        DocumentationViolation::LimitExceeded => Some(DocumentationWarningKind::LimitExceeded),
        DocumentationViolation::Format
            if fault.field() == "markdown"
                && matches!(
                    input.source().format,
                    DocumentationSourceFormat::Markdown | DocumentationSourceFormat::Mdx
                ) =>
        {
            Some(DocumentationWarningKind::MalformedSource)
        }
        _ => None,
    }
}

fn extract_source(
    input: &DocumentationInput<'_>,
    declarations: &[DocumentationDeclaration<'_>],
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    use DocumentationSourceFormat as Format;
    match input.source().format {
        Format::Markdown | Format::Mdx => extract_markdown(input, output),
        Format::Notebook => extract_cell(input, output),
        Format::Text => extract_text(input, output),
        Format::RestructuredText => extract_rst(input, output),
        Format::AttachedComment => extract_attached_comments(input, declarations, output),
    }
}

fn extract_attached_comments(
    input: &DocumentationInput<'_>,
    declarations: &[DocumentationDeclaration<'_>],
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    let Some(syntax) = input.syntax() else {
        output.omitted += 1;
        warn(
            input,
            output,
            DocumentationWarningKind::UnsupportedFormat,
            1,
        );
        return Ok(());
    };
    let accepted = declarations
        .iter()
        .filter(|declaration| declaration.source() == &input.source().identity)
        .map(|declaration| declaration.symbol().clone())
        .collect::<std::collections::BTreeSet<_>>();
    let path = super::references::declaration_path(&input.source().identity)?;
    let starts = line_starts(input.text());
    let mut ordinals = BTreeMap::new();
    for symbol in syntax.symbols() {
        let identity = rift_core::symbol_identity(
            &syntax.language().identity_segment(),
            path.as_str(),
            &symbol.qualified_name,
        );
        let symbol_identity = rift_protocol::read::SymbolId(identity);
        if !accepted.contains(&symbol_identity) {
            continue;
        }
        for range in &symbol.documentation_ranges {
            let draft = BlockDraft {
                range: TextRange {
                    start: range.start,
                    end: range.end,
                },
                line: line_of(&starts, range.start),
                kind: DocumentationBlockKind::Prose,
                structure: "attached_comment",
                headings: Vec::new(),
                language: None,
                symbol: Some(symbol_identity.clone()),
            };
            append_block(input, output, draft, &mut ordinals)?;
        }
    }
    Ok(())
}

fn extract_rst(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    let path = source_file_path(input.source())?;
    let facts = super::rst::extract_rst_facts(input.text(), &path)?;
    let mut ordinals = BTreeMap::new();
    let mut blocks = BTreeMap::new();
    for fact in facts.blocks {
        let kind = match fact.kind {
            super::rst::RstBlockKind::Prose => DocumentationBlockKind::Prose,
            super::rst::RstBlockKind::Code => DocumentationBlockKind::Code,
        };
        let key = (fact.range.start, fact.range.end);
        let headings = fact
            .heading_path
            .into_iter()
            .map(|heading| DocumentationHeading {
                level: heading.level,
                name: heading.name,
            })
            .collect();
        let draft = BlockDraft {
            range: fact.range,
            line: fact.line,
            kind,
            structure: fact.structure,
            headings,
            language: fact.language,
            symbol: None,
        };
        blocks.insert(key, append_block(input, output, draft, &mut ordinals)?);
    }
    append_rst_targets_and_links(input, output, &blocks, &facts.targets, facts.links)?;
    warn(
        input,
        output,
        DocumentationWarningKind::MalformedSource,
        facts.error_ranges.len() as u64,
    );
    warn(
        input,
        output,
        DocumentationWarningKind::OmittedRange,
        facts.omitted_ranges.len() as u64,
    );
    Ok(())
}

fn append_rst_targets_and_links(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    blocks: &BTreeMap<(u64, u64), DocumentationDigest>,
    target_facts: &[super::rst::RstTargetFact],
    links: Vec<super::rst::RstLinkFact>,
) -> Result<(), DocumentationError> {
    let mut targets = BTreeMap::<String, RstReferenceTarget>::new();
    for target in target_facts {
        if target.name.is_empty() {
            continue;
        }
        let value = target.destination.as_ref().map_or_else(
            || RstReferenceTarget::Fragment(target.name.clone()),
            |destination| RstReferenceTarget::Destination(destination.clone()),
        );
        let key = rst_reference_name(&target.name);
        targets
            .entry(key)
            .and_modify(|value| *value = RstReferenceTarget::Ambiguous)
            .or_insert(value);
        if target.destination.is_none() {
            if output.fragments.len() >= DOCUMENTATION_REFERENCES_MAX as usize {
                return Err(refused(DocumentationViolation::LimitExceeded, "fragments"));
            }
            output.fragments.push(super::DocumentationFragment {
                source: input.source().identity.clone(),
                name: target.name.clone(),
                range: target.range.clone(),
            });
        }
    }
    for link in links {
        let Some(block) = blocks.get(&(link.block_range.start, link.block_range.end)) else {
            continue;
        };
        if output.links.len() + output.unresolved_links.len()
            >= DOCUMENTATION_REFERENCES_MAX as usize
        {
            return Err(refused(DocumentationViolation::LimitExceeded, "links"));
        }
        let (authored, resolution) = match link.kind {
            super::rst::RstLinkKind::Destination => (link.authored, None),
            super::rst::RstLinkKind::Reference => match targets
                .get(&rst_reference_name(&link.authored))
            {
                Some(RstReferenceTarget::Destination(destination)) => (destination.clone(), None),
                Some(RstReferenceTarget::Fragment(name)) => (format!("#{name}"), None),
                Some(RstReferenceTarget::Ambiguous) => (
                    link.authored,
                    Some(DocumentationLinkResolution::Unresolved {
                        reason: DocumentationUnresolvedReason::Ambiguous,
                    }),
                ),
                None => (
                    link.authored,
                    Some(DocumentationLinkResolution::Unresolved {
                        reason: DocumentationUnresolvedReason::Missing,
                    }),
                ),
            },
        };
        let is_unresolved = resolution.is_some();
        let record = DocumentationLink {
            block: block.clone(),
            authored,
            range: link.range,
            resolution: resolution.unwrap_or(DocumentationLinkResolution::Unresolved {
                reason: DocumentationUnresolvedReason::Missing,
            }),
        };
        if is_unresolved {
            output.unresolved_links.push(record);
        } else {
            output.links.push(record);
        }
    }
    Ok(())
}

enum RstReferenceTarget {
    Destination(String),
    Fragment(String),
    Ambiguous,
}

fn rst_reference_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn extract_cell(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    match input.source().identity.cell.as_ref().map(|cell| cell.kind) {
        Some(NotebookCellKind::Markdown) => extract_markdown(input, output),
        Some(NotebookCellKind::Code) => {
            if input.text().is_empty() {
                return Ok(());
            }
            let language = input
                .source()
                .language
                .as_ref()
                .map(|language| language.name.clone());
            let draft = BlockDraft {
                range: TextRange {
                    start: 0,
                    end: input.source().byte_length,
                },
                line: 1,
                kind: DocumentationBlockKind::Code,
                structure: "code",
                headings: Vec::new(),
                language,
                symbol: None,
            };
            append_block(input, output, draft, &mut BTreeMap::new()).map(|_| ())
        }
        None => Err(refused(DocumentationViolation::Notebook, "cell")),
    }
}

fn markdown_document<'a>(
    input: &'a DocumentationInput<'_>,
) -> Result<Cow<'a, SyntaxDocument>, DocumentationError> {
    if let Some(document) = input.syntax() {
        if document.markdown_facts().is_none() {
            return Err(refused(DocumentationViolation::Format, "syntax"));
        }
        return Ok(Cow::Borrowed(document));
    }
    let path = source_file_path(input.source())?;
    MarkdownSyntaxProvider::default()
        .analyze(
            SyntaxSource {
                path: &path,
                text: input.text(),
            },
            SyntaxLimits::default(),
        )
        .map(Cow::Owned)
        .map_err(|error| {
            rift_core::Error::new(
                super::DocumentationFault::new(DocumentationViolation::Format, "markdown")
                    .caused_by(error),
            )
        })
}

fn extract_markdown(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    let syntax = markdown_document(input)?;
    let facts = syntax
        .markdown_facts()
        .ok_or_else(|| refused(DocumentationViolation::Format, "syntax"))?;
    let filtered;
    let facts = if input.source().format == DocumentationSourceFormat::Mdx {
        filtered = facts.for_mdx(input.text());
        &filtered
    } else {
        facts
    };
    let mut ordinals = BTreeMap::new();
    let mut block_ranges = BTreeMap::new();
    for fact in facts.blocks() {
        let headings = facts
            .heading_path(fact.heading)
            .into_iter()
            .map(|heading| {
                syntax
                    .symbols()
                    .get(heading.symbol_index)
                    .map(|symbol| DocumentationHeading {
                        level: u32::from(heading.level),
                        name: symbol.qualified_name.clone(),
                    })
                    .ok_or_else(|| refused(DocumentationViolation::Range, "heading"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let kind = match fact.kind {
            MarkdownBlockKind::Prose => DocumentationBlockKind::Prose,
            MarkdownBlockKind::Code => DocumentationBlockKind::Code,
        };
        let draft = BlockDraft {
            range: wire_range(fact.range),
            line: fact.line,
            kind,
            structure: structure_name(fact.structure),
            headings,
            language: fact.code_language.clone(),
            symbol: None,
        };
        let identity = append_block(input, output, draft, &mut ordinals)?;
        block_ranges.insert((fact.range.start, fact.range.end), identity);
    }
    append_markdown_references(input, output, facts, &block_ranges)?;
    warn(
        input,
        output,
        DocumentationWarningKind::MalformedSource,
        facts.error_ranges().len() as u64,
    );
    warn(
        input,
        output,
        DocumentationWarningKind::OmittedRange,
        facts.omitted_ranges().len() as u64,
    );
    Ok(())
}

fn structure_name(structure: rift_syntax::MarkdownBlockStructure) -> &'static str {
    use rift_syntax::MarkdownBlockStructure as Structure;
    match structure {
        Structure::Heading => "heading",
        Structure::Paragraph => "paragraph",
        Structure::List => "list",
        Structure::Table => "table",
        Structure::BlockQuote => "block_quote",
        Structure::Code => "code",
        Structure::LinkDefinition => "link_definition",
    }
}

fn append_markdown_references(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    facts: &rift_syntax::MarkdownFacts,
    blocks: &BTreeMap<(u64, u64), DocumentationDigest>,
) -> Result<(), DocumentationError> {
    for candidate in facts.reference_candidates() {
        let Some(block) = blocks.get(&(candidate.block_range.start, candidate.block_range.end))
        else {
            continue;
        };
        if output.candidates.len() >= DOCUMENTATION_REFERENCES_MAX as usize {
            return Err(refused(DocumentationViolation::LimitExceeded, "references"));
        }
        output.candidates.push(DocumentationReferenceCandidate {
            block: block.clone(),
            range: wire_range(candidate.range),
            authored: slice(input.text(), &wire_range(candidate.range))?.to_owned(),
            language: None,
            reason: DocumentationUnresolvedReason::Missing,
        });
    }
    let definitions = markdown_definitions(input.text(), facts)?;
    for link in facts.links() {
        append_markdown_link(input, output, link, blocks, &definitions)?;
    }
    Ok(())
}

fn append_markdown_link(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    link: &rift_syntax::MarkdownLinkFact,
    blocks: &BTreeMap<(u64, u64), DocumentationDigest>,
    definitions: &BTreeMap<String, String>,
) -> Result<(), DocumentationError> {
    let Some(block) = blocks.get(&(link.block_range.start, link.block_range.end)) else {
        return Ok(());
    };
    let (authored, resolvable) = markdown_destination(input.text(), link, definitions)?;
    let Some(authored) = authored else {
        return Ok(());
    };
    if output.links.len() + output.unresolved_links.len() >= DOCUMENTATION_REFERENCES_MAX as usize {
        return Err(refused(DocumentationViolation::LimitExceeded, "links"));
    }
    let links = if resolvable {
        &mut output.links
    } else {
        &mut output.unresolved_links
    };
    links.push(DocumentationLink {
        block: block.clone(),
        authored,
        range: wire_range(link.range),
        resolution: DocumentationLinkResolution::Unresolved {
            reason: DocumentationUnresolvedReason::Missing,
        },
    });
    Ok(())
}

fn markdown_definitions(
    text: &str,
    facts: &rift_syntax::MarkdownFacts,
) -> Result<BTreeMap<String, String>, DocumentationError> {
    let mut definitions = BTreeMap::new();
    for link in facts
        .links()
        .iter()
        .filter(|link| link.kind == rift_syntax::MarkdownLinkKind::ReferenceDefinition)
    {
        let (Some(label), Some(destination)) = (link.label_range, link.destination_range) else {
            continue;
        };
        let label = reference_label(slice(text, &wire_range(label))?);
        let destination = slice(text, &wire_range(destination))?.to_owned();
        definitions.entry(label).or_insert(destination);
    }
    Ok(definitions)
}

fn reference_label(label: &str) -> String {
    label
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn markdown_destination(
    text: &str,
    link: &rift_syntax::MarkdownLinkFact,
    definitions: &BTreeMap<String, String>,
) -> Result<(Option<String>, bool), DocumentationError> {
    if let Some(destination) = link.destination_range {
        return Ok((
            Some(slice(text, &wire_range(destination))?.to_owned()),
            true,
        ));
    }
    let Some(label) = link.label_range else {
        return Ok((None, false));
    };
    let authored = slice(text, &wire_range(label))?;
    match definitions.get(&reference_label(authored)) {
        Some(destination) => Ok((Some(destination.clone()), true)),
        None => Ok((Some(authored.to_owned()), false)),
    }
}

struct BlockDraft {
    range: TextRange,
    line: u64,
    kind: DocumentationBlockKind,
    structure: &'static str,
    headings: Vec<DocumentationHeading>,
    language: Option<String>,
    symbol: Option<rift_protocol::read::SymbolId>,
}

fn append_block(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    draft: BlockDraft,
    ordinals: &mut BTreeMap<String, u32>,
) -> Result<DocumentationDigest, DocumentationError> {
    let exact = slice(input.text(), &draft.range)?;
    if output.blocks.len() >= DOCUMENTATION_BLOCKS_MAX as usize {
        return Err(refused(DocumentationViolation::LimitExceeded, "blocks"));
    }
    let key = canonical_digest(&(&draft.headings, draft.structure, draft.kind))?.0;
    let ordinal = ordinals.entry(key).or_default();
    *ordinal += 1;
    let identity = canonical_digest(&(
        &input.source().identity,
        &draft.headings,
        draft.structure,
        draft.kind,
        *ordinal,
    ))?;
    let digest = content_digest(exact.as_bytes());
    let start = input
        .chunks()
        .partition_point(|chunk| chunk.range.end <= draft.range.start);
    let chunks = input.chunks()[start..]
        .iter()
        .take_while(|chunk| chunk.range.start < draft.range.end)
        .cloned()
        .collect();
    output.blocks.push(DocumentationBlock {
        identity: identity.clone(),
        source: input.source().identity.clone(),
        content_digest: digest,
        heading_path: draft.headings,
        range: draft.range,
        line: draft.line,
        kind: draft.kind,
        language: draft.language,
        chunks,
        symbol: draft.symbol,
    });
    Ok(identity)
}

fn extract_text(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
) -> Result<(), DocumentationError> {
    let starts = line_starts(input.text());
    let mut range_start = None;
    let mut offset = 0_u64;
    let mut ordinals = BTreeMap::new();
    for line in lines_inclusive(input.text()) {
        if line.trim().is_empty() {
            append_paragraph(
                input,
                output,
                range_start.take(),
                offset,
                &starts,
                &mut ordinals,
            )?;
        } else {
            range_start.get_or_insert(offset);
        }
        offset += line.len() as u64;
    }
    append_paragraph(input, output, range_start, offset, &starts, &mut ordinals)
}

fn append_paragraph(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    start: Option<u64>,
    end: u64,
    starts: &[usize],
    ordinals: &mut BTreeMap<String, u32>,
) -> Result<(), DocumentationError> {
    let Some(start) = start else {
        return Ok(());
    };
    let draft = BlockDraft {
        range: TextRange { start, end },
        line: line_of(starts, start),
        kind: DocumentationBlockKind::Prose,
        structure: "paragraph",
        headings: Vec::new(),
        language: None,
        symbol: None,
    };
    append_block(input, output, draft, ordinals).map(|_| ())
}

fn warn(
    input: &DocumentationInput<'_>,
    output: &mut Collected,
    kind: DocumentationWarningKind,
    count: u64,
) {
    if count == 0 {
        return;
    }
    if let Some(existing) = output.warnings.iter_mut().find(|warning| {
        warning.source == input.source().identity
            && warning.stage == DocumentationStage::Extract
            && warning.kind == kind
    }) {
        existing.count = existing.count.saturating_add(count);
        return;
    }
    if output.warnings.len() >= DOCUMENTATION_WARNINGS_MAX as usize {
        return;
    }
    output.warnings.push(DocumentationWarning {
        source: input.source().identity.clone(),
        stage: DocumentationStage::Extract,
        kind,
        count,
    });
}

fn wire_range(range: ByteRange) -> TextRange {
    TextRange {
        start: range.start,
        end: range.end,
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::documentation::{
        DocumentationBlockKind, DocumentationContentIdentity, DocumentationSelectionReason,
        DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
    };
    use rift_protocol::read::{
        ProjectPath, SourceKind, SourceLocationKind, SymbolOrigin, TextRange,
    };

    use super::{BlockDraft, Collected, append_block};
    use crate::documentation::{DocumentationInput, DocumentationViolation, content_digest};

    #[test]
    fn invalid_block_range_remains_a_refusal() {
        let text = "abc\n";
        let source = DocumentationSource {
            identity: DocumentationContentIdentity {
                source: DocumentationSourceIdentity::Project {
                    path: ProjectPath("README.md".to_owned()),
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
            format: DocumentationSourceFormat::Markdown,
            media_type: "text/markdown".to_owned(),
            selection: DocumentationSelectionReason::Workspace,
            byte_length: text.len() as u64,
            language: None,
            physical_ranges: Vec::new(),
            license: None,
        };
        let input = DocumentationInput::new(source, text).expect("source");
        let error = append_block(
            &input,
            &mut Collected::default(),
            BlockDraft {
                range: TextRange { start: 0, end: 99 },
                line: 1,
                kind: DocumentationBlockKind::Prose,
                structure: "paragraph",
                headings: Vec::new(),
                language: None,
                symbol: None,
            },
            &mut std::collections::BTreeMap::new(),
        )
        .expect_err("invalid range");
        assert_eq!(error.fault().violation(), DocumentationViolation::Range);
    }
}
