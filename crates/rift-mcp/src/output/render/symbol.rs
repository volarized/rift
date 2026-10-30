//! Text of one `get_symbol` hit.

use rift_protocol::documentation::{
    DocumentationContext, DocumentationHit, DocumentationReferenceHit,
};
use rift_protocol::read::{
    CoChangeTruncation, GetSymbolHit, HistoryCommit, HistoryEdge, ProjectPath, ReadWarning,
    SymbolHistory,
};
use rift_protocol::revision::{Revision, RevisionPoint};

use super::facts::{
    Facts, author_of, block_of, cut_hash, date_of, describe, location, spaced_name,
};
use super::layout::{self, Lines};
use super::warning;
use crate::output::text::TextError;

/// Levels that indent an entry under its section label.
const ENTRY_INDENT: usize = 1;
/// Levels that indent the lines under an entry.
const DETAIL_INDENT: usize = 2;
/// The member a history edge's wire object names its edge kind under.
const EDGE_TAG: &str = "edge";

/// Writes one `get_symbol` hit: the declaration, its place, then each requested section.
pub(super) fn hit(lines: &mut Lines<'_>, hit: &GetSymbolHit) -> Result<(), TextError> {
    let GetSymbolHit {
        symbol,
        path,
        unit,
        range,
        line,
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
    Ok(())
}

/// Writes one documentation text after a blank line. Whitespace-only text writes nothing.
fn text_section(lines: &mut Lines<'_>, text: &str) -> Result<(), TextError> {
    if text.trim().is_empty() {
        return Ok(());
    }
    lines.blank()?;
    lines.verbatim(0, text)
}

/// Writes the documentation context: one entry per reference, with its excerpt, then one warning
/// line per warning of the context, written as the `documentation` read warning.
fn context_section(lines: &mut Lines<'_>, context: &DocumentationContext) -> Result<(), TextError> {
    let DocumentationContext {
        documentation_revision: _,
        references,
        truncated,
        warnings,
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
    for context_warning in warnings {
        let read_warning = ReadWarning::Documentation {
            warning: context_warning.clone(),
        };
        lines.line(ENTRY_INDENT, &warning::line(&read_warning)?)?;
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
    facts.push_value(&block.place);
    facts.push_value(&block.headings);
    lines.line(ENTRY_INDENT, facts.as_str())?;
    match excerpt.as_deref() {
        Some(excerpt) if !excerpt.is_empty() => lines.verbatim(DETAIL_INDENT, excerpt),
        Some(_) | None => Ok(()),
    }
}

/// Writes the history: one entry per edge, commit edges newest first, then co-changes.
fn history_section(
    lines: &mut Lines<'_>,
    history: &SymbolHistory,
    own_path: Option<&ProjectPath>,
) -> Result<(), TextError> {
    let SymbolHistory {
        symbol: _,
        edges,
        complete,
        co_changes_truncated,
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
    for edge in edges {
        edge_entry(lines, edge, own_path)?;
    }
    if let Some(CoChangeTruncation {
        commits_past_bound,
        symbols_past_bound,
    }) = co_changes_truncated
    {
        lines.line(
            ENTRY_INDENT,
            &format!(
                "co-changes truncated: {commits_past_bound} commits past bound, \
                 {symbols_past_bound} symbols past bound"
            ),
        )?;
    }
    Ok(())
}

/// Writes one edge. A commit edge reads date, commit, edge, the change or the path the
/// declaration came from, author, its path when the declaration lived elsewhere, and the
/// commit summary below; a co-change reads the other symbol and the shared commit count.
fn edge_entry(
    lines: &mut Lines<'_>,
    edge: &HistoryEdge,
    own_path: Option<&ProjectPath>,
) -> Result<(), TextError> {
    let mut facts = Facts::default();
    let mut detail = Facts::default();
    let commit = match edge {
        HistoryEdge::IntroducedIn { commit } | HistoryEdge::RemovedIn { commit } => commit,
        HistoryEdge::ChangedIn { commit, change } => {
            detail.push(&spaced_name(change)?);
            commit
        }
        HistoryEdge::RenamedFrom { commit, from } => {
            detail.push_value(&from.0);
            commit
        }
        HistoryEdge::CoChanges { symbol, commits } => {
            facts.push(&edge_name(edge)?);
            facts.push_value(symbol.as_str());
            facts.push(&format!("{commits} commits"));
            return lines.line(ENTRY_INDENT, facts.as_str());
        }
    };
    let HistoryCommit {
        revision,
        path,
        timestamp,
        summary,
        author,
    } = commit;
    facts.push_value(date_of(timestamp));
    facts.push_value(cut_hash(commit_spelling(revision)));
    facts.push(&edge_name(edge)?);
    facts.extend(&detail);
    facts.push_value(&author_of(author));
    if Some(path) != own_path {
        facts.push_value(&path.0);
    }
    lines.line(ENTRY_INDENT, facts.as_str())?;
    lines.line(DETAIL_INDENT, summary.as_deref().unwrap_or_default())
}

/// The wire tag of `edge` with underscores written as spaces, such as `introduced in`.
fn edge_name(edge: &HistoryEdge) -> Result<String, TextError> {
    serde_json::to_value(edge)
        .ok()
        .and_then(|value| {
            value
                .get(EDGE_TAG)?
                .as_str()
                .map(|name| name.replace('_', " "))
        })
        .ok_or(TextError::Unsupported("history edge without a wire tag"))
}

/// The commit id a history commit names; empty for a revision of another form, which a
/// history never answers.
fn commit_spelling(revision: &Revision) -> &str {
    match revision.point() {
        RevisionPoint::Commit(id) => id,
        RevisionPoint::Reference(_) | RevisionPoint::Release(..) | RevisionPoint::Tree(_) => "",
    }
}

pub(super) fn canonical(
    out: &mut crate::output::text::TextWriter,
    result: &rift_protocol::symbol_read::GetSymbolResult,
) -> Result<(), TextError> {
    use rift_protocol::symbol_read::GetSymbolResult;
    let (revision, warnings) = match result {
        GetSymbolResult::Found {
            symbol,
            revision,
            declarations,
            declaration_cursor,
            history,
            documentation,
            warnings,
        } => {
            let mut lines = Lines::new(out, 0, 1);
            let described = describe(symbol)?;
            lines.head(&described.declaration)?;
            if let Some(id) = described.id {
                lines.line(0, id.as_str())?;
            }
            lines.line(0, described.facts.as_str())?;
            for block in described.documentation {
                text_section(&mut lines, &block.text)?;
            }
            if let Some(context) = documentation {
                context_section(&mut lines, context)?;
            }
            if let Some(history) = history {
                history_section(
                    &mut lines,
                    history,
                    declarations
                        .first()
                        .and_then(|declaration| declaration.path.as_ref()),
                )?;
            }
            declarations_section(&mut lines, declarations)?;
            if let Some(cursor) = declaration_cursor {
                lines.line(0, &format!("declaration_cursor {cursor}"))?;
            }
            (Some(revision), warnings)
        }
        GetSymbolResult::Missing {
            symbol_not_found,
            revision,
            warnings,
        } => {
            out.raw_line(0, "error: symbol_not_found")?;
            let mut lines = Lines::new(out, 0, 1);
            lines.line(0, symbol_not_found.id.as_str())?;
            if !symbol_not_found.alternatives.is_empty() {
                lines.line(0, "closest matches:")?;
            }
            for alternative in &symbol_not_found.alternatives {
                lines.line(
                    1,
                    &format!("{} · {}", alternative.id.as_str(), alternative.name),
                )?;
                lines.line(2, &format!("{:?}", alternative.match_context))?;
            }
            if let Some(detail) = &symbol_not_found.detail {
                lines.verbatim(0, detail)?;
            }
            (Some(revision), warnings)
        }
        GetSymbolResult::Unavailable {
            id,
            revision,
            reason,
            warnings,
        } => {
            let mut lines = Lines::new(out, 0, 1);
            lines.head(&format!("unavailable · {reason:?}"))?;
            lines.line(0, id.as_str())?;
            (revision.as_ref(), warnings)
        }
    };
    layout::revision(out, revision)?;
    warning::section(out, warnings)
}

fn declarations_section(
    lines: &mut Lines<'_>,
    declarations: &[rift_protocol::symbol_read::SymbolDeclaration],
) -> Result<(), TextError> {
    if !declarations.is_empty() {
        lines.blank()?;
        lines.line(0, "declarations:")?;
    }
    for declaration in declarations {
        lines.line(
            1,
            &location(
                declaration.path.as_ref(),
                declaration.unit.as_ref(),
                Some(declaration.line),
            ),
        )?;
        lines.line(
            1,
            &format!("{}..{}", declaration.range.start, declaration.range.end),
        )?;
    }
    Ok(())
}
