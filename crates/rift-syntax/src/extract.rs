//! The bounded tree walk every provider's `analyze` shares.
//!
//! The walk is grammar-agnostic. Per-language decisions - what counts as a
//! declaration, which node opens a nesting scope, where an attached span
//! starts - come from the grammar's [`GrammarRules`], so a new language
//! plugs in without touching the walk or its node and depth budgets.

use rift_core::Error;
use rift_protocol::read::{Documentation, Extensions, Language, Signature, SymbolFacet};
use tree_sitter::{Node, TreeCursor};

use crate::document::{ByteRange, SyntaxNode, SyntaxSymbol};
use crate::failure::{SyntaxError, SyntaxFault, position_overflow};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Pushes one worklist entry per named child of `node`, reversed, so a
/// worklist that pops from its end visits the children in source order.
///
/// `cursor` steps from each child to its next sibling, so the work is linear
/// in the child count. [`Node::child`] and [`Node::named_child`] start again
/// from the first child on every call, so indexing each child in turn is
/// quadratic in that count, and a file of comment lines alone places every
/// line under the root.
pub(crate) fn push_named_children<'tree, T>(
    pending: &mut Vec<T>,
    cursor: &mut TreeCursor<'tree>,
    node: Node<'tree>,
    entry: impl FnMut(Node<'tree>) -> T,
) {
    let first_child = pending.len();
    pending.extend(node.named_children(cursor).map(entry));
    pending[first_child..].reverse();
}

/// The nodes the walk has visited, each with its parent and previous named
/// sibling.
///
/// Tree-sitter's `Node::parent`, `Node::prev_sibling`, and `Node::next_sibling`
/// search again from the root or from the parent's first child on every call,
/// so a rule stepping through a run of siblings with them is quadratic in the
/// run's length. The walk visits every named node once, children in source
/// order, and records both as it goes, so each step is one lookup.
#[derive(Debug, Default)]
pub(crate) struct Visits<'tree> {
    /// Every visited node, by visit index.
    nodes: Vec<Node<'tree>>,
    /// Each visited node's parent.
    parents: Vec<Option<usize>>,
    /// Each visited node's previous sibling, when that sibling is named.
    previous_siblings: Vec<Option<usize>>,
    /// Each visited node's most recently visited child.
    last_children: Vec<Option<usize>>,
}

impl<'tree> Visits<'tree> {
    /// Records one visited node and returns its visit index.
    ///
    /// A node's parent is visited before it and its earlier siblings are
    /// visited before it, so the parent's most recently visited child is the
    /// previous named sibling whenever no anonymous token stands between.
    fn visit(
        &mut self,
        node: Node<'tree>,
        parent: Option<usize>,
        follows_named_sibling: bool,
    ) -> usize {
        let index = self.nodes.len();
        let previous_child = parent.and_then(|parent| self.last_children[parent].replace(index));
        self.nodes.push(node);
        self.parents.push(parent);
        self.previous_siblings
            .push(previous_child.filter(|_| follows_named_sibling));
        self.last_children.push(None);
        index
    }

    /// The visited node at `index`.
    fn visited(&self, index: usize) -> Visited<'_, 'tree> {
        Visited {
            visits: self,
            index,
        }
    }
}

/// One visited node, with the parent and previous named sibling the walk
/// recorded for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Visited<'walk, 'tree> {
    visits: &'walk Visits<'tree>,
    index: usize,
}

impl<'tree> Visited<'_, 'tree> {
    /// The visited node.
    pub(crate) fn node(self) -> Node<'tree> {
        self.visits.nodes[self.index]
    }

    /// The node's parent; `None` for the root.
    pub(crate) fn parent(self) -> Option<Self> {
        self.visits.parents[self.index].map(|index| self.visits.visited(index))
    }

    /// The sibling immediately before the node, when that sibling is named;
    /// `None` when the node comes first or an anonymous token precedes it.
    pub(crate) fn previous_sibling(self) -> Option<Self> {
        self.visits.previous_siblings[self.index].map(|index| self.visits.visited(index))
    }
}

/// Per-grammar decisions the shared walk delegates.
pub(crate) trait GrammarRules {
    /// The declaration facts behind the visited node; `None` for a node that
    /// declares nothing.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError`] when a grammar position cannot fit the wire
    /// width.
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, SyntaxError>;

    /// The name child declarations nest under; `None` when `node` opens no
    /// scope.
    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String>;

    /// Byte offset where the visited node's whole declaration starts,
    /// attached attributes and doc comments included.
    fn declaration_start(&self, visited: Visited<'_, '_>, text: &str) -> usize;

    /// The grammar's exact declaration name field, absent for providers without one.
    fn name_range(&self, _node: Node<'_>) -> Result<Option<ByteRange>, SyntaxError> {
        Ok(None)
    }

    /// The separator qualified names join with, such as `::`.
    fn qualification_separator(&self) -> &'static str;
}

/// One declaration's rendered facts, before the walk adds qualification and
/// spans.
#[derive(Debug)]
pub(crate) struct Declaration {
    /// Declared short name.
    pub(crate) name: String,
    /// The provider's kind word, carried on the wire unchanged.
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
    /// Exact source ranges for attached documentation.
    pub(crate) documentation_ranges: Vec<ByteRange>,
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
    let mut visits = Visits::default();
    let mut pending = vec![Queued {
        node: root,
        parent: None,
        qualification: String::new(),
        depth: 0,
        follows_named_sibling: false,
    }];
    let mut cursor = root.walk();
    while let Some(queued) = pending.pop() {
        let Queued {
            node,
            parent,
            qualification,
            depth,
            follows_named_sibling,
        } = queued;
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

        let node_index = visits.visit(node, parent, follows_named_sibling);
        let range = byte_range(node)?;
        nodes.push(SyntaxNode {
            kind: node.kind().into(),
            range,
            parent,
            has_error: node.is_error() || node.is_missing(),
        });

        let visited = visits.visited(node_index);
        if let Some(declaration) = rules.declaration(visited, text)? {
            symbols.push(qualified_symbol(
                declaration,
                visited,
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

        if pending.len() + nodes.len() + node.named_child_count() > limits.syntax_nodes_max() {
            return Err(too_many_nodes(source, limits));
        }
        queue_children(
            &mut pending,
            &mut cursor,
            node,
            node_index,
            &child_qualification,
            depth + 1,
        );
    }
    Ok((nodes, symbols))
}

/// One node the walk has queued, with what its visit needs from its parent.
struct Queued<'tree> {
    node: Node<'tree>,
    /// The parent's visit index; `None` for the root.
    parent: Option<usize>,
    /// The qualification declarations under this node take.
    qualification: String,
    depth: usize,
    /// Whether a named sibling immediately precedes this node.
    follows_named_sibling: bool,
}

/// Queues `node`'s named children so they pop in source order, each marked
/// with whether a named sibling immediately precedes it. The anonymous
/// children are read only to set that mark.
fn queue_children<'tree>(
    pending: &mut Vec<Queued<'tree>>,
    cursor: &mut TreeCursor<'tree>,
    node: Node<'tree>,
    parent: usize,
    qualification: &str,
    depth: usize,
) {
    let first_child = pending.len();
    let mut follows_named_sibling = false;
    for child in node.children(cursor) {
        if child.is_named() {
            pending.push(Queued {
                node: child,
                parent: Some(parent),
                qualification: qualification.to_owned(),
                depth,
                follows_named_sibling,
            });
        }
        follows_named_sibling = child.is_named();
    }
    pending[first_child..].reverse();
}

/// Places one rendered declaration in the file's symbol space: qualification,
/// container, the attachment-extended span, and the signature its facets and
/// body range derive.
fn qualified_symbol(
    declaration: Declaration,
    visited: Visited<'_, '_>,
    text: &str,
    qualification: &str,
    item_range: ByteRange,
    language: &Language,
    rules: &dyn GrammarRules,
) -> Result<SyntaxSymbol, SyntaxError> {
    let node = visited.node();
    let start = rules.declaration_start(visited, text);
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
        name_range: rules.name_range(node)?,
        body_range: declaration.body_range,
        signatures,
        documentation: declaration.documentation,
        documentation_ranges: declaration.documentation_ranges,
    })
}

/// The terminator a bodyless declaration ends its item text with in every
/// grammar that spells one: `fn next(&mut self) -> Option<Self::Item>;`,
/// `map<U>(callbackfn: (value: T) => U): U[];`.
const DECLARATION_TERMINATOR: char = ';';

/// A bodyless declaration's own item text as its signature, without its
/// closing `DECLARATION_TERMINATOR` and the whitespace around it.
fn bodyless_display(item: &str) -> &str {
    let item = item.trim_end();
    item.strip_suffix(DECLARATION_TERMINATOR)
        .unwrap_or(item)
        .trim_end()
}

/// One rendered callable form for a declaration the grammar marks
/// [`SymbolFacet::Callable`]: the source text from the declaration's own item
/// start to where its implementation begins, trimmed of trailing whitespace.
/// A declaration with no implementation - a trait method with no body, an
/// interface method signature, a TypeScript overload - renders its whole item
/// text without the closing `;`. `None` for a declaration that is not
/// callable.
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
    let start = node.start_byte();
    let display = match declaration.body_range {
        Some(body_range) => {
            let end = usize::try_from(body_range.start)
                .unwrap_or(text.len())
                .min(text.len());
            text.get(start..end)?.trim_end()
        }
        None => bodyless_display(text.get(start..node.end_byte())?),
    };
    Some(Signature {
        display: display.to_owned(),
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use rift_core::ProjectPath;
    use rift_protocol::read::Language;
    use tree_sitter::{Node, Parser};

    use super::{Declaration, GrammarRules, Visited, extract};
    use crate::failure::SyntaxError;
    use crate::provider::{SyntaxLimits, SyntaxSource};

    /// Rules that compare every visited node's recorded parent and previous
    /// sibling with the ones tree-sitter computes itself, and declare nothing.
    #[derive(Debug, Default)]
    struct LinkOracle {
        compared: Cell<usize>,
    }

    impl GrammarRules for LinkOracle {
        fn declaration(
            &self,
            visited: Visited<'_, '_>,
            text: &str,
        ) -> Result<Option<Declaration>, SyntaxError> {
            let node = visited.node();
            let spelling = text.get(node.byte_range()).unwrap_or_default();
            assert_eq!(
                visited.parent().map(Visited::node),
                node.parent(),
                "the recorded parent must be tree-sitter's parent: kind={}, text={spelling:?}",
                node.kind(),
            );
            assert_eq!(
                visited.previous_sibling().map(Visited::node),
                node.prev_sibling().filter(Node::is_named),
                "the recorded previous sibling must be tree-sitter's previous sibling when \
                 that sibling is named: kind={}, text={spelling:?}",
                node.kind(),
            );
            self.compared.set(self.compared.get() + 1);
            Ok(None)
        }

        fn container_name(&self, _node: Node<'_>, _text: &str) -> Option<String> {
            None
        }

        fn declaration_start(&self, visited: Visited<'_, '_>, _text: &str) -> usize {
            visited.node().start_byte()
        }

        fn qualification_separator(&self) -> &'static str {
            "::"
        }
    }

    /// Walks `text` parsed with `grammar` and returns how many nodes the
    /// oracle compared.
    fn compared_nodes(grammar: &tree_sitter::Language, name: &str, text: &str) -> usize {
        let mut parser = Parser::new();
        parser
            .set_language(grammar)
            .expect("the pinned grammar must load");
        let tree = parser.parse(text, None).expect("the fixture must parse");
        let path = ProjectPath::new("fixture").expect("valid fixture path");
        let source = SyntaxSource { path: &path, text };
        let language = Language {
            name: name.to_owned(),
            dialect: None,
        };
        let oracle = LinkOracle::default();
        extract(
            tree.root_node(),
            source,
            SyntaxLimits::default(),
            &language,
            &oracle,
        )
        .expect("the fixture must walk");
        oracle.compared.get()
    }

    #[test]
    fn recorded_parents_and_siblings_match_tree_sitter_across_grammars() {
        let fixtures: [(tree_sitter::Language, &str, &str); 4] = [
            (
                tree_sitter_rust::LANGUAGE.into(),
                "rust",
                "//! Crate docs.\n/// Doc.\n#[derive(Debug)]\npub struct Beacon;\n\n\
                 impl Beacon {\n    /// Method.\n    pub fn beam(&self) {}\n}\n\
                 fn broken( {\n",
            ),
            (
                tree_sitter_javascript::LANGUAGE.into(),
                "javascript",
                "/** Doc. */\nexport const beacon = () => 1;\n// note\n\
                 class Beacon { /** Method. */ beam() {} }\nmodule.exports = { beacon };\n",
            ),
            (
                tree_sitter_python::LANGUAGE.into(),
                "python",
                "@decorated\ndef beacon():\n    \"Doc.\"\n    return 1\n\n\
                 class Beacon:\n    LIGHT = 1\n",
            ),
            (
                tree_sitter_md::LANGUAGE.into(),
                "markdown",
                "# Beacon\n\nText.\n\nLoose\n---\n\n- item\n- item\n",
            ),
        ];
        for (grammar, name, text) in &fixtures {
            let compared = compared_nodes(grammar, name, text);
            assert!(
                compared > 10,
                "the oracle must compare the fixture's nodes: language={name}, \
                 compared={compared}"
            );
        }
    }
}
