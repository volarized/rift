//! Extracts range metadata from selected source owners and existing syntax facts.

use std::borrow::Cow;
use std::collections::BTreeMap;

use rift_core::line::{line_of, line_starts, lines_inclusive};
use rift_protocol::documentation::{
    DOCUMENTATION_BLOCKS_MAX, DOCUMENTATION_REFERENCES_MAX, DOCUMENTATION_WARNINGS_MAX,
    DocumentationBlock, DocumentationBlockKind, DocumentationCoverage, DocumentationDigest,
    DocumentationHeading, DocumentationIndex, DocumentationLink, DocumentationLinkResolution,
    DocumentationReferenceCandidate, DocumentationSourceFormat, DocumentationStage,
    DocumentationUnresolvedReason, DocumentationWarning, DocumentationWarningKind,
    NotebookCellKind,
};
use rift_protocol::read::TextRange;
use rift_syntax::{
    ByteRange, MarkdownBlockKind, MarkdownSyntaxProvider, SyntaxDocument, SyntaxProvider,
    SyntaxSource,
};

use super::failure::{DocumentationError, DocumentationViolation, refused};
use super::identity::{canonical_digest, content_digest};
use super::input::{slice, source_path};
use super::{
    DocumentationCollection, DocumentationDeclaration, DocumentationInput, DocumentationSourceSet,
    resolve_references,
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
    let mut output = Collected::default();
    for input in sources.sources() {
        let checkpoint = output.checkpoint();
        if let Err(error) = extract_source(input, declarations, &mut output) {
            let Some(kind) = recoverable_source_error(input, &error) else {
                return Err(error);
            };
            output.rollback(checkpoint);
            output.omitted = output.omitted.saturating_add(1);
            warn(input, &mut output, kind, 1);
        }
    }
    let records = sources
        .sources()
        .iter()
        .map(|input| input.source().clone())
        .collect::<Vec<_>>();
    super::resolve_links(
        &records,
        &output.blocks,
        &mut output.links,
        &output.fragments,
    )?;
    output.links.append(&mut output.unresolved_links);
    let (mut references, unresolved_references) =
        resolve_references(declarations, &output.candidates)?.into_parts();
    references.extend(super::links::resolve_declaration_links(
        &records,
        &output.blocks,
        &mut output.links,
        declarations,
    )?);
    references.sort_by(|left, right| {
        (
            &left.evidence,
            &left.block,
            left.range.start,
            &left.identity,
        )
            .cmp(&(
                &right.evidence,
                &right.block,
                right.range.start,
                &right.identity,
            ))
    });
    output.blocks.sort_by(|a, b| {
        (&a.source, a.range.start, a.kind, &a.identity).cmp(&(
            &b.source,
            b.range.start,
            b.kind,
            &b.identity,
        ))
    });
    let selected = u32::try_from(sources.sources().len())
        .map_err(|_| refused(DocumentationViolation::LimitExceeded, "sources"))?;
    DocumentationCollection::new(DocumentationIndex {
        documentation_revision: super::documentation_revision(),
        selection_digest: sources.selection_digest().clone(),
        sources: records,
        blocks: output.blocks,
        links: output.links,
        references,
        unresolved_references,
        coverage: DocumentationCoverage {
            selected,
            parsed: selected - output.omitted,
            omitted: output.omitted,
            truncated: 0,
        },
        warnings: output.warnings,
    })
}

#[derive(Default)]
struct Collected {
    blocks: Vec<DocumentationBlock>,
    links: Vec<DocumentationLink>,
    unresolved_links: Vec<DocumentationLink>,
    fragments: Vec<super::DocumentationFragment>,
    candidates: Vec<DocumentationReferenceCandidate>,
    warnings: Vec<DocumentationWarning>,
    omitted: u32,
}

#[derive(Clone, Copy)]
struct Checkpoint {
    blocks: usize,
    links: usize,
    unresolved_links: usize,
    fragments: usize,
    candidates: usize,
    warnings: usize,
    omitted: u32,
}

impl Collected {
    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            blocks: self.blocks.len(),
            links: self.links.len(),
            unresolved_links: self.unresolved_links.len(),
            fragments: self.fragments.len(),
            candidates: self.candidates.len(),
            warnings: self.warnings.len(),
            omitted: self.omitted,
        }
    }

    fn rollback(&mut self, checkpoint: Checkpoint) {
        self.blocks.truncate(checkpoint.blocks);
        self.links.truncate(checkpoint.links);
        self.unresolved_links.truncate(checkpoint.unresolved_links);
        self.fragments.truncate(checkpoint.fragments);
        self.candidates.truncate(checkpoint.candidates);
        self.warnings.truncate(checkpoint.warnings);
        self.omitted = checkpoint.omitted;
    }
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
    let path = rift_core::ProjectPath::new(super::references::declaration_path(
        &input.source().identity,
    )?)
    .map_err(|_| refused(DocumentationViolation::Identity, "source"))?;
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
    let path = rift_core::ProjectPath::new(source_path(&input.source().identity)?)
        .map_err(|_| refused(DocumentationViolation::Identity, "source"))?;
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
    let path = rift_core::ProjectPath::new(source_path(&input.source().identity)?)
        .map_err(|_| refused(DocumentationViolation::Identity, "source"))?;
    MarkdownSyntaxProvider::default()
        .analyze(SyntaxSource {
            path: &path,
            text: input.text(),
        })
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
