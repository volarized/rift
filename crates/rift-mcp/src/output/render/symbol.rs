//! Text of one `get_symbol` hit.

use rift_protocol::documentation::{
    DocumentationContext, DocumentationHit, DocumentationReferenceHit,
};
use rift_protocol::mcp_symbol::{FoundSymbol, GetSymbolResult, SymbolDeclaration};
use rift_protocol::read::{
    CoChangeTruncation, GetSymbolHit, HistoryCommit, HistoryEdge, ProjectPath, SymbolHistory,
};
use rift_protocol::revision::{Revision, RevisionPoint};

use super::facts::{
    Facts, author_of, block_of, cut_hash, date_of, describe, location, spaced_name,
};
use super::layout::{self, Lines};
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

/// Writes each documentation reference with its excerpt.
fn context_section(lines: &mut Lines<'_>, context: &DocumentationContext) -> Result<(), TextError> {
    let DocumentationContext {
        documentation_revision: _,
        references,
        truncated,
        ..
    } = context;
    if references.is_empty() && !truncated {
        return Ok(());
    }
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
    facts.push_value(&block.place);
    facts.push_value(&block.headings);
    lines.line(ENTRY_INDENT, facts.as_str())?;
    match excerpt.as_deref() {
        Some(excerpt) if !excerpt.is_empty() => lines.verbatim(DETAIL_INDENT, excerpt),
        Some(_) | None => Ok(()),
    }
}

/// Writes the history: one entry per edge, commit edges newest first, then co-changes, or
/// release edges newest release first.
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
/// commit summary below; a co-change reads the other symbol and the shared commit count; a
/// release edge reads the release version, edge, and the change.
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
        HistoryEdge::IntroducedInRelease { revision }
        | HistoryEdge::RemovedInRelease { revision } => {
            return release_entry(lines, edge, revision, None);
        }
        HistoryEdge::ChangedInRelease { revision, change } => {
            return release_entry(lines, edge, revision, Some(&spaced_name(change)?));
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

/// Writes one release edge: the release version, the edge, and the change it names.
fn release_entry(
    lines: &mut Lines<'_>,
    edge: &HistoryEdge,
    revision: &Revision,
    change: Option<&str>,
) -> Result<(), TextError> {
    let mut facts = Facts::default();
    facts.push_value(revision.release_version().unwrap_or_default());
    facts.push(&edge_name(edge)?);
    if let Some(change) = change {
        facts.push(change);
    }
    lines.line(ENTRY_INDENT, facts.as_str())
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
    result: &GetSymbolResult,
) -> Result<(), TextError> {
    match result {
        GetSymbolResult::Found(found) => found_symbol(out, found),
        GetSymbolResult::Missing { .. } => Ok(()),
        GetSymbolResult::Unavailable {
            id,
            revision,
            reason,
            ..
        } => {
            let mut lines = Lines::new(out, 0, 1);
            lines.head(&super::unavailable::line(*reason))?;
            lines.line(0, id.as_str())?;
            layout::selected_revision(out, revision.as_ref())
        }
    }
}

fn found_symbol(
    out: &mut crate::output::text::TextWriter,
    found: &FoundSymbol,
) -> Result<(), TextError> {
    let id = found
        .symbol
        .id
        .as_ref()
        .ok_or(TextError::Unsupported("exact symbol without an identity"))?;
    out.raw_line(0, id.as_str())?;
    let total = found.pagination.total;
    let offset = found.pagination.offset();
    for (index, declaration) in found.declarations.iter().enumerate() {
        let ordinal = offset.saturating_add(index).saturating_add(1);
        out.raw_line(0, &format!("facets {ordinal}/{total}:"))?;
        declaration_section(&mut Lines::new(out, 0, 1), found, declaration)?;
    }

    if let Some(edges) = &found.edges {
        out.raw_line(0, "edges:")?;
        let mut previous = None;
        for edge in edges {
            let kind = edge.kind.0.as_str();
            if previous != Some(kind) {
                layout::entry(out, 0, &format!("{kind}:"))?;
                previous = Some(kind);
            }
            layout::entry(out, 1, edge.target.as_str())?;
        }
    }
    let mut lines = Lines::new(out, 0, 1);
    if total == 0 {
        for block in &found.symbol.documentation {
            text_section(&mut lines, &block.text)?;
        }
    }
    if let Some(context) = &found.documentation {
        context_section(&mut lines, context)?;
    }
    if let Some(history) = &found.history {
        history_section(
            &mut lines,
            history,
            found
                .declarations
                .first()
                .and_then(|declaration| declaration.binding.path.as_ref()),
        )?;
    }
    if let Some(page) = layout::page_fact(&found.pagination) {
        lines.line(0, &page)?;
    }
    layout::selected_revision(out, Some(&found.revision))
}

fn declaration_section(
    lines: &mut Lines<'_>,
    found: &FoundSymbol,
    declaration: &SymbolDeclaration,
) -> Result<(), TextError> {
    let binding = &declaration.binding;
    let mut facts = Facts::default();
    facts.push(&location(
        binding.path.as_ref(),
        binding.unit.as_ref(),
        Some(binding.line),
    ));
    facts.push(&format!(
        "bytes {}..{}",
        binding.range.start, binding.range.end
    ));
    lines.line(0, facts.as_str())?;
    if let Some(source) = &declaration.source {
        lines.line(0, "source:")?;
        lines.verbatim(1, &source.text)?;
    }
    for (index, documentation) in binding
        .documentation_indices
        .iter()
        .filter_map(|index| usize::try_from(*index).ok())
        .filter_map(|index| found.symbol.documentation.get(index))
        .filter(|documentation| !documentation.text.trim().is_empty())
        .enumerate()
    {
        if index == 0 {
            lines.line(0, "doc:")?;
        }
        lines.verbatim(1, &documentation.text)?;
    }
    Ok(())
}
