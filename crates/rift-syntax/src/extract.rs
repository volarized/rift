//! The bounded tree walk every provider's `analyze` shares.
//!
//! The walk is grammar-agnostic. Per-language decisions - what counts as a
//! declaration, which syntax element opens a nesting scope, where an attached span
//! starts - come from the grammar's [`GrammarRules`], so a new language
//! plugs in without touching the walk or its syntax element and depth budgets.

use std::sync::Arc;

use rift_error::errors;
use rift_protocol::read::{Documentation, Extensions, Language, Signature, SymbolFacet};
use tree_sitter::{Node, TreeCursor};

use crate::SyntaxNames;
use crate::document::{ByteRange, SyntaxElement, SyntaxSymbol};
use crate::failure::{RiftError, position_overflow};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Pushes one worklist entry per named child of `syntax element`, reversed, so a
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
    syntax_element: Node<'tree>,
    entry: impl FnMut(Node<'tree>) -> T,
) {
    let first_child = pending.len();
    pending.extend(syntax_element.named_children(cursor).map(entry));
    pending[first_child..].reverse();
}

/// The syntax elements the walk has visited, each with its parent and previous named
/// sibling.
///
/// Tree-sitter's `Node::parent`, `Node::prev_sibling`, and `Node::next_sibling`
/// search again from the root or from the parent's first child on every call,
/// so a rule stepping through a run of siblings with them is quadratic in the
/// run's length. The walk visits every named syntax element once, children in source
/// order, and records both as it goes, so each step is one lookup.
#[derive(Debug, Default)]
pub(crate) struct Visits<'tree> {
    /// Every visited syntax element, by visit index.
    syntax_elements: Vec<Node<'tree>>,
    /// Each visited syntax element's parent.
    parents: Vec<Option<usize>>,
    /// Each visited syntax element's previous sibling, when that sibling is named.
    previous_siblings: Vec<Option<usize>>,
    /// Each visited syntax element's most recently visited child.
    last_children: Vec<Option<usize>>,
}

impl<'tree> Visits<'tree> {
    /// Records one visited syntax element and returns its visit index.
    ///
    /// A syntax element's parent is visited before it and its earlier siblings are
    /// visited before it, so the parent's most recently visited child is the
    /// previous named sibling whenever no anonymous token stands between.
    fn visit(
        &mut self,
        syntax_element: Node<'tree>,
        parent: Option<usize>,
        follows_named_sibling: bool,
    ) -> usize {
        let index = self.syntax_elements.len();
        let previous_child = parent.and_then(|parent| self.last_children[parent].replace(index));
        self.syntax_elements.push(syntax_element);
        self.parents.push(parent);
        self.previous_siblings
            .push(previous_child.filter(|_| follows_named_sibling));
        self.last_children.push(None);
        index
    }

    /// The visited syntax element at `index`.
    fn visited(&self, index: usize) -> Visited<'_, 'tree> {
        Visited {
            visits: self,
            index,
        }
    }
}

/// One visited syntax element, with the parent and previous named sibling the walk
/// recorded for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Visited<'walk, 'tree> {
    visits: &'walk Visits<'tree>,
    index: usize,
}

impl<'tree> Visited<'_, 'tree> {
    /// The visited syntax element.
    pub(crate) fn syntax_element(self) -> Node<'tree> {
        self.visits.syntax_elements[self.index]
    }

    /// The syntax element's parent; `None` for the root.
    pub(crate) fn parent(self) -> Option<Self> {
        self.visits.parents[self.index].map(|index| self.visits.visited(index))
    }

    /// The sibling immediately before the syntax element, when that sibling is named;
    /// `None` when the syntax element comes first or an anonymous token precedes it.
    pub(crate) fn previous_sibling(self) -> Option<Self> {
        self.visits.previous_siblings[self.index].map(|index| self.visits.visited(index))
    }
}

/// Per-grammar decisions the shared walk delegates.
pub(crate) trait GrammarRules {
    /// Export binding recorded at this visited syntax element, when the grammar supplies one.
    fn export_binding(
        &self,
        _visited: Visited<'_, '_>,
        _text: &str,
        _qualification: &str,
    ) -> Result<Option<crate::SyntaxExportBinding>, RiftError> {
        Ok(None)
    }
    /// The declaration facts behind the visited syntax element; `None` for a syntax element that
    /// declares nothing.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a grammar position cannot fit the wire
    /// width.
    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError>;

    /// The name child declarations nest under; `None` when `syntax element` opens no
    /// scope.
    fn container_name(&self, syntax_element: Node<'_>, text: &str) -> Option<String>;

    /// The qualification declarations under this syntax element take.
    fn child_qualification(&self, syntax_element: Node<'_>, text: &str, parent: &str) -> String {
        self.container_name(syntax_element, text).map_or_else(
            || parent.to_owned(),
            |name| qualify(self.qualification_separator(), parent, &name),
        )
    }

    /// Byte offset where the visited syntax element's whole declaration starts,
    /// attached attributes and doc comments included.
    fn declaration_start(&self, visited: Visited<'_, '_>, text: &str) -> usize;

    /// The whole declaration that contains the visited declaration name.
    fn declaration_syntax_element<'tree>(&self, visited: Visited<'_, 'tree>) -> Node<'tree> {
        visited.syntax_element()
    }

    /// Whether the declaration renders a callable signature.
    fn renders_signature(&self, _visited: Visited<'_, '_>) -> bool {
        true
    }

    /// The grammar's exact declaration name field, absent for providers without one.
    fn name_range(&self, _syntax_element: Node<'_>) -> Result<Option<ByteRange>, RiftError> {
        Ok(None)
    }

    /// Captured Rust module path state; unsupported grammars leave it unknown.
    fn module_path(
        &self,
        _visited: Visited<'_, '_>,
        _text: &str,
    ) -> Result<Option<crate::RustModulePath>, RiftError> {
        Ok(None)
    }

    /// Captured Python import and overload decorator association.
    fn python_overload(
        &self,
        _visited: Visited<'_, '_>,
        _text: &str,
    ) -> Result<Option<crate::PythonOverload>, RiftError> {
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

/// Converts one syntax element's span to the wire byte width.
pub(crate) fn byte_range(syntax_element: Node<'_>) -> Result<ByteRange, RiftError> {
    let start = u64::try_from(syntax_element.start_byte())
        .map_err(|source| position_overflow(syntax_element, source))?;
    let end = u64::try_from(syntax_element.end_byte())
        .map_err(|source| position_overflow(syntax_element, source))?;
    Ok(ByteRange { start, end })
}

/// Walks the parsed tree once, collecting named syntax elements and declarations
/// within the configured syntax element and depth budgets.
///
/// # Errors
///
/// Returns [`RiftError`] when the tree exceeds a bound or a position
/// cannot fit the wire width.
pub(crate) fn extract(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
    rules: &dyn GrammarRules,
) -> Result<(Vec<SyntaxElement>, Vec<SyntaxSymbol>), RiftError> {
    let (syntax_elements, symbols, _) =
        extract_with_export_bindings(root, source, limits, language, rules)?;
    Ok((syntax_elements, symbols))
}

pub(crate) type Extraction = (
    Vec<SyntaxElement>,
    Vec<SyntaxSymbol>,
    Vec<crate::SyntaxExportBinding>,
);

/// Records export bindings in the same bounded tree walk as syntax elements and declarations.
pub(crate) fn extract_with_export_bindings(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
    rules: &dyn GrammarRules,
) -> Result<Extraction, RiftError> {
    extract_with_capture(root, source, limits, language, rules, None)
}

pub(crate) fn extract_with_capture(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    language: &Language,
    rules: &dyn GrammarRules,
    mut capture: Option<&mut crate::static_analysis::Capture<'_>>,
) -> Result<Extraction, RiftError> {
    let text = source.text;
    let names = SyntaxNames::new(language).expect("a syntax provider uses a shipped grammar");
    let grammar = names.grammar();
    assert_eq!(
        &*root.language(),
        grammar,
        "a syntax provider must parse with its shipped grammar",
    );
    let mut syntax_elements = Vec::new();
    let mut symbols = Vec::new();
    let mut export_bindings = Vec::new();
    let mut visits = Visits::default();
    let mut pending = vec![Queued {
        syntax_element: root,
        parent: None,
        qualification: String::new(),
        depth: 0,
        follows_named_sibling: false,
    }];
    let mut cursor = root.walk();
    while let Some(queued) = pending.pop() {
        let Queued {
            syntax_element,
            parent,
            qualification,
            depth,
            follows_named_sibling,
        } = queued;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        assert!(
            syntax_elements.len() < limits.syntax_elements_max(),
            "the enqueue guard must keep the walker below the syntax_element bound: \
             syntax_elements={}, syntax_elements_max={}",
            syntax_elements.len(),
            limits.syntax_elements_max(),
        );

        let syntax_element_index = visits.visit(syntax_element, parent, follows_named_sibling);
        let range = byte_range(syntax_element)?;
        syntax_elements.push(SyntaxElement {
            kind: grammar
                .node_kind_for_id(syntax_element.kind_id())
                .expect("a parsed syntax element kind belongs to its shipped grammar"),
            range,
            parent,
            has_error: syntax_element.is_error() || syntax_element.is_missing(),
        });

        let visited = visits.visited(syntax_element_index);
        if let Some(capture) = capture.as_deref_mut() {
            capture.record(syntax_element)?;
        }
        if let Some(binding) = rules.export_binding(visited, text, &qualification)? {
            export_bindings.push(binding);
        }
        if let Some(declaration) = rules.declaration(visited, text)? {
            symbols.push(qualified_symbol(
                declaration,
                visited,
                text,
                &qualification,
                language,
                rules,
                grammar,
            )?);
        }
        let child_qualification = rules.child_qualification(syntax_element, text, &qualification);

        if pending.len() + syntax_elements.len() + syntax_element.named_child_count()
            > limits.syntax_elements_max()
        {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        queue_children(
            &mut pending,
            &mut cursor,
            syntax_element,
            syntax_element_index,
            &child_qualification,
            depth + 1,
        );
    }
    Ok((syntax_elements, symbols, export_bindings))
}

/// One syntax element the walk has queued, with what its visit needs from its parent.
struct Queued<'tree> {
    syntax_element: Node<'tree>,
    /// The parent's visit index; `None` for the root.
    parent: Option<usize>,
    /// The qualification declarations under this syntax element take.
    qualification: String,
    depth: usize,
    /// Whether a named sibling immediately precedes this syntax element.
    follows_named_sibling: bool,
}

/// Queues `syntax element`'s named children so they pop in source order, each marked
/// with whether a named sibling immediately precedes it. The anonymous
/// children are read only to set that mark.
fn queue_children<'tree>(
    pending: &mut Vec<Queued<'tree>>,
    cursor: &mut TreeCursor<'tree>,
    syntax_element: Node<'tree>,
    parent: usize,
    qualification: &str,
    depth: usize,
) {
    let first_child = pending.len();
    let mut follows_named_sibling = false;
    for child in syntax_element.children(cursor) {
        if child.is_named() {
            pending.push(Queued {
                syntax_element: child,
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
    language: &Language,
    rules: &dyn GrammarRules,
    grammar: &'static tree_sitter::Language,
) -> Result<SyntaxSymbol, RiftError> {
    let visited_syntax_element = visited.syntax_element();
    let syntax_element = rules.declaration_syntax_element(visited);
    let item_range = byte_range(syntax_element)?;
    let start = rules.declaration_start(visited, text);
    let start = u64::try_from(start).map_err(|source| position_overflow(syntax_element, source))?;
    let signatures: Vec<Signature> = rules
        .renders_signature(visited)
        .then(|| callable_signature(&declaration, item_range, text, language))
        .flatten()
        .into_iter()
        .collect();
    Ok(SyntaxSymbol {
        declaration_ordinal: 0,
        qualified_name: qualify(
            rules.qualification_separator(),
            qualification,
            &declaration.name,
        ),
        container: (!qualification.is_empty()).then(|| qualification.to_owned()),
        name: declaration.name,
        kind: declaration.kind,
        syntax_element_kind: grammar.node_kind_for_id(syntax_element.kind_id()),
        facets: declaration.facets,
        visibility: declaration.visibility,
        range: ByteRange {
            start,
            end: item_range.end,
        },
        item_range,
        name_range: rules.name_range(visited_syntax_element)?,
        body_range: declaration.body_range,
        signatures: Arc::from(signatures),
        documentation: Arc::from(declaration.documentation),
        documentation_ranges: declaration.documentation_ranges,
        module_path: rules.module_path(visited, text)?,
        python_overload: rules.python_overload(visited, text)?,
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
pub(crate) fn callable_signature(
    declaration: &Declaration,
    item_range: ByteRange,
    text: &str,
    language: &Language,
) -> Option<Signature> {
    if !declaration.facets.contains(&SymbolFacet::Callable) {
        return None;
    }
    let start = usize::try_from(item_range.start).ok()?;
    let display = match declaration.body_range {
        Some(body_range) => {
            let end = usize::try_from(body_range.start)
                .unwrap_or(text.len())
                .min(text.len());
            text.get(start..end)?.trim_end()
        }
        None => bodyless_display(text.get(start..usize::try_from(item_range.end).ok()?)?),
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

pub(crate) fn qualify(separator: &str, parent: &str, name: &str) -> String {
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
    use tree_sitter::{Node, Parser};

    use super::{Declaration, GrammarRules, Visited, extract};
    use crate::failure::RiftError;
    use crate::provider::{SyntaxLimits, SyntaxSource};
    use crate::{ShippedLanguage, SyntaxNames};

    /// Rules that compare every visited syntax element's recorded parent and previous
    /// sibling with the ones tree-sitter computes itself, and declare nothing.
    #[derive(Debug)]
    struct LinkOracle {
        compared: Cell<usize>,
        names: SyntaxNames,
    }

    impl GrammarRules for LinkOracle {
        fn declaration(
            &self,
            visited: Visited<'_, '_>,
            text: &str,
        ) -> Result<Option<Declaration>, RiftError> {
            let syntax_element = visited.syntax_element();
            let spelling = text.get(syntax_element.byte_range()).unwrap_or_default();
            assert_eq!(
                self.names
                    .grammar()
                    .node_kind_for_id(syntax_element.kind_id()),
                Some(syntax_element.kind()),
                "the retained grammar must preserve the parsed syntax element's spelling: text={spelling:?}",
            );
            assert_eq!(
                visited.parent().map(Visited::syntax_element),
                syntax_element.parent(),
                "the recorded parent must be tree-sitter's parent: kind={}, text={spelling:?}",
                syntax_element.kind(),
            );
            assert_eq!(
                visited.previous_sibling().map(Visited::syntax_element),
                syntax_element.prev_sibling().filter(Node::is_named),
                "the recorded previous sibling must be tree-sitter's previous sibling when \
                 that sibling is named: kind={}, text={spelling:?}",
                syntax_element.kind(),
            );
            self.compared.set(self.compared.get() + 1);
            Ok(None)
        }

        fn container_name(&self, _syntax_element: Node<'_>, _text: &str) -> Option<String> {
            None
        }

        fn declaration_start(&self, visited: Visited<'_, '_>, _text: &str) -> usize {
            visited.syntax_element().start_byte()
        }

        fn qualification_separator(&self) -> &'static str {
            "::"
        }
    }

    /// Walks `text` parsed with `grammar` and returns how many syntax elements the
    /// oracle compared.
    fn compared_syntax_elements(
        grammar: &tree_sitter::Language,
        shipped: ShippedLanguage,
        text: &str,
    ) -> usize {
        let mut parser = Parser::new();
        parser
            .set_language(grammar)
            .expect("the pinned grammar must load");
        let tree = parser.parse(text, None).expect("the fixture must parse");
        let path = ProjectPath::new("fixture").expect("valid fixture path");
        let source = SyntaxSource { path: &path, text };
        let language = shipped.language();
        let oracle = LinkOracle {
            compared: Cell::new(0),
            names: SyntaxNames::new(&language).expect("a shipped grammar has names"),
        };
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
        let fixtures: [(tree_sitter::Language, ShippedLanguage, &str); 9] = [
            (
                tree_sitter_rust::LANGUAGE.into(),
                ShippedLanguage::Rust,
                "//! Crate docs.\n/// Doc.\n#[derive(Debug)]\npub struct Beacon;\n\n\
                 impl Beacon {\n    /// Method.\n    pub fn beam(&self) {}\n}\n\
                 fn broken( {\n",
            ),
            (
                tree_sitter_javascript::LANGUAGE.into(),
                ShippedLanguage::JavaScript,
                "/** Doc. */\nexport const beacon = () => 1;\n// note\n\
                 class Beacon { /** Method. */ beam() {} }\nmodule.exports = { beacon };\n",
            ),
            (
                tree_sitter_python::LANGUAGE.into(),
                ShippedLanguage::Python,
                "@decorated\ndef beacon():\n    \"Doc.\"\n    return 1\n\n\
                 class Beacon:\n    LIGHT = 1\n",
            ),
            (
                tree_sitter_md::LANGUAGE.into(),
                ShippedLanguage::Markdown,
                "# Beacon\n\nText.\n\nLoose\n---\n\n- item\n- item\n",
            ),
            (
                tree_sitter_typescript::language_typescript(),
                ShippedLanguage::TypeScript,
                "export interface Beacon { beam<T>(value: T): T; }\nconst light = { beam: 1 };\n",
            ),
            (
                tree_sitter_typescript::language_tsx(),
                ShippedLanguage::TypeScriptTsx,
                "export const Beacon = () => <section light={1}><span>beam</span></section>;\n",
            ),
            (
                tree_sitter_json::LANGUAGE.into(),
                ShippedLanguage::Json,
                "{\"beacon\": {\"beam\": [1, true, null]}, \"light\": 2}\n",
            ),
            (
                tree_sitter_yaml::LANGUAGE.into(),
                ShippedLanguage::Yaml,
                "beacon:\n  beam: [one, two]\n  light: true\nother:\n  - key: value\n",
            ),
            (
                tree_sitter_toml_ng::LANGUAGE.into(),
                ShippedLanguage::Toml,
                "[beacon]\nbeam = [1, 2]\nlight = true\n[other]\nname = \"value\"\n",
            ),
        ];
        for (grammar, shipped, text) in &fixtures {
            let compared = compared_syntax_elements(grammar, *shipped, text);
            let name = shipped.language().name;
            assert!(
                compared > 10,
                "the oracle must compare the fixture's syntax_elements: language={name}, \
                 compared={compared}"
            );
        }
    }
}
