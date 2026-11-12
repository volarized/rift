//! Text of a `nodes` answer: one item per node, the written sources, the cursor, then the
//! warnings.

use rift_protocol::read::{Node, NodeFacet, NodeRegion, NodesResult};

use super::facts::{Facts, wire_name};
use super::layout::{self, Lines};
use super::warning;
use crate::output::text::{TextError, TextWriter};

/// Noun counted by the title of a `nodes` answer.
const NODE_NOUN: &str = "node";

/// Writes the title with the answer's symbol, the nodes section, the written sources, the
/// cursor, and the warnings section.
///
/// An outer node's source holds the sources of the nodes inside it, so the text writes each
/// source once: the source of each node no earlier node encloses, one per declaration of a
/// symbol, or the file node's for a unit.
pub(super) fn answer(out: &mut TextWriter, result: &NodesResult) -> Result<(), TextError> {
    let NodesResult {
        symbol,
        nodes,
        node_cursor,
        revision,
        view,
        warnings,
    } = result;
    layout::title(
        out,
        nodes.len(),
        NODE_NOUN,
        symbol.as_ref().map(|symbol| symbol.0.as_str()),
    )?;
    layout::items(out, nodes, entry)?;
    let mut lines = Lines::after_last(out, nodes.len());
    for node in written(nodes) {
        lines.blank()?;
        lines.verbatim(0, &node.source)?;
    }
    if let Some(cursor) = node_cursor {
        out.raw_line(0, &format!("node_cursor {cursor}"))?;
    }
    warning::section(out, warnings)?;
    layout::revision(out, Some(revision))?;
    if let Some(view) = view {
        out.raw_line(
            0,
            &format!("view {} · expires {}", view.id.as_str(), view.expires_at),
        )?;
    }
    Ok(())
}

/// The nodes whose source the text writes, in answer order: each node no earlier node
/// encloses.
fn written(nodes: &[Node]) -> Vec<&Node> {
    let mut outermost: Vec<&Node> = Vec::new();
    for node in nodes {
        if !outermost.iter().any(|outer| encloses(outer, node)) {
            outermost.push(node);
        }
    }
    outermost
}

/// Whether `outer` spans `inner` within the same unit.
fn encloses(outer: &Node, inner: &Node) -> bool {
    outer.unit == inner.unit
        && outer.range.start <= inner.range.start
        && inner.range.end <= outer.range.end
}

/// Writes one node: kind and facts, byte range, address, and regions.
fn entry(lines: &mut Lines<'_>, node: &Node) -> Result<(), TextError> {
    let Node {
        declaration,
        index,
        unit: _,
        language: _,
        kind,
        facets,
        range,
        source,
        regions,
        parent,
        extensions: _,
    } = node;
    let mut facts = Facts::default();
    facts.push_value(&kind.0);
    let count = source.lines().count();
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
    lines.line(0, &format!("{}..{}", range.start, range.end))?;
    lines.line(0, &address(*declaration, *index, *parent))?;
    lines.line(0, &regions_fact(regions)?)
}

/// The node's address as `declaration D · index I · parent P`, each part present when the
/// node carries it.
fn address(declaration: Option<u64>, index: u64, parent: Option<u64>) -> String {
    let mut facts = Facts::default();
    if let Some(declaration) = declaration {
        facts.push(&format!("declaration {declaration}"));
    }
    facts.push(&format!("index {index}"));
    if let Some(parent) = parent {
        facts.push(&format!("parent {parent}"));
    }
    facts.as_str().to_owned()
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
