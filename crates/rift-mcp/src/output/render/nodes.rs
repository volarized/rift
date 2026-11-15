//! Text of a `nodes` answer, including its source and continuation.

use rift_protocol::identity::SymbolIdentity;
use rift_protocol::nodes_read::{NodeDeclaration, NodePart, NodesSummary};
use rift_protocol::read::{
    FILE_NODE_KIND, Node, NodeFacet, NodeRegion, NodesResult, SourceExcerpt, SourceUnitId,
    SourceUnitSpan, TextRange,
};

use super::facts::wire_name;
use super::layout::{self, Lines};
use crate::output::text::{TextError, TextWriter, quoted, visible};

/// Most declarations in a summary.
const DECLARATIONS_MAX: usize = 4;
/// Most bytes shown per declaration or file excerpt.
const SOURCE_BYTES_MAX: usize = 1536;
/// Most child nodes shown per declaration.
const PARTS_MAX: usize = 8;
const SEPARATOR: &str = " | ";

/// Projects one page of declaration roots and returns the offset of any roots left on it.
pub(crate) fn select(
    result: NodesResult,
    position: Option<u64>,
    skip: usize,
) -> (NodesSummary, Option<usize>) {
    let NodesResult {
        symbol,
        nodes,
        node_cursor: _,
        revision,
        warnings,
    } = result;
    let mut roots = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.index == 0 && node.declaration.is_some())
        .skip(skip);
    let declarations: Vec<_> = roots
        .by_ref()
        .take(DECLARATIONS_MAX)
        .map(|(start, root)| {
            let parts = nodes[start + 1..]
                .iter()
                .take_while(|node| node.index != 0 && node.declaration == root.declaration)
                .filter(|node| node.parent == Some(root.index) && structural(node))
                .take(PARTS_MAX)
                .map(|node| NodePart {
                    index: node.index,
                    kind: node.kind.clone(),
                    range: node.range.clone(),
                })
                .collect();
            NodeDeclaration {
                declaration: root.declaration.unwrap_or_default(),
                kind: root.kind.clone(),
                range: root.range.clone(),
                source: source_excerpt(root, None),
                regions: root.regions.iter().take(PARTS_MAX).cloned().collect(),
                parts,
            }
        })
        .collect();
    let excerpt = nodes
        .first()
        .filter(|node| node.kind.0 == FILE_NODE_KIND)
        .map(|node| source_excerpt(node, position));
    let truncated = declarations
        .iter()
        .any(|declaration| declaration.source.span.range != declaration.range)
        .then_some(true);
    let remaining = roots.next().map(|_| skip + DECLARATIONS_MAX);
    (
        NodesSummary {
            symbol,
            declarations,
            excerpt,
            node_cursor: None,
            truncated,
            revision,
            warnings,
        },
        remaining,
    )
}

/// Recognizes structural children from the provider's facets and grammar names.
fn structural(node: &Node) -> bool {
    node.facets.iter().any(|facet| {
        matches!(
            facet,
            NodeFacet::Body
                | NodeFacet::Block
                | NodeFacet::Parameter
                | NodeFacet::Pattern
                | NodeFacet::Statement
                | NodeFacet::TypeExpression
        )
    }) || matches!(node.kind.0.as_str(), "parameters" | "block")
        || node.kind.0.ends_with("_list")
        || node.kind.0.ends_with("_parameters")
        || node.kind.0.ends_with("_type")
}

/// A declaration starts at its header. A file excerpt centers on the selected byte where it fits.
fn source_excerpt(node: &Node, position: Option<u64>) -> SourceExcerpt {
    let source = &node.source;
    let length = source.len();
    let center = position
        .map(|position| position.saturating_sub(node.range.start))
        .and_then(|position| usize::try_from(position).ok())
        .unwrap_or_default()
        .min(length);
    let start = source.ceil_char_boundary(
        center
            .saturating_sub(SOURCE_BYTES_MAX / 2)
            .min(length.saturating_sub(SOURCE_BYTES_MAX)),
    );
    let end = source.floor_char_boundary(start.saturating_add(SOURCE_BYTES_MAX));
    SourceExcerpt {
        span: SourceUnitSpan {
            unit: node.unit.clone(),
            range: TextRange {
                start: node.range.start + start as u64,
                end: node.range.start + end as u64,
            },
        },
        text: source[start..end].to_owned(),
    }
}

pub(super) fn summary(out: &mut TextWriter, result: &NodesSummary) -> Result<(), TextError> {
    if let Some(symbol) = &result.symbol {
        let identity = SymbolIdentity::parse(symbol.as_str())
            .map_err(|error| TextError::Custom(error.to_string()))?;
        if let Some(name) = identity.qualified_path().last() {
            out.raw_line(0, &visible(name))?;
        }
    }
    for declaration in &result.declarations {
        declaration_text(out, declaration)?;
    }
    if let Some(excerpt) = &result.excerpt {
        location_text(
            out,
            &excerpt.span.unit,
            &excerpt.span.range,
            "Source near the selected position",
        )?;
        out.raw_lines(1, &excerpt.text)?;
    }
    if result.declarations.is_empty() && result.excerpt.is_none() {
        out.raw_line(0, "No declaration source available.")?;
    }
    if let Some(cursor) = &result.node_cursor {
        out.raw_line(0, &format!("next declarations | node_cursor {cursor}"))?;
    }
    layout::selected_revision(out, Some(&result.revision))
}

fn declaration_text(out: &mut TextWriter, declaration: &NodeDeclaration) -> Result<(), TextError> {
    let NodeDeclaration {
        declaration,
        kind,
        range,
        source,
        regions,
        parts,
    } = declaration;
    let kind = visible(&kind.0);
    let kind = quoted(&kind, &[SEPARATOR]);
    location_text(
        out,
        &source.span.unit,
        range,
        &format!("Declaration {declaration} | {kind}"),
    )?;
    out.raw_lines(1, &source.text)?;
    if &source.span.range != range {
        out.raw_line(
            1,
            &format!(
                "Source shown | bytes {}..{} | syntax: true for exact source",
                source.span.range.start, source.span.range.end
            ),
        )?;
    }
    let mut facts = region_facts(regions)?;
    facts.extend(parts.iter().map(|part| {
        let name = visible(&part.kind.0).replace('_', " ");
        quoted(&name, &[SEPARATOR]).into_owned()
    }));
    if !facts.is_empty() {
        out.raw_line(1, &format!("Structure | {}", facts.join(SEPARATOR)))?;
    }
    Ok(())
}

fn location_text(
    out: &mut TextWriter,
    unit: &SourceUnitId,
    range: &TextRange,
    heading: &str,
) -> Result<(), TextError> {
    let path = unit.project_path();
    let path = path.as_deref().unwrap_or(unit.as_str());
    let path = visible(path);
    out.raw_line(
        0,
        &format!(
            "{heading} | {} | bytes {}..{}",
            quoted(&path, &[SEPARATOR]),
            range.start,
            range.end
        ),
    )
}

/// Noun counted by the title of a `nodes` answer.
const NODE_NOUN: &str = "node";

/// Writes the title, nodes, source, and continuation.
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
        ..
    } = result;
    let title = layout::counted(nodes.len(), NODE_NOUN);
    if let Some(symbol) = symbol {
        out.raw_line(0, &format!("{title} | {}", visible(symbol.as_str())))?;
    } else {
        out.raw_line(0, &title)?;
    }
    for node in nodes {
        entry(out, node)?;
    }
    let mut lines = Lines::after_last(out, nodes.len());
    for node in written(nodes) {
        lines.blank()?;
        lines.verbatim(0, &node.source)?;
    }
    if let Some(cursor) = node_cursor {
        out.raw_line(0, &format!("node_cursor {cursor}"))?;
    }
    layout::revision(out, Some(revision))
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

/// Writes a node's kind, byte range, address, and other facts on one line.
fn entry(out: &mut TextWriter, node: &Node) -> Result<(), TextError> {
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
    let kind = visible(&kind.0);
    let mut facts = vec![
        quoted(&kind, &[SEPARATOR]).into_owned(),
        format!("bytes {}..{}", range.start, range.end),
    ];
    if let Some(declaration) = declaration {
        facts.push(format!("declaration {declaration}"));
    }
    facts.push(format!("index {index}"));
    if let Some(parent) = parent {
        facts.push(format!("parent {parent}"));
    }
    let count = source.lines().count();
    if count > 1 {
        facts.push(format!("{count} lines"));
    }
    if facets.contains(&NodeFacet::Generated) {
        facts.push("generated".to_owned());
    }
    if facets.contains(&NodeFacet::Test) {
        facts.push("test".to_owned());
    }
    facts.extend(region_facts(regions)?);
    out.raw_line(1, &facts.join(SEPARATOR))
}

/// The regions as `role START..END`, one fact per region.
fn region_facts(regions: &[NodeRegion]) -> Result<Vec<String>, TextError> {
    regions
        .iter()
        .map(|NodeRegion { role, range }| {
            Ok(format!(
                "{} {}..{}",
                wire_name(role)?,
                range.start,
                range.end
            ))
        })
        .collect()
}
