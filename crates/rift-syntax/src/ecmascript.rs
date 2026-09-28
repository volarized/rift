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
//! - The `Public` facet marks a declaration the module exports. An `export`
//!   statement wrapping a declaration adds it, and so does an export naming a
//!   module-scope declaration: `a` and `b` in `export { a, b as c }`, `a` in
//!   `export default a`, and the declarations a `module.exports = a`,
//!   `module.exports = { a, b: c }`, `module.exports.name = a`, or
//!   `exports.name = a` assignment names. The declaration keeps its own
//!   name. A method written in an object literal the module exports whole
//!   (`export default { .. }`, `module.exports = { .. }`) is exported too.
//! - Exports are read from the module's top-level statements: an assignment
//!   nested in a block or a function marks nothing. A re-export
//!   (`export { x } from './y'`, `export * from './y'`) names another
//!   module's declarations, so it marks none here. A function or class
//!   expression assigned to an export (`exports.run = function () {}`)
//!   declares nothing, so nothing carries the facet for it.
//! - `export` is not a visibility spelling: the `visibility` field carries
//!   only an authored `accessibility_modifier` (`public`, `private`,
//!   `protected`), and stays `None` everywhere else.
//! - A `variable_declarator` under a `lexical_declaration` or
//!   `variable_declaration` declares a `variable`; an arrow function
//!   assigned to one is that variable, named by the declarator. A
//!   destructuring declarator declares its names through a pattern, not a
//!   single name, and emits no symbol.
//! - A computed member key names its method by the key expression's own
//!   bytes, so a name can run to any length. A declaration whose name or
//!   qualified name passes `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes is left out
//!   of the document with every declaration nested under it, and counted.
//! - A declaration's complete span includes directly attached `JSDoc` comments.
//!   Decorators remain children of the declared node.
//! - A `method_signature` or `property_signature` declares a `method` or a
//!   `property` only inside an interface or class body, qualified by it
//!   (`Array.map`); the same kinds inside a type literal (`{ size: number }`)
//!   declare nothing.
//! - `documentation` remains empty. A `Signature` renders for a callable
//!   declaration from `body_range` and the `Callable` facet, and a bodyless
//!   one, an overload or a member signature, renders its own text.

use std::collections::BTreeSet;
use std::num::NonZeroU16;

use rift_core::Error;
use rift_protocol::read::{Language, NodeFacet, SymbolFacet};
use tree_sitter::{Node, Parser};

use crate::document::{ByteRange, SyntaxDocument};
use crate::extract::{self, ChildIndices, Declaration, GrammarRules};
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
/// Grammar spelling of a bodyless `method_signature` (TypeScript grammars
/// only).
const METHOD_SIGNATURE_KIND: &str = "method_signature";
/// Grammar spelling of a `property_signature` (TypeScript grammars only).
const PROPERTY_SIGNATURE_KIND: &str = "property_signature";
/// Grammar spelling of an interface's `object_type` body (TypeScript
/// grammars only).
const INTERFACE_BODY_KIND: &str = "interface_body";
/// Grammar spelling of a `class_body`.
const CLASS_BODY_KIND: &str = "class_body";
/// Grammar spelling of the root `program`, whose statements sit at module
/// scope.
const PROGRAM_KIND: &str = "program";
/// Grammar spelling of an `export_clause`, the braces of `export { a, b as c }`.
const EXPORT_CLAUSE_KIND: &str = "export_clause";
/// Grammar spelling of an `expression_statement`.
const EXPRESSION_STATEMENT_KIND: &str = "expression_statement";
/// Grammar spelling of an `assignment_expression`.
const ASSIGNMENT_EXPRESSION_KIND: &str = "assignment_expression";
/// Grammar spelling of a `member_expression`, such as `module.exports`.
const MEMBER_EXPRESSION_KIND: &str = "member_expression";
/// Grammar spelling of an `object` literal.
const OBJECT_KIND: &str = "object";
/// Grammar spelling of a `shorthand_property_identifier`, `a` in `{ a }`.
const SHORTHAND_PROPERTY_IDENTIFIER_KIND: &str = "shorthand_property_identifier";
/// Grammar spelling of a `pair`, `b: c` in `{ b: c }`.
const PAIR_KIND: &str = "pair";
/// The variable naming the running module, `module` in `module.exports`.
const MODULE_VARIABLE: &str = "module";
/// The property of `module`, and the variable, holding a module's exports:
/// `module.exports`, `exports.name`.
const EXPORTS_NAME: &str = "exports";

/// ECMAScript declaration kind emitted by the JavaScript and TypeScript
/// providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EcmaScriptSymbolKind {
    /// Function declaration, generator, or bodyless signature.
    Function,
    /// Class.
    Class,
    /// Method inside a class body, or a method signature inside an interface
    /// or class body (TypeScript).
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
    /// Property signature inside an interface or class body (TypeScript).
    Property,
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
            Self::Property => "property",
        }
    }

    /// Portable facets for this kind, before an export adds `Public`.
    fn facets(self) -> Vec<SymbolFacet> {
        match self {
            Self::Function | Self::Method => vec![SymbolFacet::Value, SymbolFacet::Callable],
            Self::Class | Self::Interface | Self::Enum => vec![SymbolFacet::Type],
            Self::TypeAlias => vec![SymbolFacet::Type, SymbolFacet::Alias],
            Self::Variable | Self::Property => vec![SymbolFacet::Value],
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
            Self::Variable | Self::TypeAlias | Self::Property => EcmaScriptGrammarField::Value,
        }
    }

    /// Whether declarations inside this kind's body qualify under its name.
    const fn opens_scope(self) -> bool {
        matches!(self, Self::Class | Self::Namespace | Self::Interface)
    }
}

/// Grammar field this module reads, common to all three pinned grammars.
#[derive(Debug, Clone, Copy)]
enum EcmaScriptGrammarField {
    /// `name` field on declaration nodes.
    Name,
    /// `body` field on block-bodied declaration nodes.
    Body,
    /// `value` field on `variable_declarator`, `type_alias_declaration`,
    /// `pair`, and a default `export_statement`.
    Value,
    /// `source` field on an `export_statement` that re-exports another module.
    Source,
    /// `left` field on an `assignment_expression`.
    Left,
    /// `right` field on an `assignment_expression`.
    Right,
    /// `object` field on a `member_expression`, `module` in `module.exports`.
    Object,
    /// `property` field on a `member_expression`, `exports` in `module.exports`.
    Property,
}

/// What an assignment's left side exports, ordered so an assignment chain
/// keeps the widest target it passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ExportTarget {
    /// `module.exports.name` or `exports.name`: one export, taken from a
    /// declaration the value names.
    Property,
    /// `module.exports`: the module's whole export, a declaration name or an
    /// object literal.
    Module,
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
    /// Member signature kinds: declarations only inside an interface or a
    /// class body, since a type literal spells them too. Empty on the
    /// JavaScript grammar.
    member_signatures: Vec<u16>,
    /// The bodies a member signature declares in: `interface_body` and
    /// `class_body`. Empty on the JavaScript grammar.
    member_bodies: Vec<u16>,
    program: u16,
    export_clause: u16,
    expression_statement: u16,
    assignment_expression: u16,
    member_expression: u16,
    object_literal: u16,
    shorthand_property_identifier: u16,
    pair: u16,
    name: NonZeroU16,
    body: NonZeroU16,
    value: NonZeroU16,
    source: NonZeroU16,
    left: NonZeroU16,
    right: NonZeroU16,
    object: NonZeroU16,
    property: NonZeroU16,
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
            member_signatures: Vec::new(),
            member_bodies: Vec::new(),
            program: kind_id(language, PROGRAM_KIND),
            export_clause: kind_id(language, EXPORT_CLAUSE_KIND),
            expression_statement: kind_id(language, EXPRESSION_STATEMENT_KIND),
            assignment_expression: kind_id(language, ASSIGNMENT_EXPRESSION_KIND),
            member_expression: kind_id(language, MEMBER_EXPRESSION_KIND),
            object_literal: kind_id(language, OBJECT_KIND),
            shorthand_property_identifier: kind_id(language, SHORTHAND_PROPERTY_IDENTIFIER_KIND),
            pair: kind_id(language, PAIR_KIND),
            name: field_id(language, "name"),
            body: field_id(language, "body"),
            value: field_id(language, "value"),
            source: field_id(language, "source"),
            left: field_id(language, "left"),
            right: field_id(language, "right"),
            object: field_id(language, "object"),
            property: field_id(language, "property"),
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
        let member_signatures = [
            (METHOD_SIGNATURE_KIND, EcmaScriptSymbolKind::Method),
            (PROPERTY_SIGNATURE_KIND, EcmaScriptSymbolKind::Property),
        ]
        .map(|(kind, symbol)| (kind_id(language, kind), symbol));
        kinds.declarations.extend(member_signatures);
        kinds.member_signatures = member_signatures.map(|(kind, _)| kind).to_vec();
        kinds.member_bodies = vec![
            kind_id(language, INTERFACE_BODY_KIND),
            kind_id(language, CLASS_BODY_KIND),
        ];
        kinds.accessibility_modifier = Some(kind_id(language, ACCESSIBILITY_MODIFIER_KIND));
        kinds
    }

    /// Whether `node` is a member signature outside an interface or class
    /// body, such as `{ size: number }` in a type annotation.
    fn is_type_literal_member(&self, node: Node<'_>) -> bool {
        self.member_signatures.contains(&node.kind_id())
            && !node
                .parent()
                .is_some_and(|parent| self.member_bodies.contains(&parent.kind_id()))
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
            EcmaScriptGrammarField::Source => self.source,
            EcmaScriptGrammarField::Left => self.left,
            EcmaScriptGrammarField::Right => self.right,
            EcmaScriptGrammarField::Object => self.object,
            EcmaScriptGrammarField::Property => self.property,
        }
    }

    /// `node`'s child in `field`; `None` when the node omits it.
    fn child<'tree>(
        &self,
        node: Node<'tree>,
        field: EcmaScriptGrammarField,
    ) -> Option<Node<'tree>> {
        node.child_by_field_id(self.field(field).get())
    }

    /// Whether `node` is a `let`, `const`, or `var` statement holding
    /// declarators.
    fn is_declaration_statement(&self, node: Node<'_>) -> bool {
        node.kind_id() == self.lexical_declaration || node.kind_id() == self.variable_declaration
    }

    /// Whether `node` is the plain identifier `spelling`.
    fn is_identifier(&self, node: Node<'_>, text: &str, spelling: &str) -> bool {
        node.kind_id() == self.identifier && text.get(node.byte_range()) == Some(spelling)
    }

    /// The nodes one top-level statement exports without wrapping a
    /// declaration: an identifier naming a local declaration, or an object
    /// literal the module exports whole.
    fn statement_exports<'tree>(&self, statement: Node<'tree>, text: &str) -> Vec<Node<'tree>> {
        if statement.kind_id() == self.export_statement {
            return self.export_statement_exports(statement);
        }
        self.assignment_export(statement, text)
            .into_iter()
            .collect()
    }

    /// The local names an `export` statement's clause names (`a` in
    /// `export { a as b }`) and its default value (`a` in `export default a`).
    /// A re-export (`export { x } from './y'`, `export * from './y'`) carries
    /// a `source` and names another module's declarations, so it yields none.
    fn export_statement_exports<'tree>(&self, statement: Node<'tree>) -> Vec<Node<'tree>> {
        if self
            .child(statement, EcmaScriptGrammarField::Source)
            .is_some()
        {
            return Vec::new();
        }
        let clauses =
            named_children(statement).filter(|child| child.kind_id() == self.export_clause);
        let specifiers = clauses.flat_map(named_children);
        specifiers
            .filter_map(|specifier| self.child(specifier, EcmaScriptGrammarField::Name))
            .chain(self.child(statement, EcmaScriptGrammarField::Value))
            .collect()
    }

    /// The value an assignment statement exports: `module.exports = value`,
    /// `module.exports.name = value`, or `exports.name = value`, following a
    /// chain such as `exports = module.exports = value` to its value. A
    /// property export yields a declaration name alone; `None` for any other
    /// statement.
    ///
    /// The chain walk descends one child per step, so it ends within the
    /// tree's depth.
    fn assignment_export<'tree>(&self, statement: Node<'tree>, text: &str) -> Option<Node<'tree>> {
        if statement.kind_id() != self.expression_statement {
            return None;
        }
        let mut value = statement.named_child(0)?;
        let mut target = None;
        while value.kind_id() == self.assignment_expression {
            let left = self.child(value, EcmaScriptGrammarField::Left)?;
            target = target.max(self.export_target(left, text));
            value = self.child(value, EcmaScriptGrammarField::Right)?;
        }
        match target? {
            ExportTarget::Module => Some(value),
            ExportTarget::Property => (value.kind_id() == self.identifier).then_some(value),
        }
    }

    /// The export an assignment's left side addresses; `None` for any other
    /// target.
    fn export_target(&self, left: Node<'_>, text: &str) -> Option<ExportTarget> {
        if self.is_module_exports(left, text) {
            return Some(ExportTarget::Module);
        }
        let object = self.member_object(left)?;
        let exports_variable = self.is_identifier(object, text, EXPORTS_NAME);
        let module_exports = self.is_module_exports(object, text);
        (exports_variable || module_exports).then_some(ExportTarget::Property)
    }

    /// Whether `node` spells `module.exports`.
    fn is_module_exports(&self, node: Node<'_>, text: &str) -> bool {
        let Some(object) = self.member_object(node) else {
            return false;
        };
        let module_variable = self.is_identifier(object, text, MODULE_VARIABLE);
        let exports_property = self
            .child(node, EcmaScriptGrammarField::Property)
            .and_then(|property| text.get(property.byte_range()))
            == Some(EXPORTS_NAME);
        module_variable && exports_property
    }

    /// The `object` of a `member_expression`; `None` for any other node.
    fn member_object<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        if node.kind_id() != self.member_expression {
            return None;
        }
        self.child(node, EcmaScriptGrammarField::Object)
    }

    /// The identifier one property of an exported object literal names: `a`
    /// in `{ a }` and `c` in `{ b: c }`; `None` for a method, a spread, or a
    /// value that is no identifier.
    fn property_export<'tree>(&self, property: Node<'tree>) -> Option<Node<'tree>> {
        if property.kind_id() == self.shorthand_property_identifier {
            return Some(property);
        }
        if property.kind_id() != self.pair {
            return None;
        }
        self.child(property, EcmaScriptGrammarField::Value)
            .filter(|value| value.kind_id() == self.identifier)
    }
}

/// Every named child of `node`, in order.
fn named_children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    node.named_child_indices()
        .filter_map(move |index| node.named_child(index))
}

/// What a module exports without an `export` wrapping the declaration, read
/// from its top-level statements before the walk.
///
/// The read visits the program's statements, their export clauses and
/// assignment chains, and the properties of an object literal exported
/// whole, each node once, so its work is linear in the source
/// `source_bytes_max` admits. An assignment nested in a block or a function
/// is not read.
#[derive(Debug, Default)]
struct ModuleExports<'text> {
    /// Names an export gives to module-scope declarations.
    names: BTreeSet<&'text str>,
    /// Ids of the object literals the module exports whole; a method written
    /// in one is exported.
    objects: BTreeSet<usize>,
}

impl<'text> ModuleExports<'text> {
    /// Reads what `program`'s top-level statements export by name or as a
    /// whole object literal.
    fn read(program: Node<'_>, text: &'text str, kinds: &EcmaScriptKinds) -> Self {
        let mut exports = Self::default();
        for statement in named_children(program) {
            for exported in kinds.statement_exports(statement, text) {
                exports.add(exported, text, kinds);
            }
        }
        exports
    }

    /// Records one exported node: an identifier's name, or an object literal
    /// with the names its properties give.
    fn add(&mut self, exported: Node<'_>, text: &'text str, kinds: &EcmaScriptKinds) {
        if exported.kind_id() == kinds.identifier {
            self.names.extend(text.get(exported.byte_range()));
            return;
        }
        if exported.kind_id() != kinds.object_literal {
            return;
        }
        self.objects.insert(exported.id());
        let named = named_children(exported).filter_map(|property| kinds.property_export(property));
        self.names
            .extend(named.filter_map(|name| text.get(name.byte_range())));
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

/// One pinned grammar's decisions for the shared bounded walk over one
/// source.
#[derive(Debug)]
pub(crate) struct EcmaScriptRules<'text> {
    kinds: &'static EcmaScriptKinds,
    exports: ModuleExports<'text>,
}

impl EcmaScriptRules<'_> {
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

    /// Whether the module exports the declaration `node` names `name`: an
    /// `export_statement` wraps it, an export names it at module scope, or it
    /// is a method of an object literal the module exports whole.
    fn exported(&self, node: Node<'_>, name: &str) -> bool {
        let wrapped = self.wrapped_by_export(node);
        let named = self.exports.names.contains(name) && self.at_module_scope(node);
        let in_exported_object = node
            .parent()
            .is_some_and(|parent| self.exports.objects.contains(&parent.id()));
        wrapped || named || in_exported_object
    }

    /// Whether an `export_statement` wraps the declaration: its direct
    /// parent, or - for a declarator - the parent of its declaration
    /// statement.
    fn wrapped_by_export(&self, node: Node<'_>) -> bool {
        let Some(parent) = node.parent() else {
            return false;
        };
        if parent.kind_id() == self.kinds.export_statement {
            return true;
        }
        self.kinds.is_declaration_statement(parent)
            && parent
                .parent()
                .is_some_and(|wrapper| wrapper.kind_id() == self.kinds.export_statement)
    }

    /// Whether `node` declares at module scope: its statement - the node
    /// itself, or a declarator's declaration statement - sits directly in the
    /// program.
    fn at_module_scope(&self, node: Node<'_>) -> bool {
        let statement = node
            .parent()
            .filter(|parent| self.kinds.is_declaration_statement(*parent))
            .unwrap_or(node);
        statement
            .parent()
            .is_some_and(|parent| parent.kind_id() == self.kinds.program)
    }

    /// The authored `accessibility_modifier` text on `node`; `None` when the
    /// grammar spells none or the declaration carries none.
    fn accessibility(&self, node: Node<'_>, text: &str) -> Option<String> {
        let modifier = self.kinds.accessibility_modifier?;
        node.named_child_indices()
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

impl GrammarRules for EcmaScriptRules<'_> {
    fn name_range(&self, node: Node<'_>) -> Result<Option<crate::ByteRange>, SyntaxError> {
        node.child_by_field_id(self.kinds.field(EcmaScriptGrammarField::Name).get())
            .map(extract::byte_range)
            .transpose()
    }

    fn declaration(&self, node: Node<'_>, text: &str) -> Result<Option<Declaration>, SyntaxError> {
        let Some(kind) = self.kinds.symbol_kind(node) else {
            return Ok(None);
        };
        if self.kinds.is_type_literal_member(node) {
            return Ok(None);
        }
        let Some(name) = self.declaration_name(node, kind, text) else {
            return Ok(None);
        };
        let mut facets = kind.facets();
        if self.exported(node, &name) {
            facets.push(SymbolFacet::Public);
        }
        Ok(Some(Declaration {
            name,
            kind: kind.word(),
            facets,
            visibility: self.accessibility(node, text),
            body_range: self.body_range(node, kind)?,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
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
    let rules = EcmaScriptRules {
        kinds,
        exports: ModuleExports::read(tree.root_node(), source.text, kinds),
    };
    let (nodes, symbols) = extract::extract(tree.root_node(), source, limits, language, &rules)?;
    Ok(SyntaxDocument::new(
        language.clone(),
        source.path.clone(),
        nodes,
        symbols,
        tree.root_node().has_error(),
    )
    .with_source_witness(source.text))
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
        FUNCTION_SIGNATURE_KIND | METHOD_SIGNATURE_KIND | PROPERTY_SIGNATURE_KIND => {
            vec![NodeFacet::Declaration]
        }
        LEXICAL_DECLARATION_KIND | VARIABLE_DECLARATION_KIND => {
            vec![NodeFacet::Declaration, NodeFacet::Statement]
        }
        EXPORT_STATEMENT_KIND => vec![NodeFacet::Export, NodeFacet::Statement],
        "import_statement" => vec![NodeFacet::Import, NodeFacet::Statement],
        "statement_block" => vec![NodeFacet::Block],
        CLASS_BODY_KIND | INTERFACE_BODY_KIND | "enum_body" => vec![NodeFacet::Body],
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
            assert_eq!(typescript.declarations.len(), 12);
            assert_eq!(typescript.member_signatures.len(), 2);
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
            (EcmaScriptSymbolKind::Property, "property"),
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
            METHOD_SIGNATURE_KIND,
            PROPERTY_SIGNATURE_KIND,
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
