//! Python syntax facts from the pinned tree-sitter-python grammar.
//!
//! `def` and `class` declare; a decorated definition's span starts at its
//! first decorator, so removing the declaration removes the decorators with
//! it. The grammar parses `async def` as the same `function_definition`
//! kind, so coroutines declare exactly as functions do.
//!
//! Decisions this module fixes:
//! - Qualified names join scopes with `.`, the language's own spelling:
//!   a method files as `Widget.open`, a nested function as `outer.inner`.
//! - An assignment declares a `variable` symbol only at module level or
//!   directly in a class body, and only when its target is one plain
//!   identifier. Function-local assignments, attribute targets, and
//!   destructuring patterns declare nothing: locals would drown the
//!   file's real surface in noise.
//! - A definition's docstring - the body's leading string expression - is
//!   carried as plain-text documentation with the quotes stripped.
//! - Python states no authored visibility, so `visibility` stays `None`;
//!   the leading-underscore convention is a reading habit, not syntax.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_core::Error;
use rift_protocol::read::{Documentation, DocumentationFormat, Language, NodeFacet, SymbolFacet};
use tree_sitter::{Node, Parser};

use crate::document::SyntaxDocument;
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{
    SYNTAX_DEPTH_MAX_DEFAULT, SYNTAX_NODES_MAX_DEFAULT, SyntaxLimits, SyntaxProvider, SyntaxSource,
};

/// Grammar spelling of a function definition, `async def` included.
const FUNCTION_KIND: &str = "function_definition";
/// Grammar spelling of a class definition.
const CLASS_KIND: &str = "class_definition";
/// Grammar spelling of the node wrapping a decorated `def` or `class`.
const DECORATED_KIND: &str = "decorated_definition";
/// Grammar spelling of a suite of statements.
const BLOCK_KIND: &str = "block";
/// Grammar spelling of the module root.
const MODULE_KIND: &str = "module";
/// Grammar spelling of a statement holding one expression.
const EXPRESSION_STATEMENT_KIND: &str = "expression_statement";
/// Grammar spelling of a string literal.
const STRING_KIND: &str = "string";
/// Grammar spelling of a string literal's content between its quotes.
const STRING_CONTENT_KIND: &str = "string_content";
/// Grammar spelling of an assignment.
const ASSIGNMENT_KIND: &str = "assignment";
/// Grammar spelling of a plain identifier.
const IDENTIFIER_KIND: &str = "identifier";
/// Grammar field holding a definition's name.
const NAME_FIELD: &str = "name";
/// Grammar field holding a definition's body suite.
const BODY_FIELD: &str = "body";
/// Grammar field holding an assignment's target.
const LEFT_FIELD: &str = "left";

/// The kind word behind the wire kind `python.function`.
const FUNCTION_KIND_WORD: &str = "function";
/// The kind word behind the wire kind `python.class`.
const CLASS_KIND_WORD: &str = "class";
/// The kind word behind the wire kind `python.variable`.
const VARIABLE_KIND_WORD: &str = "variable";

/// The separator Python qualified names join scopes with.
const SCOPE_QUALIFICATION_SEPARATOR: &str = ".";

/// Numeric grammar ids for every kind and field this module reads, resolved
/// once so each walk decision compares integers.
#[derive(Debug)]
struct PythonKinds {
    function: u16,
    class: u16,
    decorated: u16,
    block: u16,
    module: u16,
    expression_statement: u16,
    string: u16,
    string_content: u16,
    assignment: u16,
    identifier: u16,
    name: NonZeroU16,
    body: NonZeroU16,
    left: NonZeroU16,
}

impl PythonKinds {
    /// Resolves the pinned grammar's declaration vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    fn resolve(language: &tree_sitter::Language) -> Self {
        Self {
            function: kind_id(language, FUNCTION_KIND),
            class: kind_id(language, CLASS_KIND),
            decorated: kind_id(language, DECORATED_KIND),
            block: kind_id(language, BLOCK_KIND),
            module: kind_id(language, MODULE_KIND),
            expression_statement: kind_id(language, EXPRESSION_STATEMENT_KIND),
            string: kind_id(language, STRING_KIND),
            string_content: kind_id(language, STRING_CONTENT_KIND),
            assignment: kind_id(language, ASSIGNMENT_KIND),
            identifier: kind_id(language, IDENTIFIER_KIND),
            name: field_id(language, NAME_FIELD),
            body: field_id(language, BODY_FIELD),
            left: field_id(language, LEFT_FIELD),
        }
    }
}

/// Resolves one node kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned Python grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one grammar field id, proving the pinned grammar defines it.
fn field_id(language: &tree_sitter::Language, field: &str) -> NonZeroU16 {
    language.field_id_for_name(field).unwrap_or_else(|| {
        panic!(
            "pinned Python grammar must define field used by symbol \
             extraction: field={field}"
        )
    })
}

/// The pinned grammar's decisions for the shared bounded walk.
#[derive(Debug)]
struct PythonRules {
    kinds: &'static PythonKinds,
}

impl PythonRules {
    /// The spelled name behind a definition's `name` field; `None` when the
    /// tree is missing it.
    fn field_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        let name = node.child_by_field_id(self.kinds.name.get())?;
        text.get(name.byte_range()).map(str::to_owned)
    }

    /// The definition's docstring: its body suite's leading string
    /// expression, quotes stripped. `None` when the body opens with
    /// anything else.
    fn docstring(&self, node: Node<'_>, text: &str) -> Vec<Documentation> {
        let Some(body) = node.child_by_field_id(self.kinds.body.get()) else {
            return Vec::new();
        };
        let Some(first) = body.named_child(0) else {
            return Vec::new();
        };
        if first.kind_id() != self.kinds.expression_statement {
            return Vec::new();
        }
        let Some(string) = first
            .named_child(0)
            .filter(|expression| expression.kind_id() == self.kinds.string)
        else {
            return Vec::new();
        };
        let mut content = String::new();
        for child_index in 0..string.named_child_count() {
            let Some(child) = string.named_child(child_index) else {
                continue;
            };
            if child.kind_id() == self.kinds.string_content
                && let Some(piece) = text.get(child.byte_range())
            {
                content.push_str(piece);
            }
        }
        if content.is_empty() {
            return Vec::new();
        }
        vec![Documentation {
            format: DocumentationFormat::Plain,
            text: content,
        }]
    }

    /// One definition's declaration facts, shared by `def` and `class`.
    fn definition_declaration(
        &self,
        node: Node<'_>,
        text: &str,
        kind: &'static str,
        facets: Vec<SymbolFacet>,
    ) -> Result<Option<Declaration>, SyntaxError> {
        let Some(name) = self.field_name(node, text) else {
            return Ok(None);
        };
        let body_range = match node.child_by_field_id(self.kinds.body.get()) {
            Some(body) => Some(extract::byte_range(body)?),
            None => None,
        };
        Ok(Some(Declaration {
            name,
            kind,
            facets,
            visibility: None,
            body_range,
            documentation: self.docstring(node, text),
        }))
    }

    /// The declared variable behind an assignment: its one plain-identifier
    /// target, accepted only at module level or directly in a class body.
    fn assignment_declaration(&self, node: Node<'_>, text: &str) -> Option<Declaration> {
        if !self.assignment_scope_accepted(node) {
            return None;
        }
        let target = node.child_by_field_id(self.kinds.left.get())?;
        if target.kind_id() != self.kinds.identifier {
            return None;
        }
        let name = text.get(target.byte_range())?;
        Some(Declaration {
            name: name.to_owned(),
            kind: VARIABLE_KIND_WORD,
            facets: vec![SymbolFacet::Value],
            visibility: None,
            body_range: None,
            documentation: Vec::new(),
        })
    }

    /// Whether an assignment sits at module level or directly in a class
    /// body: its statement's holder is the module root, or a suite whose
    /// definition is a class.
    fn assignment_scope_accepted(&self, node: Node<'_>) -> bool {
        let Some(statement) = node.parent() else {
            return false;
        };
        if statement.kind_id() != self.kinds.expression_statement {
            return false;
        }
        let Some(holder) = statement.parent() else {
            return false;
        };
        if holder.kind_id() == self.kinds.module {
            return true;
        }
        holder.kind_id() == self.kinds.block
            && holder
                .parent()
                .is_some_and(|definition| definition.kind_id() == self.kinds.class)
    }
}

impl GrammarRules for PythonRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        if node.kind_id() == self.kinds.function {
            return self.definition_declaration(
                node,
                text,
                FUNCTION_KIND_WORD,
                vec![SymbolFacet::Value, SymbolFacet::Callable],
            );
        }
        if node.kind_id() == self.kinds.class {
            return self.definition_declaration(
                node,
                text,
                CLASS_KIND_WORD,
                vec![SymbolFacet::Type],
            );
        }
        if node.kind_id() == self.kinds.assignment {
            return Ok(self.assignment_declaration(node, text));
        }
        Ok(None)
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        if node.kind_id() != self.kinds.function && node.kind_id() != self.kinds.class {
            return None;
        }
        self.field_name(node, text)
    }

    /// A decorated definition starts at its first decorator, so the whole
    /// declaration removes with its decorators.
    fn declaration_start(&self, node: Node<'_>, _text: &str) -> usize {
        match node.parent() {
            Some(parent) if parent.kind_id() == self.kinds.decorated => parent.start_byte(),
            _ => node.start_byte(),
        }
    }

    fn qualification_separator(&self) -> &'static str {
        SCOPE_QUALIFICATION_SEPARATOR
    }
}

/// Bounded Tree-sitter Python fact provider.
#[derive(Debug, Clone)]
pub struct PythonSyntaxProvider {
    language: Language,
    limits: SyntaxLimits,
}

impl PythonSyntaxProvider {
    /// Default maximum bytes this provider accepts from one Python source.
    pub const SOURCE_BYTES_MAX_DEFAULT: usize = 4 * 1_024 * 1_024;

    /// Constructs provider with explicit bounds.
    #[must_use]
    pub fn new(limits: SyntaxLimits) -> Self {
        Self {
            language: Language {
                name: "python".to_owned(),
                dialect: None,
            },
            limits,
        }
    }
}

/// The Python provider's declared default bounds, proven positive at
/// compile time.
const PYTHON_SYNTAX_LIMITS_DEFAULT: SyntaxLimits = SyntaxLimits::declared(
    PythonSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT,
    SYNTAX_NODES_MAX_DEFAULT,
    SYNTAX_DEPTH_MAX_DEFAULT,
);

impl Default for PythonSyntaxProvider {
    fn default() -> Self {
        Self::new(PYTHON_SYNTAX_LIMITS_DEFAULT)
    }
}

/// Returns the process-wide resolved kind table, computing it once.
fn python_kinds() -> &'static PythonKinds {
    static KINDS: OnceLock<PythonKinds> = OnceLock::new();
    KINDS.get_or_init(|| PythonKinds::resolve(&tree_sitter_python::LANGUAGE.into()))
}

/// A parser speaking the pinned Python grammar.
fn python_parser() -> Result<Parser, SyntaxError> {
    let grammar = tree_sitter_python::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|_| incompatible_grammar(&grammar))?;
    Ok(parser)
}

impl SyntaxProvider for PythonSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn source_bytes_max(&self) -> usize {
        self.limits.source_bytes_max()
    }

    fn analyze(&self, source: SyntaxSource<'_>) -> Result<SyntaxDocument, SyntaxError> {
        if source.text.len() > self.limits.source_bytes_max() {
            return Err(Error::new(SyntaxFault::SourceTooLarge {
                path: Some(source.path.clone()),
                source_bytes: source.text.len(),
                source_bytes_max: self.limits.source_bytes_max(),
            }));
        }
        let mut parser = python_parser()?;
        let tree = parser.parse(source.text, None).ok_or_else(|| {
            Error::new(SyntaxFault::ParseCancelled {
                path: Some(source.path.clone()),
            })
        })?;
        let rules = PythonRules {
            kinds: python_kinds(),
        };
        let (nodes, symbols) = extract::extract(
            tree.root_node(),
            source,
            self.limits,
            &self.language,
            &rules,
        )?;
        Ok(SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            nodes,
            symbols,
            tree.root_node().has_error(),
        ))
    }

    fn node_facets(&self, kind: &str) -> Vec<NodeFacet> {
        let mut facets = Vec::new();
        if kind.ends_with("_definition") {
            facets.extend([NodeFacet::Declaration, NodeFacet::Definition]);
        }
        if kind == BLOCK_KIND {
            facets.push(NodeFacet::Block);
        }
        if kind.ends_with("_statement") {
            facets.push(NodeFacet::Statement);
        }
        if kind.contains("comment") {
            facets.push(NodeFacet::Comment);
        }
        facets
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;

    fn analyze(text: &str) -> SyntaxDocument {
        let path = ProjectPath::new("src/service.py").expect("valid fixture path");
        PythonSyntaxProvider::default()
            .analyze(SyntaxSource { path: &path, text })
            .expect("Python fixture must parse")
    }

    fn symbol<'document>(
        document: &'document SyntaxDocument,
        qualified_name: &str,
    ) -> &'document crate::document::SyntaxSymbol {
        document
            .symbols()
            .iter()
            .find(|symbol| symbol.qualified_name == qualified_name)
            .unwrap_or_else(|| {
                panic!(
                    "fixture must declare {qualified_name}; declared: {:?}",
                    document
                        .symbols()
                        .iter()
                        .map(|symbol| &symbol.qualified_name)
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn test_functions_and_classes_declare_with_python_kind_words() {
        let document = analyze("def serve():\n    pass\n\nclass Widget:\n    pass\n");
        let function = symbol(&document, "serve");
        assert_eq!(function.kind, "function");
        assert_eq!(function.facets, [SymbolFacet::Value, SymbolFacet::Callable]);
        assert_eq!(function.visibility, None);
        let class = symbol(&document, "Widget");
        assert_eq!(class.kind, "class");
        assert_eq!(class.facets, [SymbolFacet::Type]);
    }

    #[test]
    fn test_async_def_declares_exactly_as_a_function() {
        let document = analyze("async def fetch():\n    pass\n");
        assert_eq!(symbol(&document, "fetch").kind, "function");
    }

    #[test]
    fn test_methods_and_nested_functions_qualify_with_dots() {
        let document = analyze(
            "class Widget:\n    def open(self):\n        def helper():\n            pass\n",
        );
        assert_eq!(symbol(&document, "Widget.open").kind, "function");
        assert_eq!(symbol(&document, "Widget.open.helper").kind, "function");
    }

    #[test]
    fn test_a_decorated_definition_spans_from_its_first_decorator() {
        let text = "@cached\n@retry(2)\ndef serve():\n    pass\n";
        let document = analyze(text);
        let function = symbol(&document, "serve");
        assert_eq!(function.range.start, 0, "the span opens at `@cached`");
        assert_eq!(
            function.item_range.start,
            u64::try_from(text.find("def").expect("fixture spells def"))
                .expect("fixture offset fits"),
            "the item itself starts at `def`"
        );
    }

    #[test]
    fn test_a_callable_signature_is_the_header_before_the_body() {
        let document = analyze("def serve(port: int) -> None:\n    pass\n");
        let function = symbol(&document, "serve");
        assert_eq!(function.signatures.len(), 1);
        assert_eq!(
            function.signatures[0].display,
            "def serve(port: int) -> None:"
        );
    }

    #[test]
    fn test_docstrings_ride_as_plain_documentation_with_quotes_stripped() {
        let document = analyze(
            "def serve():\n    \"\"\"Answers one request.\"\"\"\n    pass\n\
             \n\nclass Widget:\n    'One drawn control.'\n",
        );
        let function = symbol(&document, "serve");
        assert_eq!(
            function.documentation,
            [Documentation {
                format: DocumentationFormat::Plain,
                text: "Answers one request.".to_owned(),
            }]
        );
        let class = symbol(&document, "Widget");
        assert_eq!(class.documentation[0].text, "One drawn control.");
    }

    #[test]
    fn test_a_body_without_a_docstring_carries_no_documentation() {
        let document = analyze("def serve():\n    return 1\n");
        assert_eq!(symbol(&document, "serve").documentation, []);
    }

    #[test]
    fn test_module_and_class_assignments_declare_variables() {
        let document = analyze(
            "LIMIT = 16\n\nclass Widget:\n    retries = 2\n    def open(self):\n        local = 1\n",
        );
        assert_eq!(symbol(&document, "LIMIT").kind, "variable");
        assert_eq!(symbol(&document, "LIMIT").facets, [SymbolFacet::Value]);
        assert_eq!(symbol(&document, "Widget.retries").kind, "variable");
        assert!(
            !document
                .symbols()
                .iter()
                .any(|symbol| symbol.name == "local"),
            "a function-local assignment declares nothing"
        );
    }

    #[test]
    fn test_destructuring_and_attribute_targets_declare_nothing() {
        let document = analyze("a, b = 1, 2\nself.count = 3\n");
        assert!(
            document.symbols().is_empty(),
            "only one plain identifier target declares: {:?}",
            document
                .symbols()
                .iter()
                .map(|symbol| &symbol.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_a_broken_source_still_serves_with_its_error_flag() {
        let document = analyze("def serve(:\n");
        assert!(document.has_errors());
    }

    #[test]
    fn test_a_source_past_the_byte_bound_refuses() {
        let path = ProjectPath::new("src/service.py").expect("valid fixture path");
        let text = "x = 1\n".repeat(1024);
        let provider = PythonSyntaxProvider::new(
            SyntaxLimits::new(16, SYNTAX_NODES_MAX_DEFAULT, SYNTAX_DEPTH_MAX_DEFAULT)
                .expect("positive fixture bounds"),
        );
        let error = provider
            .analyze(SyntaxSource {
                path: &path,
                text: &text,
            })
            .expect_err("a source past the byte bound must refuse");
        assert!(error.to_string().contains("source"), "{error}");
    }

    #[test]
    fn test_node_facets_cover_definitions_blocks_statements_and_comments() {
        let provider = PythonSyntaxProvider::default();
        assert_eq!(
            provider.node_facets("function_definition"),
            [NodeFacet::Declaration, NodeFacet::Definition]
        );
        assert_eq!(provider.node_facets("block"), [NodeFacet::Block]);
        assert_eq!(
            provider.node_facets("expression_statement"),
            [NodeFacet::Statement]
        );
        assert_eq!(provider.node_facets("comment"), [NodeFacet::Comment]);
        assert_eq!(provider.node_facets("identifier"), []);
    }

    #[test]
    fn test_provider_declares_language_and_byte_bound() {
        let provider = PythonSyntaxProvider::default();
        assert_eq!(provider.language().name, "python");
        assert_eq!(provider.language().dialect, None);
        assert_eq!(
            provider.source_bytes_max(),
            PythonSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT
        );
    }
}
