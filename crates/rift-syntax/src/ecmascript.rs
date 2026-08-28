//! The ECMAScript declaration rules the JavaScript and TypeScript providers share.
//!
//! tree-sitter-typescript extends tree-sitter-javascript, so one rules
//! module reads all three pinned grammars: the JavaScript grammar resolves
//! the core declaration kinds, and the two TypeScript grammars extend that
//! table with their own. Kind and field ids are resolved once per grammar
//! ([`EcmaScriptKinds`]); the walk compares integers.
//!
//! Decisions this module fixes for the family:
//! - Qualified names join with `.`, the member access spelling
//!   (`Router.route`), the way `rust` joins with `::`.
//! - An `export` statement wrapping a declaration adds the `Public` facet.
//!   `export` is not a visibility spelling: the `visibility` field carries
//!   only an authored `accessibility_modifier` (`public`, `private`,
//!   `protected`), and stays `None` everywhere else.
//! - A `variable_declarator` under a `lexical_declaration` or
//!   `variable_declaration` declares a `variable`; an arrow function
//!   assigned to one is that variable, named by the declarator. A
//!   destructuring declarator declares its names through a pattern, not a
//!   single name, and emits no symbol.
//! - A declaration's complete span includes directly attached `JSDoc` comments.
//!   Decorators remain children of the declared node.
//! - `documentation` remains empty. A `Signature` renders for a callable
//!   declaration from `body_range` and the `Callable` facet.

use std::num::NonZeroU16;

use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet, SymbolFacet};
use tree_sitter::{Node, Parser};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, Declaration, GrammarRules};
use crate::failure::{SyntaxError, SyntaxFault, incompatible_grammar};
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Grammar spelling of a `function_declaration`.
const FUNCTION_DECLARATION_KIND: &str = "function_declaration";
/// Grammar spelling of a `generator_function_declaration`.
const GENERATOR_FUNCTION_DECLARATION_KIND: &str = "generator_function_declaration";
/// Grammar spelling of a `class_declaration`.
const CLASS_DECLARATION_KIND: &str = "class_declaration";
/// Grammar spelling of a `method_definition`.
const METHOD_DEFINITION_KIND: &str = "method_definition";
/// Grammar spelling of a `variable_declarator`.
const VARIABLE_DECLARATOR_KIND: &str = "variable_declarator";
/// Grammar spelling of a `let`/`const` declaration statement.
const LEXICAL_DECLARATION_KIND: &str = "lexical_declaration";
/// Grammar spelling of a `var` declaration statement.
const VARIABLE_DECLARATION_KIND: &str = "variable_declaration";
/// Grammar spelling of an `export_statement`.
const EXPORT_STATEMENT_KIND: &str = "export_statement";
/// Grammar spelling of a plain `identifier`.
const IDENTIFIER_KIND: &str = "identifier";
/// Grammar spelling of an `interface_declaration` (TypeScript grammars only).
const INTERFACE_DECLARATION_KIND: &str = "interface_declaration";
/// Grammar spelling of an `enum_declaration` (TypeScript grammars only).
const ENUM_DECLARATION_KIND: &str = "enum_declaration";
/// Grammar spelling of a `type_alias_declaration` (TypeScript grammars only).
const TYPE_ALIAS_DECLARATION_KIND: &str = "type_alias_declaration";
/// Grammar spelling of a `namespace` block, `internal_module` in the
/// TypeScript grammars.
const INTERNAL_MODULE_KIND: &str = "internal_module";
/// Grammar spelling of a bodyless `function_signature` (TypeScript grammars
/// only).
const FUNCTION_SIGNATURE_KIND: &str = "function_signature";
/// Grammar spelling of an `accessibility_modifier` (TypeScript grammars
/// only).
const ACCESSIBILITY_MODIFIER_KIND: &str = "accessibility_modifier";

/// ECMAScript declaration kind emitted by the JavaScript and TypeScript
/// providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EcmaScriptSymbolKind {
    /// Function declaration, generator, or bodyless signature.
    Function,
    /// Class.
    Class,
    /// Method inside a class body.
    Method,
    /// Named variable declarator.
    Variable,
    /// Interface (TypeScript).
    Interface,
    /// Enumeration (TypeScript).
    Enum,
    /// Type alias (TypeScript).
    TypeAlias,
    /// Namespace (TypeScript).
    Namespace,
}

impl EcmaScriptSymbolKind {
    /// The provider kind word behind the wire kind `{language}.{word}`.
    const fn word(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Class => "class",
            Self::Method => "method",
            Self::Variable => "variable",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::TypeAlias => "type_alias",
            Self::Namespace => "namespace",
        }
    }

    /// Portable facets for this kind, before an export adds `Public`.
    fn facets(self) -> Vec<SymbolFacet> {
        match self {
            Self::Function | Self::Method => vec![SymbolFacet::Value, SymbolFacet::Callable],
            Self::Class | Self::Interface | Self::Enum => vec![SymbolFacet::Type],
            Self::TypeAlias => vec![SymbolFacet::Type, SymbolFacet::Alias],
            Self::Variable => vec![SymbolFacet::Value],
            Self::Namespace => vec![SymbolFacet::Namespace],
        }
    }

    /// The grammar field spanning this kind's implementation part. Every
    /// kind declares one; a node that omits it - a bodyless signature, a
    /// valueless declarator - carries no body range.
    const fn body_field(self) -> EcmaScriptGrammarField {
        match self {
            Self::Function
            | Self::Class
            | Self::Method
            | Self::Interface
            | Self::Enum
            | Self::Namespace => EcmaScriptGrammarField::Body,
            Self::Variable | Self::TypeAlias => EcmaScriptGrammarField::Value,
        }
    }

    /// Whether declarations inside this kind's body qualify under its name.
    const fn opens_scope(self) -> bool {
        matches!(self, Self::Class | Self::Namespace)
    }
}

/// Grammar field this module reads, common to all three pinned grammars.
#[derive(Debug, Clone, Copy)]
enum EcmaScriptGrammarField {
    /// `name` field on declaration nodes.
    Name,
    /// `body` field on block-bodied declaration nodes.
    Body,
    /// `value` field on `variable_declarator` and `type_alias_declaration`.
    Value,
}

/// Numeric grammar ids for every kind and field this module reads, resolved
/// once per pinned grammar so each walk decision compares integers.
#[derive(Debug)]
pub(crate) struct EcmaScriptKinds {
    /// Declaration table: the grammar kind id beside the symbol kind it
    /// declares. The two TypeScript grammars extend the JavaScript core.
    declarations: Vec<(u16, EcmaScriptSymbolKind)>,
    lexical_declaration: u16,
    variable_declaration: u16,
    export_statement: u16,
    identifier: u16,
    /// `Some` on the TypeScript grammars; the JavaScript grammar spells no
    /// accessibility.
    accessibility_modifier: Option<u16>,
    name: NonZeroU16,
    body: NonZeroU16,
    value: NonZeroU16,
}

impl EcmaScriptKinds {
    /// Resolves the JavaScript grammar's declaration vocabulary.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    pub(crate) fn resolve_javascript(language: &tree_sitter::Language) -> Self {
        Self {
            declarations: vec![
                (
                    kind_id(language, FUNCTION_DECLARATION_KIND),
                    EcmaScriptSymbolKind::Function,
                ),
                (
                    kind_id(language, GENERATOR_FUNCTION_DECLARATION_KIND),
                    EcmaScriptSymbolKind::Function,
                ),
                (
                    kind_id(language, CLASS_DECLARATION_KIND),
                    EcmaScriptSymbolKind::Class,
                ),
                (
                    kind_id(language, METHOD_DEFINITION_KIND),
                    EcmaScriptSymbolKind::Method,
                ),
                (
                    kind_id(language, VARIABLE_DECLARATOR_KIND),
                    EcmaScriptSymbolKind::Variable,
                ),
            ],
            lexical_declaration: kind_id(language, LEXICAL_DECLARATION_KIND),
            variable_declaration: kind_id(language, VARIABLE_DECLARATION_KIND),
            export_statement: kind_id(language, EXPORT_STATEMENT_KIND),
            identifier: kind_id(language, IDENTIFIER_KIND),
            accessibility_modifier: None,
            name: field_id(language, "name"),
            body: field_id(language, "body"),
            value: field_id(language, "value"),
        }
    }

    /// Resolves one TypeScript grammar's declaration vocabulary: the
    /// JavaScript core extended with the TypeScript-only kinds.
    ///
    /// # Panics
    ///
    /// Panics when the pinned grammar no longer defines a kind or field this
    /// module depends on - a grammar-version error, not a reachable
    /// operating state.
    pub(crate) fn resolve_typescript(language: &tree_sitter::Language) -> Self {
        let mut kinds = Self::resolve_javascript(language);
        kinds.declarations.extend([
            (
                kind_id(language, INTERFACE_DECLARATION_KIND),
                EcmaScriptSymbolKind::Interface,
            ),
            (
                kind_id(language, ENUM_DECLARATION_KIND),
                EcmaScriptSymbolKind::Enum,
            ),
            (
                kind_id(language, TYPE_ALIAS_DECLARATION_KIND),
                EcmaScriptSymbolKind::TypeAlias,
            ),
            (
                kind_id(language, INTERNAL_MODULE_KIND),
                EcmaScriptSymbolKind::Namespace,
            ),
            (
                kind_id(language, FUNCTION_SIGNATURE_KIND),
                EcmaScriptSymbolKind::Function,
            ),
        ]);
        kinds.accessibility_modifier = Some(kind_id(language, ACCESSIBILITY_MODIFIER_KIND));
        kinds
    }

    /// The symbol kind `node` declares; `None` for a kind outside the table.
    fn symbol_kind(&self, node: Node<'_>) -> Option<EcmaScriptSymbolKind> {
        let id = node.kind_id();
        self.declarations
            .iter()
            .find(|(kind, _)| *kind == id)
            .map(|(_, symbol)| *symbol)
    }

    const fn field(&self, field: EcmaScriptGrammarField) -> NonZeroU16 {
        match field {
            EcmaScriptGrammarField::Name => self.name,
            EcmaScriptGrammarField::Body => self.body,
            EcmaScriptGrammarField::Value => self.value,
        }
    }
}

/// Resolves one node kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned ECMAScript grammar must define node kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one grammar field id, proving the pinned grammar defines it.
fn field_id(language: &tree_sitter::Language, field: &str) -> NonZeroU16 {
    language.field_id_for_name(field).unwrap_or_else(|| {
        panic!(
            "pinned ECMAScript grammar must define field used by symbol \
             extraction: field={field}"
        )
    })
}

/// One pinned grammar's decisions for the shared bounded walk.
#[derive(Debug)]
pub(crate) struct EcmaScriptRules {
    kinds: &'static EcmaScriptKinds,
}

impl EcmaScriptRules {
    /// The declared name's text: the grammar `name` field. A `variable`
    /// requires a plain identifier name; a destructuring pattern declares no
    /// single name.
    fn declaration_name(
        &self,
        node: Node<'_>,
        kind: EcmaScriptSymbolKind,
        text: &str,
    ) -> Option<String> {
        let name = node.child_by_field_id(self.kinds.field(EcmaScriptGrammarField::Name).get())?;
        if kind == EcmaScriptSymbolKind::Variable && name.kind_id() != self.kinds.identifier {
            return None;
        }
        text.get(name.byte_range()).map(Into::into)
    }

    /// Whether an `export_statement` wraps the declaration: its direct
    /// parent, or - for a declarator - the parent of its declaration
    /// statement.
    fn exported(&self, node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind_id() == self.kinds.export_statement {
            return true;
        }
        let declaration_statement = parent.kind_id() == self.kinds.lexical_declaration
            || parent.kind_id() == self.kinds.variable_declaration;
        declaration_statement
            && parent
                .parent()
                .is_some_and(|wrapper| wrapper.kind_id() == self.kinds.export_statement)
    }

    /// The authored `accessibility_modifier` text on `node`; `None` when the
    /// grammar spells none or the declaration carries none.
    fn accessibility(&self, node: Node<'_>, text: &str) -> Option<String> {
        let modifier = self.kinds.accessibility_modifier?;
        (0..node.named_child_count())
            .filter_map(|index| node.named_child(index))
            .find(|child| child.kind_id() == modifier)
            .and_then(|child| text.get(child.byte_range()))
            .map(Into::into)
    }

    /// The kind's body or value field span; `None` when this node omits the
    /// field - a bodyless signature, a valueless declarator.
    fn body_range(
        &self,
        node: Node<'_>,
        kind: EcmaScriptSymbolKind,
    ) -> Result<Option<ByteRange>, SyntaxError> {
        let Some(body) = node.child_by_field_id(self.kinds.field(kind.body_field()).get()) else {
            return Ok(None);
        };
        extract::byte_range(body).map(Some)
    }
}

impl GrammarRules for EcmaScriptRules {
    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        let Some(kind) = self.kinds.symbol_kind(node) else {
            return Ok(None);
        };
        let Some(name) = self.declaration_name(node, kind, text) else {
            return Ok(None);
        };
        let mut facets = kind.facets();
        if self.exported(node) {
            facets.push(SymbolFacet::Public);
        }
        Ok(Some(Declaration {
            name,
            kind: kind.word(),
            facets,
            visibility: self.accessibility(node, text),
            body_range: self.body_range(node, kind)?,
            documentation: Vec::new(),
        }))
    }

    fn container_name(&self, node: Node<'_>, text: &str) -> Option<String> {
        let kind = self.kinds.symbol_kind(node)?;
        if !kind.opens_scope() {
            return None;
        }
        let name = node.child_by_field_id(self.kinds.field(EcmaScriptGrammarField::Name).get())?;
        text.get(name.byte_range()).map(Into::into)
    }

    /// Extends declaration start over directly attached `JSDoc` comments.
    fn declaration_start(&self, node: Node<'_>, text: &str) -> usize {
        let mut front = node;
        while let Some(previous) = front.prev_sibling() {
            if previous.kind() != "comment" {
                break;
            }
            let Some(comment) = text.get(previous.byte_range()) else {
                break;
            };
            if !comment.trim_start().starts_with("/**") {
                break;
            }
            let Some(gap) = text.get(previous.end_byte()..front.start_byte()) else {
                break;
            };
            if !gap.chars().all(char::is_whitespace)
                || gap.bytes().filter(|byte| *byte == b'\n').count() > 1
            {
                break;
            }
            front = previous;
        }
        front.start_byte()
    }

    fn qualification_separator(&self) -> &'static str {
        "."
    }
}

/// Parses one source through a pinned ECMAScript grammar and extracts its
/// named nodes and declarations, the shared `analyze` behind all three
/// providers.
///
/// # Errors
///
/// Returns [`SyntaxError`] for an oversized source, an incompatible
/// grammar, cancellation, or an exceeded tree bound.
pub(crate) fn analyze(
    language: &Language,
    grammar: &tree_sitter::Language,
    kinds: &'static EcmaScriptKinds,
    limits: SyntaxLimits,
    source: SyntaxSource<'_>,
) -> Result<SyntaxDocument, SyntaxError> {
    if source.text.len() > limits.source_bytes_max() {
        return Err(Error::new(SyntaxFault::SourceTooLarge {
            path: Some(source.path.clone()),
            source_bytes: source.text.len(),
            source_bytes_max: limits.source_bytes_max(),
        }));
    }
    let mut parser = Parser::new();
    parser
        .set_language(grammar)
        .map_err(|_| incompatible_grammar(grammar))?;
    let tree = parser.parse(source.text, None).ok_or_else(|| {
        Error::new(SyntaxFault::ParseCancelled {
            path: Some(source.path.clone()),
        })
    })?;
    let rules = EcmaScriptRules { kinds };
    let (nodes, symbols) = extract::extract(tree.root_node(), source, limits, language, &rules)?;
    Ok(SyntaxDocument::new(
        language.clone(),
        source.path.clone(),
        nodes,
        symbols,
        tree.root_node().has_error(),
    ))
}

/// Portable structural facets for one ECMAScript grammar node kind, shared
/// by all three providers: every declaration kind the rules interpret maps
/// to `Declaration`, and the grammar's suffix conventions classify the rest.
pub(crate) fn node_facets(kind: &str) -> Vec<NodeFacet> {
    match kind {
        FUNCTION_DECLARATION_KIND
        | GENERATOR_FUNCTION_DECLARATION_KIND
        | CLASS_DECLARATION_KIND
        | METHOD_DEFINITION_KIND
        | VARIABLE_DECLARATOR_KIND
        | INTERFACE_DECLARATION_KIND
        | ENUM_DECLARATION_KIND
        | TYPE_ALIAS_DECLARATION_KIND
        | INTERNAL_MODULE_KIND => vec![NodeFacet::Declaration, NodeFacet::Definition],
        FUNCTION_SIGNATURE_KIND => vec![NodeFacet::Declaration],
        LEXICAL_DECLARATION_KIND | VARIABLE_DECLARATION_KIND => {
            vec![NodeFacet::Declaration, NodeFacet::Statement]
        }
        EXPORT_STATEMENT_KIND => vec![NodeFacet::Export, NodeFacet::Statement],
        "import_statement" => vec![NodeFacet::Import, NodeFacet::Statement],
        "statement_block" => vec![NodeFacet::Block],
        "class_body" | "interface_body" | "enum_body" => vec![NodeFacet::Body],
        "required_parameter" | "optional_parameter" => vec![NodeFacet::Parameter],
        "decorator" => vec![NodeFacet::Annotation],
        "comment" | "html_comment" => vec![NodeFacet::Comment],
        "type_annotation" => vec![NodeFacet::TypeExpression],
        "arrow_function" | "jsx_element" | "jsx_self_closing_element" => {
            vec![NodeFacet::Expression]
        }
        suffixed => suffix_facets(suffixed),
    }
}

/// The grammar's spelling conventions for kinds outside the named table.
fn suffix_facets(kind: &str) -> Vec<NodeFacet> {
    let mut facets = Vec::new();
    if kind.ends_with("_statement") {
        facets.push(NodeFacet::Statement);
    }
    if kind.ends_with("_expression") {
        facets.push(NodeFacet::Expression);
    }
    if kind.ends_with("_type") {
        facets.push(NodeFacet::TypeExpression);
    }
    facets
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution asserts every kind and field id is non-zero, so resolving
    /// each pinned grammar's table is the proof the vocabulary exists.
    #[test]
    fn test_kind_tables_resolve_on_every_pinned_grammar() {
        let javascript =
            EcmaScriptKinds::resolve_javascript(&tree_sitter_javascript::LANGUAGE.into());
        assert_eq!(javascript.declarations.len(), 5);
        assert!(javascript.accessibility_modifier.is_none());

        for grammar in [
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            tree_sitter_typescript::LANGUAGE_TSX,
        ] {
            let typescript = EcmaScriptKinds::resolve_typescript(&grammar.into());
            assert_eq!(typescript.declarations.len(), 10);
            assert!(typescript.accessibility_modifier.is_some());
        }
    }

    #[test]
    #[should_panic(expected = "must define node kind used by symbol extraction: \
                               kind=interface_declaration")]
    fn test_typescript_table_refuses_a_grammar_without_the_typescript_kinds() {
        let _ = EcmaScriptKinds::resolve_typescript(&tree_sitter_javascript::LANGUAGE.into());
    }

    /// Every kind word behind the wire kind `{language}.{word}`, pinned.
    #[test]
    fn test_kind_words_are_the_wire_spellings() {
        let words = [
            (EcmaScriptSymbolKind::Function, "function"),
            (EcmaScriptSymbolKind::Class, "class"),
            (EcmaScriptSymbolKind::Method, "method"),
            (EcmaScriptSymbolKind::Variable, "variable"),
            (EcmaScriptSymbolKind::Interface, "interface"),
            (EcmaScriptSymbolKind::Enum, "enum"),
            (EcmaScriptSymbolKind::TypeAlias, "type_alias"),
            (EcmaScriptSymbolKind::Namespace, "namespace"),
        ];
        for (kind, word) in words {
            assert_eq!(kind.word(), word);
        }
    }

    /// Every declaration kind the rules interpret carries the `Declaration`
    /// facet, so the node table stays exhaustive over the interpreted
    /// vocabulary.
    #[test]
    fn test_node_facets_classify_every_interpreted_declaration_kind() {
        for kind in [
            FUNCTION_DECLARATION_KIND,
            GENERATOR_FUNCTION_DECLARATION_KIND,
            CLASS_DECLARATION_KIND,
            METHOD_DEFINITION_KIND,
            VARIABLE_DECLARATOR_KIND,
            INTERFACE_DECLARATION_KIND,
            ENUM_DECLARATION_KIND,
            TYPE_ALIAS_DECLARATION_KIND,
            INTERNAL_MODULE_KIND,
            FUNCTION_SIGNATURE_KIND,
            LEXICAL_DECLARATION_KIND,
            VARIABLE_DECLARATION_KIND,
        ] {
            assert!(
                node_facets(kind).contains(&NodeFacet::Declaration),
                "kind {kind} must classify as a declaration"
            );
        }
    }

    #[test]
    fn test_node_facets_classify_structure_boundaries_and_suffix_conventions() {
        assert_eq!(
            node_facets("export_statement"),
            [NodeFacet::Export, NodeFacet::Statement]
        );
        assert_eq!(
            node_facets("import_statement"),
            [NodeFacet::Import, NodeFacet::Statement]
        );
        assert_eq!(node_facets("statement_block"), [NodeFacet::Block]);
        assert_eq!(node_facets("class_body"), [NodeFacet::Body]);
        assert_eq!(node_facets("interface_body"), [NodeFacet::Body]);
        assert_eq!(node_facets("enum_body"), [NodeFacet::Body]);
        assert_eq!(node_facets("required_parameter"), [NodeFacet::Parameter]);
        assert_eq!(node_facets("decorator"), [NodeFacet::Annotation]);
        assert_eq!(node_facets("comment"), [NodeFacet::Comment]);
        assert_eq!(node_facets("type_annotation"), [NodeFacet::TypeExpression]);
        assert_eq!(node_facets("jsx_element"), [NodeFacet::Expression]);
        assert_eq!(node_facets("return_statement"), [NodeFacet::Statement]);
        assert_eq!(node_facets("binary_expression"), [NodeFacet::Expression]);
        assert_eq!(node_facets("predefined_type"), [NodeFacet::TypeExpression]);
        assert_eq!(node_facets("identifier"), []);
    }
}
