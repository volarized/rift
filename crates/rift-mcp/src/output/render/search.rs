//! Text of the `search` hits, each laid out per target variant.
//!
//! Hits a relationship walk reached are written as a tree by the `walk` module.

use rift_protocol::documentation::DocumentationHit;
use rift_protocol::read::{NodeId, ProjectPath, SourceUnitId, Symbol, SymbolId, TextRange};
use rift_protocol::search::{
    CommitHit, GraphHop, MatchedField, SearchHit, SearchHitTarget, SymbolChange,
};

use super::facts::{
    Facts, author_of, block_of, cut_hash, describe, location, moment_of, spaced_name, wire_names,
};
use super::layout::{self, Lines, Page};
use super::walk;
use crate::output::text::{TextError, TextWriter};

/// Text shown after the message of a commit that was cut.
const MESSAGE_TRUNCATED: &str = "message truncated";
/// Heading of a documentation hit whose block has none.
const DOCUMENTATION_HEADING: &str = "documentation";
/// Levels that indent a path under `paths:`.
const PATH_INDENT: usize = 1;

/// The fields of a hit that do not depend on its target.
struct HitView<'a> {
    score: Option<f64>,
    matched_by: &'a [MatchedField],
    source: Option<&'a str>,
    range: Option<&'a TextRange>,
    line: Option<u64>,
    path: Option<&'a ProjectPath>,
    unit: Option<&'a SourceUnitId>,
    traversal_path: Option<&'a [GraphHop]>,
    change: Option<&'a SymbolChange>,
}

/// Splits a hit into its target and the fields every target shares.
fn split(hit: &SearchHit) -> (&SearchHitTarget, HitView<'_>) {
    let SearchHit {
        hit: target,
        score,
        matched_by,
        source,
        range,
        line,
        path,
        unit,
        traversal_path,
        distance: _,
        change,
    } = hit;
    let view = HitView {
        score: *score,
        matched_by,
        source: source.as_deref(),
        range: range.as_ref(),
        line: *line,
        path: path.as_ref(),
        unit: unit.as_ref(),
        traversal_path: traversal_path.as_deref(),
        change: change.as_ref(),
    };
    (target, view)
}

/// Writes a `search` answer: the hits outside a relationship walk as items, then the walk.
pub(super) fn answer(
    out: &mut TextWriter,
    page: &Page<'_>,
    hits: &[SearchHit],
) -> Result<(), TextError> {
    let walk = walk::Walk::of(hits)?;
    layout::answer_of(out, page, hits.len(), |out| {
        layout::items(out, walk.outside(), |lines, hit| self::hit(lines, hit))?;
        walk.write(out)
    })
}

/// The first hop and the further hops of the relationship path that reached `hit`, when its
/// target is a symbol and it carries hops.
pub(super) fn walked(hit: &SearchHit) -> Option<(&GraphHop, &[GraphHop])> {
    let (target, view) = split(hit);
    match target {
        SearchHitTarget::Symbol { symbol: _ } => view.traversal_path?.split_first(),
        _ => None,
    }
}

/// The identity of the symbol of `hit`, when its target is a symbol that has one.
pub(super) fn symbol_id(hit: &SearchHit) -> Option<&SymbolId> {
    match split(hit).0 {
        SearchHitTarget::Symbol { symbol } => symbol.id.as_ref(),
        _ => None,
    }
}

/// Writes one search hit outside a relationship walk.
fn hit(lines: &mut Lines<'_>, hit: &SearchHit) -> Result<(), TextError> {
    let (target, view) = split(hit);
    match target {
        SearchHitTarget::Symbol { symbol } => {
            symbol_hit(lines, symbol, &view, view.matched_by, walk::Labels::Item)
        }
        SearchHitTarget::File {
            size: _,
            languages: _,
        } => file_hit(lines, &view),
        SearchHitTarget::Node { node } => node_hit(lines, node, &view),
        SearchHitTarget::Documentation { documentation } => {
            documentation_hit(lines, documentation, &view)
        }
        SearchHitTarget::Commit { commit } => commit_hit(lines, commit, &view),
    }
}

/// The score of a hit, which is written only when the hit carries it.
fn rank_facts(view: &HitView<'_>, facts: &mut Facts) {
    if let Some(score) = view.score {
        facts.push(&format!("score {score}"));
    }
}

/// The place, the matched fields, the rank, and `tail` of a hit as one line.
fn place_facts(view: &HitView<'_>, place: &str, tail: &Facts) -> Result<Facts, TextError> {
    matched_facts(view, view.matched_by, place, tail)
}

/// The place, the `matched` fields, the rank, and `tail` of a hit as one line.
fn matched_facts(
    view: &HitView<'_>,
    matched: &[MatchedField],
    place: &str,
    tail: &Facts,
) -> Result<Facts, TextError> {
    let mut facts = Facts::default();
    facts.push(place);
    facts.push(&wire_names(matched)?);
    rank_facts(view, &mut facts);
    facts.extend(tail);
    Ok(facts)
}

/// Writes the node of a relationship walk that is `hit`: the lines of a symbol hit behind
/// `labels`, without the `relationship` field among its matched fields, which the tree states.
///
/// A hit whose target is not a symbol writes nothing.
pub(super) fn walk_node(
    lines: &mut Lines<'_>,
    hit: &SearchHit,
    labels: walk::Labels,
) -> Result<(), TextError> {
    let (target, view) = split(hit);
    let SearchHitTarget::Symbol { symbol } = target else {
        return Ok(());
    };
    let matched: Vec<MatchedField> = view
        .matched_by
        .iter()
        .copied()
        .filter(|field| *field != MatchedField::Relationship)
        .collect();
    symbol_hit(lines, symbol, &view, &matched, labels)
}

/// `text` behind `label`. Empty text stays empty, so its line is not written.
fn labeled(label: Option<&str>, text: &str) -> String {
    match label {
        Some(label) if !text.is_empty() => format!("{label} {text}"),
        Some(_) | None => text.to_owned(),
    }
}

/// Writes the symbol hit: declaration, facts with the `matched` fields, identity, summary,
/// change, source.
///
/// A node under the root of a relationship walk has its head line already and writes the
/// declaration below it. A node of a walk writes the facts and the identity behind the words
/// of `labels`.
fn symbol_hit(
    lines: &mut Lines<'_>,
    symbol: &Symbol,
    view: &HitView<'_>,
    matched: &[MatchedField],
    labels: walk::Labels,
) -> Result<(), TextError> {
    let described = describe(symbol)?;
    match labels {
        walk::Labels::Item | walk::Labels::Root => lines.head(&described.declaration)?,
        walk::Labels::Branch { by: _ } => {
            lines.line(0, &labeled(labels.declaration(), &described.declaration))?;
        }
    }
    let place = location(view.path, view.unit, view.line);
    let facts = matched_facts(view, matched, &place, &described.facts)?;
    let place_label = labels.place().filter(|_| !place.is_empty());
    lines.line(0, &labeled(place_label, facts.as_str()))?;
    match (described.id, view.range) {
        (Some(id), _) => lines.line(0, &labeled(labels.identity(), &id.0))?,
        (None, Some(range)) => lines.line(0, &format!("{}..{}", range.start, range.end))?,
        (None, None) => {}
    }
    lines.line(0, described.summary.unwrap_or_default())?;
    change_line(lines, view)?;
    match view.source {
        Some(source) => {
            lines.blank()?;
            lines.verbatim(0, source)
        }
        None => Ok(()),
    }
}

/// Writes the change line of a hit that a comparison produced: the kind with underscores as
/// spaces, then the path facts.
fn change_line(lines: &mut Lines<'_>, view: &HitView<'_>) -> Result<(), TextError> {
    let Some(change) = view.change else {
        return Ok(());
    };
    let SymbolChange {
        kind,
        base_path,
        head_path,
    } = change;
    let moved = match (base_path, head_path) {
        (Some(base), Some(head)) if base != head => Some(format!("{} → {}", base.0, head.0)),
        (Some(_), Some(_)) | (None, None) => None,
        (Some(only), None) | (None, Some(only)) => {
            (Some(only) != view.path).then(|| only.0.clone())
        }
    };
    let mut facts = Facts::default();
    facts.push(&spaced_name(kind)?);
    facts.push(&moved.unwrap_or_default());
    lines.line(0, facts.as_str())
}

/// Writes a file hit: its place and matched fields, then the source directly under it.
fn file_hit(lines: &mut Lines<'_>, view: &HitView<'_>) -> Result<(), TextError> {
    let place = location(view.path, view.unit, view.line);
    let facts = place_facts(view, &place, &Facts::default())?;
    lines.head(facts.as_str())?;
    plain_source(lines, view.source)
}

/// Writes a node hit: its place and matched fields, its identity, then the source.
fn node_hit(lines: &mut Lines<'_>, node: &NodeId, view: &HitView<'_>) -> Result<(), TextError> {
    let place = location(view.path, view.unit, view.line);
    let facts = place_facts(view, &place, &Facts::default())?;
    lines.head(facts.as_str())?;
    lines.line(0, &node.0)?;
    plain_source(lines, view.source)
}

/// Writes a documentation hit: its headings, its place, the symbol it documents, the source.
fn documentation_hit(
    lines: &mut Lines<'_>,
    documentation: &DocumentationHit,
    view: &HitView<'_>,
) -> Result<(), TextError> {
    let DocumentationHit {
        block,
        source: _,
        documentation_revision: _,
    } = documentation;
    let block = block_of(block);
    let heading = if block.headings.is_empty() {
        DOCUMENTATION_HEADING
    } else {
        &block.headings
    };
    lines.head(heading)?;
    let facts = place_facts(view, &block.place, &Facts::default())?;
    lines.line(0, facts.as_str())?;
    if let Some(symbol) = block.symbol {
        lines.line(0, &symbol.0)?;
    }
    plain_source(lines, view.source)
}

/// Writes the source directly under the lines above it.
fn plain_source(lines: &mut Lines<'_>, source: Option<&str>) -> Result<(), TextError> {
    match source {
        Some(source) => lines.verbatim(0, source),
        None => Ok(()),
    }
}

/// Writes a commit hit: revision, time, author, message, and the changed paths.
fn commit_hit(
    lines: &mut Lines<'_>,
    commit: &CommitHit,
    view: &HitView<'_>,
) -> Result<(), TextError> {
    let CommitHit {
        revision,
        message,
        message_truncated,
        author,
        timestamp,
        paths,
        paths_truncated,
    } = commit;
    let mut facts = Facts::default();
    facts.push(cut_hash(&revision.0));
    facts.push(&author_of(author));
    facts.push(&moment_of(timestamp));
    rank_facts(view, &mut facts);
    lines.head(facts.as_str())?;
    let (subject, body) = split_message(message);
    lines.line(0, subject)?;
    if !body.is_empty() {
        lines.blank()?;
        lines.verbatim(0, body)?;
    }
    if *message_truncated {
        lines.line(0, MESSAGE_TRUNCATED)?;
    }
    path_lines(lines, paths, *paths_truncated)
}

/// The first line of a commit message, and the lines after the blank line that follows it.
///
/// Trailing whitespace of the message is dropped.
fn split_message(message: &str) -> (&str, &str) {
    let (subject, rest) = message.split_once('\n').unwrap_or((message, ""));
    (subject, rest.trim_start_matches('\n').trim_end())
}

/// Writes the changed paths under a counted label. No paths write nothing.
///
/// A truncated list says so in its label and ends with a line of dots.
fn path_lines(
    lines: &mut Lines<'_>,
    paths: &[ProjectPath],
    truncated: bool,
) -> Result<(), TextError> {
    let count = paths.len();
    if count == 0 {
        return Ok(());
    }
    lines.blank()?;
    let label = match (truncated, count) {
        (true, _) => format!("{count}+ paths · truncated"),
        (false, 1) => "1 path:".to_owned(),
        (false, _) => format!("{count} paths:"),
    };
    lines.line(0, &label)?;
    for path in paths {
        lines.line(PATH_INDENT, &path.0)?;
    }
    if truncated {
        lines.line(PATH_INDENT, "...")?;
    }
    Ok(())
}
