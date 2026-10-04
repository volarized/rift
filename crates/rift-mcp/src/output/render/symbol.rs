//! Text of one `get_symbol` hit.

use rift_protocol::documentation::{
    DocumentationContext, DocumentationHit, DocumentationReferenceHit,
};
use rift_protocol::read::{GetSymbolHit, ProjectPath, SymbolHistory, SymbolVersion};

use super::facts::{
    Facts, author_of, block_of, cut_hash, date_of, describe, location, spaced_name,
};
use super::layout::Lines;
use crate::output::text::TextError;

/// Levels that indent an entry under its section label.
const ENTRY_INDENT: usize = 1;
/// Levels that indent the lines under an entry.
const DETAIL_INDENT: usize = 2;

/// Writes one `get_symbol` hit: the declaration, its place, then each requested section.
pub(super) fn hit(lines: &mut Lines<'_>, hit: &GetSymbolHit) -> Result<(), TextError> {
    let GetSymbolHit {
        symbol,
        path,
        unit,
        range,
        line,
        node: _,
        source,
        history,
        documentation,
    } = hit;
    let described = describe(symbol)?;
    lines.head(&described.declaration)?;
    lines.line(0, &location(path.as_ref(), unit.as_ref(), Some(*line)))?;
    match described.id {
        Some(id) => lines.line(0, &id.0)?,
        None => lines.line(0, &format!("{}..{}", range.start, range.end))?,
    }
    lines.line(0, described.facts.as_str())?;
    for documentation_text in described.documentation {
        text_section(lines, &documentation_text.text)?;
    }
    if let Some(context) = documentation {
        context_section(lines, context)?;
    }
    if let Some(history) = history {
        history_section(lines, history, path.as_ref())?;
    }
    match source {
        Some(source) => {
            lines.blank()?;
            lines.verbatim(0, source)
        }
        None => Ok(()),
    }
}

/// Writes one documentation text after a blank line. Whitespace-only text writes nothing.
fn text_section(lines: &mut Lines<'_>, text: &str) -> Result<(), TextError> {
    if text.trim().is_empty() {
        return Ok(());
    }
    lines.blank()?;
    lines.verbatim(0, text)
}

/// Writes the documentation context: one entry per reference, with its excerpt.
fn context_section(lines: &mut Lines<'_>, context: &DocumentationContext) -> Result<(), TextError> {
    let DocumentationContext {
        documentation_revision: _,
        references,
        truncated,
        warnings: _,
    } = context;
    lines.blank()?;
    lines.line(
        0,
        if *truncated {
            "documentation (truncated):"
        } else {
            "documentation:"
        },
    )?;
    for reference in references {
        reference_entry(lines, reference)?;
    }
    Ok(())
}

/// Writes the place and headings of a referenced block, and its excerpt when it has one.
fn reference_entry(
    lines: &mut Lines<'_>,
    reference: &DocumentationReferenceHit,
) -> Result<(), TextError> {
    let DocumentationReferenceHit {
        reference: _,
        documentation,
        excerpt,
    } = reference;
    let DocumentationHit {
        block,
        source: _,
        documentation_revision: _,
    } = documentation;
    let block = block_of(block);
    let mut facts = Facts::default();
    facts.push(&block.place);
    facts.push(&block.headings);
    lines.line(ENTRY_INDENT, facts.as_str())?;
    match excerpt.as_deref() {
        Some(excerpt) if !excerpt.is_empty() => lines.verbatim(DETAIL_INDENT, excerpt),
        Some(_) | None => Ok(()),
    }
}

/// Writes the timeline: one entry per version, newest first.
fn history_section(
    lines: &mut Lines<'_>,
    history: &SymbolHistory,
    own_path: Option<&ProjectPath>,
) -> Result<(), TextError> {
    let SymbolHistory {
        symbol: _,
        versions,
        complete,
    } = history;
    lines.blank()?;
    lines.line(
        0,
        if *complete {
            "history:"
        } else {
            "history (incomplete):"
        },
    )?;
    for version in versions {
        version_entry(lines, version, own_path)?;
    }
    Ok(())
}

/// Writes one version: date, revision, kind, author, and its path when the symbol moved.
fn version_entry(
    lines: &mut Lines<'_>,
    version: &SymbolVersion,
    own_path: Option<&ProjectPath>,
) -> Result<(), TextError> {
    let SymbolVersion {
        revision,
        path,
        kind,
        timestamp,
        summary,
        author,
    } = version;
    let mut facts = Facts::default();
    facts.push(date_of(timestamp));
    facts.push(cut_hash(&revision.0));
    facts.push(&spaced_name(kind)?);
    facts.push(&author_of(author));
    if Some(path) != own_path {
        facts.push(&path.0);
    }
    lines.line(ENTRY_INDENT, facts.as_str())?;
    lines.line(DETAIL_INDENT, summary.as_deref().unwrap_or_default())
}
