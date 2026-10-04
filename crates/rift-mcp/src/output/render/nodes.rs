//! Text of a `nodes` answer: one item per node, the innermost source last, then the warnings.

use rift_protocol::read::{Node, NodeFacet, NodeRegion, NodesResult};

use super::facts::{Facts, wire_name};
use super::layout::{self, Lines};
use super::warning;
use crate::output::text::{TextError, TextWriter};

/// The failure of an answer whose `nodes` and `source` differ in length.
const NODE_EXCERPT_MISSING: TextError = TextError::Unsupported("nodes and source differ in length");
/// Noun counted by the title of a `nodes` answer.
const NODE_NOUN: &str = "node";

/// Writes the nodes section, the innermost excerpt, and the warnings section.
///
/// Each outer excerpt holds the inner ones, so only the innermost excerpt is written: it carries
/// the source of all of them.
pub(super) fn answer(out: &mut TextWriter, result: &NodesResult) -> Result<(), TextError> {
    let NodesResult {
        nodes,
        source,
        warnings,
    } = result;
    if nodes.len() != source.len() {
        return Err(NODE_EXCERPT_MISSING);
    }
    layout::title(out, nodes.len(), NODE_NOUN, None)?;
    let pairs: Vec<(&Node, &String)> = nodes.iter().zip(source).collect();
    layout::items(out, &pairs, |lines, (node, excerpt)| {
        entry(lines, node, excerpt)
    })?;
    if let Some(innermost) = source.last() {
        let mut lines = Lines::after_last(out, nodes.len());
        lines.blank()?;
        lines.verbatim(0, innermost)?;
    }
    warning::section(out, warnings)
}

/// Writes one node: kind and facts, identity, symbol, and regions.
fn entry(lines: &mut Lines<'_>, node: &Node, excerpt: &str) -> Result<(), TextError> {
    let Node {
        id,
        symbol,
        unit: _,
        language: _,
        kind,
        facets,
        range: _,
        regions,
        parent: _,
        extensions: _,
    } = node;
    let mut facts = Facts::default();
    facts.push(&kind.0);
    let count = excerpt.lines().count();
    if count > 1 {
        facts.push(&format!("{count} lines"));
    }
    if facets.contains(&NodeFacet::Generated) {
        facts.push("generated");
    }
    if facets.contains(&NodeFacet::Test) {
        facts.push("test");
    }
    lines.head(facts.as_str())?;
    lines.line(0, &id.0)?;
    if let Some(symbol) = symbol {
        lines.line(0, &symbol.0)?;
    }
    lines.line(0, &regions_fact(regions)?)
}

/// The regions as `role START..END`, joined like facts. Empty without regions.
fn regions_fact(regions: &[NodeRegion]) -> Result<String, TextError> {
    let mut facts = Facts::default();
    for NodeRegion { role, range } in regions {
        facts.push(&format!(
            "{} {}..{}",
            wire_name(role)?,
            range.start,
            range.end
        ));
    }
    Ok(facts.as_str().to_owned())
}
