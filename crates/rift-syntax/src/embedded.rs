//! Included script and style parses retain ranges into their host's original bytes.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use rift_core::line::{LineEnding, lines_inclusive};
use rift_error::{RiftError, errors};
use tree_sitter::{Point, Range};

use crate::{SyntaxDocument, SyntaxLimits, SyntaxSource, TypeScriptDialect};

/// Host grammar spellings shared by HTML and component grammars.
const RAW_TEXT: &str = "raw_text";
const SCRIPT: &str = "script_element";
const STYLE: &str = "style_element";
const START_TAG: &str = "start_tag";
const ATTRIBUTE: &str = "attribute";
const ATTRIBUTE_NAME: &str = "attribute_name";
const ATTRIBUTE_VALUE: &str = "attribute_value";

/// Language selected by a script or style element's authored attributes.
#[derive(Clone, Copy)]
enum EmbeddedLanguage {
    JavaScript,
    TypeScript,
    Css,
}

/// Appends bounded script and style facts to the host grammar's document.
pub(crate) fn append(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    document: &SyntaxDocument,
) -> Result<SyntaxDocument, RiftError> {
    let lines = line_starts(source.text);
    let selections = selected_languages(document, source.text);
    let mut nodes = document.nodes().to_vec();
    let mut symbols = document.symbols().to_vec();
    let mut has_errors = document.has_errors();
    let mut omitted = document.left_out_declaration_count();
    for (index, node) in document.nodes().iter().enumerate() {
        if node.kind != RAW_TEXT {
            continue;
        }
        let Some(parent) = node.parent else {
            continue;
        };
        let Some(&language) = selections.get(&parent) else {
            continue;
        };
        let depth = node_depth(document, index) + 1;
        let remaining = remaining_limits(source, limits, nodes.len(), depth)?;
        let range = included_range(source.text, node.range, &lines);
        let embedded = analyze(language, source, remaining, range)
            .map_err(|error| aggregate_error(error, source, limits))?;
        admit_depth(source, limits, depth, &embedded)?;
        has_errors |= embedded.has_errors();
        omitted = omitted
            .checked_add(embedded.left_out_declaration_count())
            .expect("bounded omitted declarations");
        append_nodes(&mut nodes, &embedded, index);
        symbols.extend_from_slice(embedded.symbols());
    }
    order_nodes(&mut nodes);
    symbols.sort_by_key(|symbol| symbol.range.start);
    Ok(SyntaxDocument::new(
        document.language().clone(),
        source.path.clone(),
        nodes,
        symbols,
        has_errors,
    )
    .with_source_witness(source.text)
    .with_syntax_limits(limits)
    .with_left_out_declarations(omitted))
}

fn analyze(
    language: EmbeddedLanguage,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    range: Range,
) -> Result<SyntaxDocument, RiftError> {
    match language {
        EmbeddedLanguage::JavaScript => crate::ecmascript::analyze_included(
            &crate::ShippedLanguage::JavaScript.language(),
            &crate::javascript::javascript_grammar(),
            crate::javascript::javascript_kinds(),
            limits,
            source,
            &[range],
        ),
        EmbeddedLanguage::TypeScript => crate::ecmascript::analyze_included(
            &crate::ShippedLanguage::TypeScript.language(),
            &TypeScriptDialect::TypeScript.grammar(),
            TypeScriptDialect::TypeScript.kinds(),
            limits,
            source,
            &[range],
        ),
        EmbeddedLanguage::Css => crate::css::analyze_included(source, limits, range),
    }
}

fn selected_languages(document: &SyntaxDocument, text: &str) -> BTreeMap<usize, EmbeddedLanguage> {
    let mut tags = BTreeMap::new();
    let mut attributes: BTreeMap<usize, BTreeMap<&str, &str>> = BTreeMap::new();
    for (index, node) in document.nodes().iter().enumerate() {
        let Some(parent) = node.parent else {
            continue;
        };
        if node.kind == START_TAG {
            tags.insert(parent, index);
        }
        if node.kind == ATTRIBUTE
            && let Some((name, value)) = attribute(document, index, text)
        {
            attributes.entry(parent).or_default().insert(name, value);
        }
    }
    tags.into_iter()
        .filter_map(|(parent, tag)| {
            let attribute = attributes.get(&tag);
            let language = attribute.and_then(|values| values.get("lang").copied());
            let script_type = attribute.and_then(|values| values.get("type").copied());
            selection(document.nodes()[parent].kind, language, script_type)
                .map(|language| (parent, language))
        })
        .collect()
}

fn selection(
    host: &str,
    language: Option<&str>,
    script_type: Option<&str>,
) -> Option<EmbeddedLanguage> {
    match (host, language, script_type) {
        (
            SCRIPT,
            None | Some("js" | "javascript"),
            None | Some("module" | "text/javascript" | "application/javascript"),
        ) => Some(EmbeddedLanguage::JavaScript),
        (SCRIPT, Some("ts" | "typescript"), _) => Some(EmbeddedLanguage::TypeScript),
        (STYLE, None | Some("css"), _) => Some(EmbeddedLanguage::Css),
        _ => None,
    }
}

fn attribute<'a>(
    document: &SyntaxDocument,
    index: usize,
    text: &'a str,
) -> Option<(&'a str, &'a str)> {
    let range = document.nodes()[index].range;
    let children = document.nodes()[index + 1..]
        .iter()
        .take_while(|node| node.range.start < range.end);
    let mut name = None;
    let mut value = None;
    for node in children {
        match node.kind {
            ATTRIBUTE_NAME => name = range_text(text, node.range),
            ATTRIBUTE_VALUE => value = range_text(text, node.range),
            _ => {}
        }
    }
    name.zip(value)
}

fn range_text(text: &str, range: crate::ByteRange) -> Option<&str> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    text.get(start..end)
}

pub(crate) fn remaining_limits(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    count: usize,
    depth: usize,
) -> Result<SyntaxLimits, RiftError> {
    if count >= limits.syntax_nodes_max() {
        return errors::syntax::too_many_nodes()
            .path(source.path)
            .syntax_nodes_max(limits.syntax_nodes_max())
            .fail();
    }
    if depth > limits.syntax_depth_max() {
        return errors::syntax::too_deep()
            .path(source.path)
            .syntax_depth_max(limits.syntax_depth_max())
            .fail();
    }
    SyntaxLimits::new(
        limits.source_bytes_max(),
        limits.syntax_nodes_max() - count,
        (limits.syntax_depth_max() - depth).max(1),
    )
}

pub(crate) fn node_depth(document: &SyntaxDocument, index: usize) -> usize {
    let mut depth = 0;
    let mut parent = document.nodes()[index].parent;
    while let Some(index) = parent {
        depth += 1;
        parent = document.nodes()[index].parent;
    }
    depth
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    let mut position = 0;
    for line in lines_inclusive(text) {
        position += line.len();
        if LineEnding::of(line).is_some() {
            starts.push(position);
        }
    }
    starts
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "captured ranges originate in the same in-memory source"
)]
fn included_range(text: &str, range: crate::ByteRange, lines: &[usize]) -> Range {
    assert!(
        range.end <= text.len() as u64,
        "captured range must fit original source: range={range:?}"
    );
    let start_byte = range.start as usize;
    let end_byte = range.end as usize;
    Range {
        start_byte,
        end_byte,
        start_point: point(start_byte, lines),
        end_point: point(end_byte, lines),
    }
}

fn point(position: usize, lines: &[usize]) -> Point {
    let row = lines
        .partition_point(|start| *start <= position)
        .saturating_sub(1);
    Point {
        row,
        column: position - lines[row],
    }
}

pub(crate) fn order_nodes(nodes: &mut Vec<crate::SyntaxNode>) {
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by_key(|index| (nodes[*index].range.start, Reverse(nodes[*index].range.end)));
    let mut positions = vec![0; nodes.len()];
    for (position, index) in order.iter().enumerate() {
        positions[*index] = position;
    }
    *nodes = order
        .into_iter()
        .map(|index| {
            let mut node = nodes[index].clone();
            node.parent = node.parent.map(|parent| positions[parent]);
            node
        })
        .collect();
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    crate::ecmascript::restored_symbol_kind(TypeScriptDialect::TypeScript.kinds(), name)
        .or_else(|| crate::css::restored_symbol_kind(name))
}

/// Appends one included tree and keeps every parent attached to its host node.
pub(crate) fn append_nodes(
    nodes: &mut Vec<crate::SyntaxNode>,
    embedded: &SyntaxDocument,
    parent: usize,
) {
    let offset = nodes.len();
    nodes.extend(embedded.nodes().iter().cloned().map(|mut node| {
        node.parent = Some(node.parent.map_or(parent, |parent| offset + parent));
        node
    }));
}

/// Resolves a captured byte range against the original source's line starts.
pub(crate) fn source_range(text: &str, range: crate::ByteRange) -> Range {
    included_range(text, range, &line_starts(text))
}

/// Reports combined parser bounds using the caller's configured ceiling.
pub(crate) fn aggregate_error(
    error: RiftError,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
) -> RiftError {
    match error.slug() {
        errors::syntax::too_many_nodes::SLUG => errors::syntax::too_many_nodes()
            .path(source.path)
            .syntax_nodes_max(limits.syntax_nodes_max())
            .error(),
        errors::syntax::too_deep::SLUG => errors::syntax::too_deep()
            .path(source.path)
            .syntax_depth_max(limits.syntax_depth_max())
            .error(),
        _ => error,
    }
}

/// Checks the included root and its descendants against their host depth.
pub(crate) fn admit_depth(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    host_depth: usize,
    document: &SyntaxDocument,
) -> Result<(), RiftError> {
    let mut depths = Vec::with_capacity(document.nodes().len());
    for node in document.nodes() {
        let depth = node.parent.map_or(host_depth, |parent| depths[parent] + 1);
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        depths.push(depth);
    }
    Ok(())
}
