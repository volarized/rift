//! The bounded tree walk every provider's `analyze` shares.
//!
//! The walk is grammar-agnostic. Per-language decisions - what counts as a
//! declaration, which node opens a nesting scope, where an attached span
//! starts - come from the grammar's [`GrammarRules`], so a new language
//! plugs in without touching the walk or its node and depth budgets.

use rift_core::Error;
use rift_protocol::read::{Documentation, Extensions, Language, Signature, SymbolFacet};
use tree_sitter::Node;

use crate::document::{ByteRange, SyntaxNode, SyntaxSymbol};
use crate::failure::{SyntaxError, SyntaxFault, position_overflow};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Per-grammar decisions the shared walk delegates.
pub(crate) trait GrammarRules {
    /// The declaration facts behind `node`; `None` for a node that declares
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] when a grammar position cannot fit the wire
    /// width.
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError>;

    /// The name child declarations nest under; `None` when `node` opens no
    /// scope.
    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String>;

    /// Byte offset where `node`'s whole declaration starts, attached
    /// attributes and doc comments included.
    fn declaration_start(&self, node: Node<'_>, text: &str) -> usize;

    /// The separator qualified names join with, such as `::`.
    fn qualification_separator(&self) -> &'static str;
}

/// One declaration's rendered facts, before the walk adds qualification and
/// spans.
#[derive(Debug)]
pub(crate) struct Declaration {
    /// Declared short name.
    pub(crate) name: String,
    /// The provider's kind word behind the wire kind `{language}.{kind}`.
    pub(crate) kind: &'static str,
    /// Portable categories, in the grammar's declared order.
    pub(crate) facets: Vec<SymbolFacet>,
    /// Authored visibility spelling; `None` when the language states none.
    pub(crate) visibility: Option<String>,
    /// The implementation part's span; `None` for a declaration without one.
    pub(crate) body_range: Option<ByteRange>,
    /// Doc comments the grammar attaches to this declaration; empty when
    /// nothing attaches. Carried through to [`SyntaxSymbol::documentation`]
    /// unchanged - the walk itself never inspects a comment's syntax.
    pub(crate) documentation: Vec<Documentation>,
}

/// Converts one node's span to the wire byte width.
pub(crate) fn byte_range(node: Node<'_>) -> Result<ByteRange, SyntaxError> {
    let start =
        u64::try_from(node.start_byte()).map_err(|source| position_overflow(node, source))?;
    let end = u64::try_from(node.end_byte()).map_err(|source| position_overflow(node, source))?;
    Ok(ByteRange { start, end })
}

/// Walks the parsed tree once, collecting named nodes and declarations
/// within the configured node and depth budgets.
///
/// # Errors
///
/// Returns [`SyntaxError`] when the tree exceeds a bound or a position
/// cannot fit the wire width.
pub(crate) fn extract(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
    rules: &dyn GrammarRules,
) -> Result<(Vec<SyntaxNode>, Vec<SyntaxSymbol>), SyntaxError> {
    let text = source.text;
    let mut nodes = Vec::new();
    let mut symbols = Vec::new();
    let mut pending = vec![(root, None, String::new(), 0_usize)];
    while let Some((node, parent, qualification, depth)) = pending.pop() {
        if depth > limits.syntax_depth_max() {
            return Err(Error::new(SyntaxFault::TooDeep {
                path: source.path.clone(),
                syntax_depth_max: limits.syntax_depth_max(),
            }));
        }
        assert!(
            nodes.len() < limits.syntax_nodes_max(),
            "the enqueue guard must keep the walker below the node bound: \
             nodes={}, syntax_nodes_max={}",
            nodes.len(),
            limits.syntax_nodes_max(),
        );

        let node_index = nodes.len();
        let range = byte_range(node)?;
        nodes.push(SyntaxNode {
            kind: node.kind().into(),
            range,
            parent,
            has_error: node.is_error() || node.is_missing(),
        });

        if let Some(declaration) = rules.declaration(node, text)? {
            symbols.push(qualified_symbol(
                declaration,
                node,
                text,
                &qualification,
                range,
                language,
                rules,
            )?);
        }
        let child_qualification = rules.container_name(node, text).map_or_else(
            || qualification.clone(),
            |name| qualify(rules.qualification_separator(), &qualification, &name),
        );

        for child_index in (0..node.child_count()).rev() {
            let Some(child) = node.child(child_index) else {
                continue;
            };
            if !child.is_named() {
                continue;
            }
            if pending.len() + nodes.len() >= limits.syntax_nodes_max() {
                return Err(too_many_nodes(source, limits));
            }
            pending.push((
                child,
                Some(node_index),
                child_qualification.clone(),
                depth + 1,
            ));
        }
    }
    Ok((nodes, symbols))
}

/// Places one rendered declaration in the file's symbol space: qualification,
/// container, the attachment-extended span, and the signature its facets and
/// body range derive.
fn qualified_symbol(
    declaration: Declaration,
    node: Node<'_>,
    text: &str,
    qualification: &str,
    item_range: ByteRange,
    language: &Language,
    rules: &dyn GrammarRules,
) -> Result<SyntaxSymbol, SyntaxError> {
    let start = rules.declaration_start(node, text);
    let start = u64::try_from(start).map_err(|source| position_overflow(node, source))?;
    let signatures = callable_signature(&declaration, node, text, language)
        .into_iter()
        .collect();
    Ok(SyntaxSymbol {
        qualified_name: qualify(
            rules.qualification_separator(),
            qualification,
            &declaration.name,
        ),
        container: (!qualification.is_empty()).then(|| qualification.to_owned()),
        name: declaration.name,
        kind: declaration.kind,
        facets: declaration.facets,
        visibility: declaration.visibility,
        range: ByteRange {
            start,
            end: item_range.end,
        },
        item_range,
        body_range: declaration.body_range,
        signatures,
        documentation: declaration.documentation,
    })
}

/// One rendered callable form for a declaration the grammar marks
/// [`SymbolFacet::Callable`] and gives an implementation: the source text
/// from the declaration's own item start to where that implementation
/// begins, trimmed of trailing whitespace. `None` for a declaration that is
/// not callable, or one with no implementation - a trait method with no
/// body, an interface method signature.
///
/// This is the one place `signatures` is derived: every provider's grammar
/// already states whether a kind is callable (`Declaration::facets`) and
/// where its implementation starts (`Declaration::body_range`), so deriving
/// the header here, once, keeps a match on language out of this walk.
fn callable_signature(
    declaration: &Declaration,
    node: Node<'_>,
    text: &str,
    language: &Language,
) -> Option<Signature> {
    if !declaration.facets.contains(&SymbolFacet::Callable) {
        return None;
    }
    let body_range = declaration.body_range?;
    let start = node.start_byte();
    let end = usize::try_from(body_range.start)
        .unwrap_or(text.len())
        .min(text.len());
    let display = text.get(start..end)?.trim_end().to_owned();
    Some(Signature {
        display,
        links: Vec::new(),
        language: language.clone(),
        receiver: None,
        parameters: Vec::new(),
        returns: Vec::new(),
        type_parameters: Vec::new(),
        throws: Vec::new(),
        effects: Vec::new(),
        extensions: Extensions(std::collections::BTreeMap::new()),
    })
}

fn too_many_nodes(source: SyntaxSource<'_>, limits: SyntaxLimits) -> SyntaxError {
    Error::new(SyntaxFault::TooManyNodes {
        path: source.path.clone(),
        syntax_nodes_max: limits.syntax_nodes_max(),
    })
}

fn qualify(separator: &str, parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}{separator}{name}")
    }
}
