//! Vue, Svelte, and Angular template syntax with original source ranges.

use rift_error::{RiftError, errors};
use rift_protocol::read::{Language, NodeFacet};
use tree_sitter::{Node, Range};

use crate::extract::{Declaration, GrammarRules, Visited};
use crate::{SyntaxDocument, SyntaxLimits, SyntaxProvider, SyntaxSource};

/// The shipped Vue grammar.
pub(crate) fn vue_grammar() -> tree_sitter::Language {
    tree_sitter_vue_next::LANGUAGE.into()
}

/// The shipped Svelte grammar.
pub(crate) fn svelte_grammar() -> tree_sitter::Language {
    tree_sitter_svelte_ng::LANGUAGE.into()
}

/// The shipped Angular template grammar.
pub(crate) fn angular_grammar() -> tree_sitter::Language {
    tree_sitter_angular::language()
}

/// Bounded Vue component syntax provider.
#[derive(Debug)]
pub struct VueSyntaxProvider {
    language: Language,
}

impl Default for VueSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "vue".into(),
                dialect: None,
            },
        }
    }
}

/// Bounded Svelte component syntax provider.
#[derive(Debug)]
pub struct SvelteSyntaxProvider {
    language: Language,
}

impl Default for SvelteSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "svelte".into(),
                dialect: None,
            },
        }
    }
}

/// Bounded Angular template provider selected by component ownership or configuration.
#[derive(Debug)]
pub struct AngularSyntaxProvider {
    language: Language,
}

impl Default for AngularSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "html".into(),
                dialect: Some("angular".into()),
            },
        }
    }
}

impl SyntaxProvider for VueSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        let document = analyze(&self.language, &vue_grammar(), source, limits, &[])?;
        crate::embedded::append(source, limits, &document)
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        node_facets(kind)
    }
}

impl SyntaxProvider for SvelteSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        let document = analyze(&self.language, &svelte_grammar(), source, limits, &[])?;
        crate::embedded::append(source, limits, &document)
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        node_facets(kind)
    }
}

impl SyntaxProvider for AngularSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }
    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        let document = analyze(&self.language, &angular_grammar(), source, limits, &[])?;
        crate::embedded::append(source, limits, &document)
    }
    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        node_facets(kind)
    }
}

/// Parses an Angular template range captured from its TypeScript component.
///
/// # Errors
///
/// Returns provider failures for source, node, and depth bounds, invalid ranges, or cancellation.
pub fn analyze_angular_included(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    range: crate::ByteRange,
) -> Result<SyntaxDocument, RiftError> {
    crate::restore::range(source.text, range)?;
    let range = crate::embedded::source_range(source.text, range);
    let provider = AngularSyntaxProvider::default();
    analyze(
        provider.language(),
        &angular_grammar(),
        source,
        limits,
        &[range],
    )
}

fn analyze(
    language: &Language,
    grammar: &tree_sitter::Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    ranges: &[Range],
) -> Result<SyntaxDocument, RiftError> {
    crate::parse::document(source, limits, language, grammar, &ComponentRules, ranges)
}

struct ComponentRules;

impl GrammarRules for ComponentRules {
    fn declaration(
        &self,
        _visited: Visited<'_, '_>,
        _text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        Ok(None)
    }
    fn container_name(&self, _node: Node<'_>, _text: &str) -> Option<String> {
        None
    }
    fn declaration_start(&self, visited: Visited<'_, '_>, _text: &str) -> usize {
        visited.node().start_byte()
    }
    fn qualification_separator(&self) -> &'static str {
        "."
    }
}

fn node_facets(kind: &str) -> Vec<NodeFacet> {
    match kind {
        "comment" => vec![NodeFacet::Comment],
        "attribute" | "directive_attribute" => vec![NodeFacet::Annotation],
        "interpolation" | "expression" | "pipe_call" => vec![NodeFacet::Expression],
        kind if kind.ends_with("_statement") || kind.ends_with("_block") => vec![NodeFacet::Block],
        _ => crate::ecmascript::node_facets(kind),
    }
}

/// Components declare symbols through their embedded JavaScript, TypeScript, and CSS.
pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    crate::embedded::restored_symbol_kind(name)
}

/// Adds captured inline Angular template syntax to its TypeScript document.
///
/// # Errors
///
/// Returns provider failures for invalid ranges or when combined syntax crosses a node or depth bound.
pub fn append_angular_templates(
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    document: &SyntaxDocument,
    ranges: &[crate::ByteRange],
) -> Result<SyntaxDocument, RiftError> {
    limits.admit_source(source)?;
    if document.source_digest() != Some(&rift_core::FileDigest::of(source.text.as_bytes())) {
        return errors::syntax::facts_range_invalid().fail();
    }
    if document.nodes().len() > limits.syntax_nodes_max() {
        return errors::syntax::too_many_nodes()
            .path(source.path)
            .syntax_nodes_max(limits.syntax_nodes_max())
            .fail();
    }
    crate::embedded::admit_depth(source, limits, 0, document)?;
    if ranges.len() > limits.syntax_nodes_max() {
        return errors::syntax::too_many_nodes()
            .path(source.path)
            .syntax_nodes_max(limits.syntax_nodes_max())
            .fail();
    }
    let mut previous_end = 0;
    for range in ranges {
        crate::restore::range(source.text, *range)?;
        if range.start < previous_end {
            return errors::syntax::facts_range_invalid().fail();
        }
        previous_end = range.end;
    }
    let mut nodes = document.nodes().to_vec();
    let mut has_errors = document.has_errors();
    let omitted = document.left_out_declaration_count();
    for range in ranges {
        if range.start == range.end {
            continue;
        }
        let parent = containing_node(document, *range)
            .ok_or_else(|| errors::syntax::facts_range_invalid().error())?;
        let depth = crate::embedded::node_depth(document, parent) + 1;
        let remaining = crate::embedded::remaining_limits(source, limits, nodes.len(), depth)?;
        let template = analyze_angular_included(source, remaining, *range)
            .map_err(|error| crate::embedded::aggregate_error(error, source, limits))?;
        crate::embedded::admit_depth(source, limits, depth, &template)?;
        has_errors |= template.has_errors();
        crate::embedded::append_nodes(&mut nodes, &template, parent);
    }
    crate::embedded::order_nodes(&mut nodes);
    Ok(SyntaxDocument::new(
        document.language().clone(),
        source.path.clone(),
        nodes,
        document.symbols().to_vec(),
        has_errors,
    )
    .with_source_witness(source.text)
    .with_syntax_limits(limits)
    .with_left_out_declarations(omitted))
}

fn containing_node(document: &SyntaxDocument, range: crate::ByteRange) -> Option<usize> {
    let mut index = document
        .nodes()
        .partition_point(|node| node.range.start <= range.start)
        .checked_sub(1)?;
    loop {
        let node = &document.nodes()[index];
        if node.range.start <= range.start && range.end <= node.range.end {
            return Some(index);
        }
        index = node.parent?;
    }
}
