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
//! - The `Public` kind marks a declaration the module exports. An `export`
//!   statement wrapping a declaration adds it, and so does an export naming a
//!   module-scope declaration: `a` and `b` in `export { a, b as c }`, `a` in
//!   `export default a` and the TypeScript `export = a`, and the declarations
//!   a `module.exports = a`, `module.exports = { a, b: c }`,
//!   `module.exports.name = a`, or `exports.name = a` assignment names. The
//!   declaration keeps its own name. A method written in an object literal
//!   the module exports whole (`export default { .. }`, `export = { .. }`,
//!   `module.exports = { .. }`) is exported too.
//! - Exports are read from the module's top-level statements: an assignment
//!   nested in a block or a function marks nothing. A re-export
//!   (`export { x } from './y'`, `export * from './y'`) names another
//!   module's declarations, so it marks none here. A function in a
//!   literal property assignment (`exports.run = function () {}`) retains the
//!   property binding and the original function source. A class or any other
//!   value but `void 0` declares the property it is assigned to
//!   (`exports.limit = 10`), under `exports` when a declaration at module scope
//!   has that name (`exports.limit`). A recorded top-level binding carries `Public` in a file without
//!   ECMAScript module syntax. A named function expression keeps its local name
//!   inside its own body. An anonymous `module.exports` function or class has
//!   the export name `default`.
//! - A module-scope name bound to the exported object
//!   (`var app = exports = module.exports = {}`) is the module's default
//!   export, and a function assigned to its member is exported with it.
//! - `export` is not a visibility spelling: the `visibility` field carries
//!   only an authored `accessibility_modifier` (`public`, `private`,
//!   `protected`), and stays `None` everywhere else.
//! - A `variable_declarator` under a `lexical_declaration` or
//!   `variable_declaration` declares a `variable`; an arrow function
//!   assigned to one is that variable, named by the declarator, and carries
//!   the `Callable` facet. A destructuring declarator declares one
//!   `variable` per name its pattern binds, each spanning its own name.
//! - A function, a method, and a variable named by an identifier open a
//!   scope, as a class does: a local or a method of an object literal
//!   qualifies under the declaration holding it (`outer.local`,
//!   `options.open`), never at module scope.
//! - A function assigned to a literal member declares a method when its
//!   receiver uniquely resolves to a local variable, function, or class.
//!   A prototype method belongs to its local function or class. Declarations
//!   in the method body keep that method's qualified name as their owner.
//! - A class field and a constructor parameter property declare a
//!   `property` of the class; an enum member declares an `enum_member` of
//!   the enum. The anonymous class `export default class { .. }` declares a
//!   class named `default`, the name the module exports it under.
//! - A computed member key names its method by the key expression's own
//!   bytes, so a name can run to any length. A declaration whose name or
//!   qualified name passes `PROVIDER_SYMBOL_ID_BYTES_MAX` bytes is left out
//!   of the document with every declaration nested under it, and counted.
//! - A declaration's complete span starts at the statement carrying it - the
//!   `export_statement`, or the `lexical_declaration` or `variable_declaration`
//!   holding it as its first declarator - and extends over the `JSDoc`
//!   comments directly attached before that statement. A decorator belongs
//!   to the declaration it qualifies: a child of the declared syntax element, or, for a
//!   TypeScript method, a sibling written before it that starts its span.
//! - A `method_signature` or `property_signature` declares a `method` or a
//!   `property` inside an interface, class, or type literal, qualified by the
//!   declaration containing it (`Array.map`, `Options.size`).
//! - `documentation` carries those attached `JSDoc` blocks, nearest last,
//!   joined by a line break, with the `/**`, `*/`, and leading `*` markers
//!   stripped and `@` tags kept as written. A `//` or `/* */` comment is not
//!   documentation, and it stops the walk, as a blank line does. A
//!   `Signature` renders for a callable declaration from `body_range` and the
//!   `Callable` kind, and a bodyless one, an overload or a member signature,
//!   renders its own text.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

use rift_core::line::{LINE_FEED, LineEnding, lines_inclusive, without_ending};
use rift_error::errors;
use rift_protocol::read::{Documentation, DocumentationFormat, Language, SymbolKind, SyntaxKind};

mod callback_parameters;
mod checked_parameters;
mod commonjs;
mod members;
mod overload_parameters;
pub(crate) mod recovery;
mod scoped_function_parameters;
mod shorthand_parameters;
mod type_parameters;
mod type_signature_parameters;
mod variance;
mod wrapped_parameters;

use tree_sitter::{Node, Parser};

use crate::document::{ByteRange, SyntaxDocument, SyntaxExportBinding, SyntaxExportKind};
use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::failure::RiftError;
use crate::provider::{SyntaxLimits, SyntaxSource};

/// Grammar spelling of a `function_declaration`.
const FUNCTION_DECLARATION_KIND: &str = "function_declaration";
/// Grammar spelling of a `generator_function_declaration`.
const GENERATOR_FUNCTION_DECLARATION_KIND: &str = "generator_function_declaration";
/// Grammar spelling of a `class_declaration`.
const CLASS_DECLARATION_KIND: &str = "class_declaration";
/// Grammar spelling of a `method_definition`.
const METHOD_DEFINITION_KIND: &str = "method_definition";
/// Grammar spelling of a JavaScript class field.
const FIELD_DEFINITION_KIND: &str = "field_definition";
/// Grammar spelling of a TypeScript class field.
const PUBLIC_FIELD_DEFINITION_KIND: &str = "public_field_definition";
/// Grammar spelling of an `abstract class` (TypeScript grammars only).
const ABSTRACT_CLASS_DECLARATION_KIND: &str = "abstract_class_declaration";
/// Grammar spelling of a bodyless `abstract` method (TypeScript grammars
/// only).
const ABSTRACT_METHOD_SIGNATURE_KIND: &str = "abstract_method_signature";
/// Grammar spelling of an enum member that assigns a value (TypeScript
/// grammars only).
const ENUM_ASSIGNMENT_KIND: &str = "enum_assignment";
/// Grammar spelling of an enum's body (TypeScript grammars only).
const ENUM_BODY_KIND: &str = "enum_body";
/// Grammar spelling of a member name, `B` in `enum Kind { B }`.
const PROPERTY_IDENTIFIER_KIND: &str = "property_identifier";
/// Grammar spelling of a class expression, `class { .. }` in
/// `export default class { .. }`.
const CLASS_KIND: &str = "class";
/// Grammar spelling of an arrow function expression.
const ARROW_FUNCTION_KIND: &str = "arrow_function";
/// Grammar spelling of a `function` expression.
const FUNCTION_EXPRESSION_KIND: &str = "function_expression";
/// Grammar spelling of a `function*` expression.
const GENERATOR_FUNCTION_KIND: &str = "generator_function";
/// Grammar spelling of a constructor parameter (TypeScript grammars only).
const REQUIRED_PARAMETER_KIND: &str = "required_parameter";
/// Grammar spelling of an optional constructor parameter (TypeScript
/// grammars only).
const OPTIONAL_PARAMETER_KIND: &str = "optional_parameter";
/// Binding patterns a destructuring declarator holds its names through:
/// every name inside one is bound.
const BINDING_PATTERN_KINDS: [&str; 3] = ["object_pattern", "array_pattern", "rest_pattern"];
/// Grammar spelling of `key: value` inside an object pattern; only its value
/// binds.
const PAIR_PATTERN_KIND: &str = "pair_pattern";
/// Patterns with a default, `a = b`; only their left side binds.
const DEFAULT_PATTERN_KINDS: [&str; 2] = ["assignment_pattern", "object_assignment_pattern"];
/// Grammar spelling of `a` in the object pattern `{ a }`.
const SHORTHAND_PROPERTY_IDENTIFIER_PATTERN_KIND: &str = "shorthand_property_identifier_pattern";
/// The method name a class constructor carries.
const CONSTRUCTOR_NAME: &str = "constructor";
/// The name a module exports its default export under, which an anonymous
/// default class declares as.
const DEFAULT_EXPORT_NAME: &str = "default";
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
/// Grammar spelling of a TypeScript module declaration.
const MODULE_KIND: &str = "module";
/// Grammar spelling of a bodyless `function_signature` (TypeScript grammars
/// only).
const FUNCTION_SIGNATURE_KIND: &str = "function_signature";
/// TypeScript `declare` wrapper around an ambient declaration.
const AMBIENT_DECLARATION_KIND: &str = "ambient_declaration";
/// Qualification of declarations in an authored augmentation of the global scope.
pub(crate) const GLOBAL_AUGMENTATION_NAME: &str = "global";
/// Grammar spelling of a TypeScript callable type.
const FUNCTION_TYPE_KIND: &str = "function_type";
/// Grammar spelling of a TypeScript type annotation.
const TYPE_ANNOTATION_KIND: &str = "type_annotation";
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
/// Grammar spelling of one named export binding.
const EXPORT_SPECIFIER_KIND: &str = "export_specifier";
/// Grammar spelling of `export * as name`.
const NAMESPACE_EXPORT_KIND: &str = "namespace_export";
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
/// Grammar spelling of the anonymous `=` token, the one direct token that
/// separates the TypeScript `export = value` form from every other `export`.
const EQUALS_TOKEN: &str = "=";
/// The variable naming the running module, `module` in `module.exports`.
const MODULE_VARIABLE: &str = "module";
/// The property of `module`, and the variable, holding a module's exports:
/// `module.exports`, `exports.name`.
const EXPORTS_NAME: &str = "exports";
/// Grammar spelling of a `comment`, line or block.
const COMMENT_KIND: &str = "comment";
/// Grammar spelling of one `@decorator`.
const DECORATOR_KIND: &str = "decorator";
/// Opens a `JSDoc` block: a block comment whose first delimiter carries a second `*`.
const JSDOC_OPEN: &str = "/**";
/// Closes every block comment, a `JSDoc` block included.
const BLOCK_COMMENT_CLOSE: &str = "*/";
/// Leads each continuation line inside a `JSDoc` block.
const JSDOC_LINE_MARKER: char = '*';

/// Provider kind of an ordinary ECMAScript value parameter.
pub(crate) const PARAMETER_KIND: &str = "value_parameter";
pub(crate) const CALLBACK_PARAMETER_KIND: &str = "callback_parameter";
pub(crate) const CALLBACK_PATTERN_PARAMETER_KIND: &str = "javascript_callback_pattern_parameter";
pub(crate) const OVERLOAD_PARAMETER_KIND: &str = "overload_parameter";
pub(crate) const METHOD_PARAMETER_KIND: &str = "method_parameter";
pub(crate) const SIGNATURE_PARAMETER_KIND: &str = "typescript_signature_parameter";
pub(crate) const SCOPED_FUNCTION_PARAMETER_KIND: &str = "javascript_scoped_function_parameter";
pub(crate) const TYPE_PARAMETER_KIND: &str = "typescript_type_parameter";
pub(crate) const FUNCTION_TYPE_PARAMETER_KIND: &str = "typescript_function_type_parameter";
pub(crate) const SIGNATURE_TYPE_PARAMETER_KIND: &str = "typescript_signature_type_parameter";
pub(crate) const METHOD_TYPE_PARAMETER_KIND: &str = "typescript_method_type_parameter";
pub(crate) const CALLABLE_TYPE_PARAMETER_KIND: &str = "typescript_callable_type_parameter";
pub(crate) const INDEX_SIGNATURE_PARAMETER_KIND: &str = "typescript_index_signature_parameter";
pub(crate) const WRAPPED_PARAMETER_KIND: &str = "javascript_wrapped_parameter";
pub(crate) const DESTRUCTURED_PARAMETER_KIND: &str = "javascript_destructured_parameter";
pub(crate) const TYPED_DESTRUCTURED_PARAMETER_KIND: &str = "typescript_destructured_parameter";
pub(crate) const SHORTHAND_PARAMETER_KIND: &str = "javascript_shorthand_parameter";

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
    /// Class field, or a constructor parameter property (TypeScript). It
    /// carries the `property` kind word, the same declaration a property
    /// signature names, and belongs to its class.
    Field,
    /// Member of an enumeration (TypeScript).
    EnumMember,
}

impl EcmaScriptSymbolKind {
    /// The kind word this declaration kind carries on the wire.
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
            Self::Property | Self::Field => "property",
            Self::EnumMember => "enum_member",
        }
    }

    /// Portable kinds for this kind, before an export adds `Public`.
    fn kinds(self) -> Vec<SymbolKind> {
        match self {
            Self::Function | Self::Method => vec![SymbolKind::Value, SymbolKind::Callable],
            Self::Class | Self::Interface | Self::Enum => vec![SymbolKind::Type],
            Self::TypeAlias => vec![SymbolKind::Type, SymbolKind::Alias],
            Self::Variable | Self::Property => vec![SymbolKind::Value],
            Self::Field => vec![SymbolKind::Value, SymbolKind::Member],
            Self::EnumMember => vec![SymbolKind::Variant],
            Self::Namespace => vec![SymbolKind::Namespace],
        }
    }

    /// The grammar field spanning this kind's implementation part. Every
    /// kind declares one; a syntax element that omits it - a bodyless signature, a
    /// valueless declarator - carries no body range.
    const fn body_field(self) -> EcmaScriptGrammarField {
        match self {
            Self::Function
            | Self::Class
            | Self::Method
            | Self::Interface
            | Self::Enum
            | Self::Namespace => EcmaScriptGrammarField::Body,
            Self::Variable | Self::TypeAlias | Self::Property | Self::Field | Self::EnumMember => {
                EcmaScriptGrammarField::Value
            }
        }
    }

    /// Whether declarations inside this kind qualify under its name. A
    /// callable and a variable open one too, so a local or a method of an
    /// object literal files under the declaration holding it, never at
    /// module scope.
    const fn opens_scope(self) -> bool {
        matches!(
            self,
            Self::Class
                | Self::Namespace
                | Self::Interface
                | Self::Enum
                | Self::Function
                | Self::Method
                | Self::Variable
                | Self::Property
                | Self::Field
                | Self::TypeAlias
        )
    }
}

/// Grammar field this module reads, common to all three pinned grammars.
#[derive(Debug, Clone, Copy)]
enum EcmaScriptGrammarField {
    /// `name` field on declaration syntax elements.
    Name,
    /// `body` field on block-bodied declaration syntax elements.
    Body,
    /// `value` field on `variable_declarator`, `type_alias_declaration`,
    /// `pair`, and a default `export_statement`.
    Value,
    /// `source` field on an `export_statement` that re-exports another module.
    Source,
    /// Exported name of an aliased export specifier.
    Alias,
    /// `left` field on an `assignment_expression`, and on a pattern with a
    /// default.
    Left,
    /// `right` field on an `assignment_expression`.
    Right,
    /// `object` field on a `member_expression`, `module` in `module.exports`.
    Object,
    /// `property` field on a `member_expression`, `exports` in `module.exports`,
    /// and the name of a JavaScript `field_definition`.
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
    comment: u16,
    decorator: u16,
    /// `Some` on the TypeScript grammars; the JavaScript grammar spells no
    /// accessibility.
    accessibility_modifier: Option<u16>,
    ambient_declaration: Option<u16>,
    global_namespace: Option<u16>,
    global_token: Option<u16>,
    function_type: Option<u16>,
    type_annotation: Option<u16>,
    declaration_type: Option<NonZeroU16>,
    /// Member signature kinds: declarations only inside an interface or a
    /// class body, since a type literal spells them too. Empty on the
    /// JavaScript grammar.
    member_signatures: Vec<u16>,
    /// The bodies a member signature declares in: `interface_body` and
    /// `class_body`. Empty on the JavaScript grammar.
    member_bodies: Vec<u16>,
    program: u16,
    export_clause: u16,
    export_specifier: u16,
    namespace_export: u16,
    default_token: u16,
    star_token: u16,
    type_tokens: Vec<u16>,
    equals_token: u16,
    expression_statement: u16,
    assignment_expression: u16,
    member_expression: u16,
    object_literal: u16,
    shorthand_property_identifier: u16,
    pair: u16,
    key: NonZeroU16,
    static_token: u16,
    catch_clause: u16,
    for_in: u16,
    loop_kind: NonZeroU16,
    parameter: NonZeroU16,
    parenthesized: u16,
    name: NonZeroU16,
    body: NonZeroU16,
    value: NonZeroU16,
    source: NonZeroU16,
    alias: NonZeroU16,
    left: NonZeroU16,
    right: NonZeroU16,
    object: NonZeroU16,
    property: NonZeroU16,
    /// `abstract_class_declaration` and `abstract_method_signature`; empty on
    /// the JavaScript grammar.
    abstract_declarations: Vec<u16>,
    /// The class expression an anonymous `export default class` holds.
    class_expression: u16,
    /// Function expressions that make the variable holding one callable.
    function_values: Vec<u16>,
    /// Expressions that hold the same value as their operand: a type assertion first,
    /// then `as`, `satisfies` and a non-null mark. Empty on the JavaScript grammar.
    value_assertions: Vec<u16>,
    /// `Some` on the TypeScript grammars.
    enum_body: Option<u16>,
    property_identifier: u16,
    method_definition: u16,
    /// Constructor parameters that can declare a property; empty on the
    /// JavaScript grammar.
    formal_parameter_kinds: Vec<u16>,
    /// The `readonly` token a parameter property can carry.
    readonly_token: Option<u16>,
    /// `pattern` field on a parameter; `None` on the JavaScript grammar.
    pattern: Option<NonZeroU16>,
    variable_declarator: u16,
    binding_patterns: Vec<u16>,
    pair_pattern: u16,
    default_patterns: Vec<u16>,
    shorthand_property_identifier_pattern: u16,
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
        let mut kinds = Self::resolve_core(language);
        kinds.declarations.push((
            kind_id(language, FIELD_DEFINITION_KIND),
            EcmaScriptSymbolKind::Field,
        ));
        kinds
    }

    fn core_declarations(language: &tree_sitter::Language) -> Vec<(u16, EcmaScriptSymbolKind)> {
        vec![
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
            (kind_id(language, PAIR_KIND), EcmaScriptSymbolKind::Property),
            (
                kind_id(language, SHORTHAND_PROPERTY_IDENTIFIER_KIND),
                EcmaScriptSymbolKind::Property,
            ),
        ]
    }

    /// Resolves the vocabulary every pinned ECMAScript grammar shares.
    fn resolve_core(language: &tree_sitter::Language) -> Self {
        Self {
            declarations: Self::core_declarations(language),
            lexical_declaration: kind_id(language, LEXICAL_DECLARATION_KIND),
            variable_declaration: kind_id(language, VARIABLE_DECLARATION_KIND),
            export_statement: kind_id(language, EXPORT_STATEMENT_KIND),
            identifier: kind_id(language, IDENTIFIER_KIND),
            comment: kind_id(language, COMMENT_KIND),
            decorator: kind_id(language, DECORATOR_KIND),
            accessibility_modifier: None,
            ambient_declaration: None,
            global_namespace: None,
            global_token: None,
            function_type: None,
            type_annotation: None,
            declaration_type: None,
            member_signatures: Vec::new(),
            member_bodies: Vec::new(),
            program: kind_id(language, PROGRAM_KIND),
            export_clause: kind_id(language, EXPORT_CLAUSE_KIND),
            export_specifier: kind_id(language, EXPORT_SPECIFIER_KIND),
            namespace_export: kind_id(language, NAMESPACE_EXPORT_KIND),
            default_token: token_id(language, "default"),
            star_token: token_id(language, "*"),
            type_tokens: Vec::new(),
            equals_token: token_id(language, EQUALS_TOKEN),
            expression_statement: kind_id(language, EXPRESSION_STATEMENT_KIND),
            assignment_expression: kind_id(language, ASSIGNMENT_EXPRESSION_KIND),
            member_expression: kind_id(language, MEMBER_EXPRESSION_KIND),
            object_literal: kind_id(language, OBJECT_KIND),
            shorthand_property_identifier: kind_id(language, SHORTHAND_PROPERTY_IDENTIFIER_KIND),
            pair: kind_id(language, PAIR_KIND),
            key: field_id(language, "key"),
            static_token: token_id(language, "static"),
            catch_clause: kind_id(language, "catch_clause"),
            for_in: kind_id(language, "for_in_statement"),
            loop_kind: field_id(language, "kind"),
            parameter: field_id(language, "parameter"),
            parenthesized: kind_id(language, "parenthesized_expression"),
            name: field_id(language, "name"),
            body: field_id(language, "body"),
            value: field_id(language, "value"),
            source: field_id(language, "source"),
            alias: field_id(language, "alias"),
            left: field_id(language, "left"),
            right: field_id(language, "right"),
            object: field_id(language, "object"),
            property: field_id(language, "property"),
            abstract_declarations: Vec::new(),
            class_expression: kind_id(language, CLASS_KIND),
            function_values: [
                ARROW_FUNCTION_KIND,
                FUNCTION_EXPRESSION_KIND,
                GENERATOR_FUNCTION_KIND,
            ]
            .map(|kind| kind_id(language, kind))
            .to_vec(),
            value_assertions: [
                "type_assertion",
                "as_expression",
                "satisfies_expression",
                "non_null_expression",
            ]
            .into_iter()
            .map(|kind| language.id_for_node_kind(kind, true))
            .filter(|id| *id != 0)
            .collect(),
            enum_body: None,
            property_identifier: kind_id(language, PROPERTY_IDENTIFIER_KIND),
            method_definition: kind_id(language, METHOD_DEFINITION_KIND),
            formal_parameter_kinds: Vec::new(),
            readonly_token: None,
            pattern: None,
            variable_declarator: kind_id(language, VARIABLE_DECLARATOR_KIND),
            binding_patterns: BINDING_PATTERN_KINDS
                .map(|kind| kind_id(language, kind))
                .to_vec(),
            pair_pattern: kind_id(language, PAIR_PATTERN_KIND),
            default_patterns: DEFAULT_PATTERN_KINDS
                .map(|kind| kind_id(language, kind))
                .to_vec(),
            shorthand_property_identifier_pattern: kind_id(
                language,
                SHORTHAND_PROPERTY_IDENTIFIER_PATTERN_KIND,
            ),
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
        let mut kinds = Self::resolve_core(language);
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
                kind_id(language, MODULE_KIND),
                EcmaScriptSymbolKind::Namespace,
            ),
            (
                kind_id(language, FUNCTION_SIGNATURE_KIND),
                EcmaScriptSymbolKind::Function,
            ),
            (
                kind_id(language, PUBLIC_FIELD_DEFINITION_KIND),
                EcmaScriptSymbolKind::Field,
            ),
            (
                kind_id(language, ABSTRACT_CLASS_DECLARATION_KIND),
                EcmaScriptSymbolKind::Class,
            ),
            (
                kind_id(language, ENUM_ASSIGNMENT_KIND),
                EcmaScriptSymbolKind::EnumMember,
            ),
        ]);
        let member_signatures = [
            (METHOD_SIGNATURE_KIND, EcmaScriptSymbolKind::Method),
            (PROPERTY_SIGNATURE_KIND, EcmaScriptSymbolKind::Property),
        ]
        .map(|(kind, symbol)| (kind_id(language, kind), symbol));
        kinds.declarations.extend(member_signatures);
        kinds.member_signatures = member_signatures.map(|(kind, _)| kind).to_vec();
        let abstract_method = kind_id(language, ABSTRACT_METHOD_SIGNATURE_KIND);
        kinds
            .declarations
            .push((abstract_method, EcmaScriptSymbolKind::Method));
        kinds.abstract_declarations = vec![
            kind_id(language, ABSTRACT_CLASS_DECLARATION_KIND),
            abstract_method,
        ];
        kinds.enum_body = Some(kind_id(language, ENUM_BODY_KIND));
        kinds.formal_parameter_kinds = [REQUIRED_PARAMETER_KIND, OPTIONAL_PARAMETER_KIND]
            .map(|kind| kind_id(language, kind))
            .to_vec();
        kinds.readonly_token = Some(token_id(language, "readonly"));
        kinds.pattern = Some(field_id(language, "pattern"));
        kinds.member_bodies = vec![
            kind_id(language, INTERFACE_BODY_KIND),
            kind_id(language, CLASS_BODY_KIND),
            kind_id(language, "object_type"),
        ];
        kinds.accessibility_modifier = Some(kind_id(language, ACCESSIBILITY_MODIFIER_KIND));
        kinds.ambient_declaration = Some(kind_id(language, AMBIENT_DECLARATION_KIND));
        kinds.global_namespace = Some(kind_id(language, "global_namespace"));
        kinds.global_token = Some(token_id(language, "global"));
        kinds.function_type = Some(kind_id(language, FUNCTION_TYPE_KIND));
        kinds.type_annotation = Some(kind_id(language, TYPE_ANNOTATION_KIND));
        kinds.declaration_type = Some(field_id(language, "type"));
        kinds.type_tokens = vec![token_id(language, "type"), token_id(language, "typeof")];
        kinds
    }

    fn is_global_augmentation(&self, syntax_element: Node<'_>) -> bool {
        self.global_namespace == Some(syntax_element.kind_id())
            || (self.ambient_declaration == Some(syntax_element.kind_id())
                && self
                    .global_token
                    .is_some_and(|token| EcmaScriptRules::has_token(syntax_element, &[token])))
    }

    /// Whether `syntax element` is a member signature outside an interface or class
    /// body, such as `{ size: number }` in a type annotation.
    fn is_type_literal_member(&self, visited: Visited<'_, '_>) -> bool {
        self.member_signatures
            .contains(&visited.syntax_element().kind_id())
            && !visited.parent().is_some_and(|parent| {
                self.member_bodies
                    .contains(&parent.syntax_element().kind_id())
            })
    }

    /// The symbol kind `syntax element` declares; `None` for a kind outside the table.
    fn symbol_kind(&self, syntax_element: Node<'_>) -> Option<EcmaScriptSymbolKind> {
        let id = syntax_element.kind_id();
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
            EcmaScriptGrammarField::Alias => self.alias,
            EcmaScriptGrammarField::Left => self.left,
            EcmaScriptGrammarField::Right => self.right,
            EcmaScriptGrammarField::Object => self.object,
            EcmaScriptGrammarField::Property => self.property,
        }
    }

    /// `syntax element`'s child in `field`; `None` when the syntax element omits it.
    fn child<'tree>(
        &self,
        syntax_element: Node<'tree>,
        field: EcmaScriptGrammarField,
    ) -> Option<Node<'tree>> {
        syntax_element.child_by_field_id(self.field(field).get())
    }

    /// Whether `syntax element` is a `let`, `const`, or `var` statement holding
    /// declarators.
    fn is_declaration_statement(&self, syntax_element: Node<'_>) -> bool {
        syntax_element.kind_id() == self.lexical_declaration
            || syntax_element.kind_id() == self.variable_declaration
    }

    /// The value inside expression parentheses. Syntax admission bounds the depth.
    fn expression_value<'tree>(&self, mut value: Node<'tree>) -> Node<'tree> {
        while value.kind_id() == self.parenthesized {
            let Some(child) = value.named_child(0) else {
                break;
            };
            value = child;
        }
        value
    }

    /// The value an initializer holds once parentheses and assertions are read through.
    ///
    /// `<T>value`, `value as T`, `value satisfies T` and `value!` hold `value` itself. The
    /// grammar also reads a generic arrow function after a parse error as a type assertion
    /// over the arrow. Syntax admission bounds the depth.
    fn asserted_value<'tree>(&self, value: Node<'tree>) -> Node<'tree> {
        let mut value = self.expression_value(value);
        while self.value_assertions.contains(&value.kind_id()) {
            // A type assertion, listed first, writes its type before the value; the
            // other forms write the value first.
            let cast = self.value_assertions.first() == Some(&value.kind_id());
            let operand = if cast {
                u32::try_from(value.named_child_count())
                    .ok()
                    .and_then(|count| count.checked_sub(1))
                    .and_then(|last| value.named_child(last))
            } else {
                value.named_child(0)
            };
            let Some(operand) = operand else {
                break;
            };
            value = self.expression_value(operand);
        }
        value
    }

    /// Whether `syntax element` is the plain identifier `spelling`.
    fn is_identifier(&self, syntax_element: Node<'_>, text: &str, spelling: &str) -> bool {
        syntax_element.kind_id() == self.identifier
            && text.get(syntax_element.byte_range()) == Some(spelling)
    }

    /// The syntax elements one top-level statement exports without wrapping a
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
    /// `export { a as b }`), its default value (`a` in `export default a`),
    /// and the value a TypeScript `export = a` exports whole. A re-export
    /// (`export { x } from './y'`, `export * from './y'`) carries a `source`
    /// and names another module's declarations, so it yields none.
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
            .chain(self.export_assignment_value(statement))
            .collect()
    }

    /// The value of a TypeScript `export = value` statement: the first named
    /// child after its `=` token. The grammar gives the value no field, and
    /// `export as namespace name` places an identifier in the same child
    /// position, so the token decides; `None` for any other `export`.
    fn export_assignment_value<'tree>(&self, statement: Node<'tree>) -> Option<Node<'tree>> {
        let mut cursor = statement.walk();
        let mut children = statement.children(&mut cursor);
        children.find(|child| child.kind_id() == self.equals_token)?;
        children.find(Node::is_named)
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

    /// The original property token and the function or class assigned to one literal
    /// export.
    fn export_definition<'tree>(
        &self,
        syntax_element: Node<'tree>,
        text: &str,
    ) -> Option<(Node<'tree>, Node<'tree>)> {
        if syntax_element.kind_id() != self.assignment_expression {
            return None;
        }
        let left = self.child(syntax_element, EcmaScriptGrammarField::Left)?;
        self.export_target(left, text)?;
        let value =
            self.expression_value(self.child(syntax_element, EcmaScriptGrammarField::Right)?);
        if !self.function_values.contains(&value.kind_id())
            && value.kind_id() != self.class_expression
        {
            return None;
        }
        let property = self.child(left, EcmaScriptGrammarField::Property)?;
        Some((property, value))
    }

    /// The innermost assignment of a chain and the value it assigns: `b = value` and
    /// `value` in `a = b = value`. Each step descends one admitted syntax element.
    fn assignment_chain<'tree>(
        &self,
        assignment: Node<'tree>,
    ) -> Option<(Node<'tree>, Node<'tree>)> {
        let mut inner = assignment;
        loop {
            let value = self.expression_value(self.child(inner, EcmaScriptGrammarField::Right)?);
            if value.kind_id() != self.assignment_expression {
                return Some((inner, value));
            }
            inner = value;
        }
    }

    /// The value an assignment or a chain of assignments ends in.
    fn assigned_value<'tree>(&self, assignment: Node<'tree>) -> Option<Node<'tree>> {
        self.assignment_chain(assignment).map(|(_, value)| value)
    }

    fn export_function_name<'text>(
        &self,
        assignment: Node<'_>,
        text: &'text str,
    ) -> Option<&'text str> {
        let (property, _) = self.export_definition(assignment, text)?;
        let left = self.child(assignment, EcmaScriptGrammarField::Left)?;
        if self.export_target(left, text) == Some(ExportTarget::Module) {
            Some(DEFAULT_EXPORT_NAME)
        } else {
            text.get(property.byte_range())
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

    /// Whether `syntax element` spells `module.exports`.
    fn is_module_exports(&self, syntax_element: Node<'_>, text: &str) -> bool {
        let Some(object) = self.member_object(syntax_element) else {
            return false;
        };
        let module_variable = self.is_identifier(object, text, MODULE_VARIABLE);
        let exports_property = self
            .child(syntax_element, EcmaScriptGrammarField::Property)
            .and_then(|property| text.get(property.byte_range()))
            == Some(EXPORTS_NAME);
        module_variable && exports_property
    }

    /// The declaration syntax element's name: its `name` field, or the `property` field
    /// a JavaScript `field_definition` spells it in.
    fn name_syntax_element<'tree>(&self, syntax_element: Node<'tree>) -> Option<Node<'tree>> {
        if syntax_element.kind_id() == self.pair {
            return syntax_element.child_by_field_id(self.key.get());
        }
        if syntax_element.kind_id() == self.shorthand_property_identifier {
            return Some(syntax_element);
        }
        self.child(syntax_element, EcmaScriptGrammarField::Name)
            .or_else(|| {
                (self.symbol_kind(syntax_element) == Some(EcmaScriptSymbolKind::Field))
                    .then(|| self.child(syntax_element, EcmaScriptGrammarField::Property))
                    .flatten()
            })
    }

    /// The `variable_declarator` a name inside a destructuring pattern binds
    /// through: `major` in `const { major } = version`. `None` for a name
    /// that no pattern of a declarator binds - a plain declarator name, a
    /// pattern key, a default value, or a parameter pattern.
    ///
    /// Each step reads one recorded parent, so the work is bounded by the
    /// tree's depth.
    fn pattern_declarator<'walk, 'tree>(
        &self,
        visited: Visited<'walk, 'tree>,
    ) -> Option<Visited<'walk, 'tree>> {
        let id = visited.syntax_element().kind_id();
        if id != self.identifier && id != self.shorthand_property_identifier_pattern {
            return None;
        }
        let mut child = visited;
        let mut through_pattern = false;
        while let Some(parent) = child.parent() {
            let id = parent.syntax_element().kind_id();
            if id == self.for_in {
                return (parent
                    .syntax_element()
                    .child_by_field_id(self.loop_kind.get())
                    .is_some()
                    && self.child(parent.syntax_element(), EcmaScriptGrammarField::Left)
                        == Some(child.syntax_element()))
                .then_some(parent);
            }
            if id == self.variable_declarator {
                let binds = through_pattern
                    && self.child(parent.syntax_element(), EcmaScriptGrammarField::Name)
                        == Some(child.syntax_element());
                return binds.then_some(parent);
            }
            let binds = if self.binding_patterns.contains(&id) {
                true
            } else if id == self.pair_pattern {
                self.child(parent.syntax_element(), EcmaScriptGrammarField::Value)
                    == Some(child.syntax_element())
            } else if self.default_patterns.contains(&id) {
                self.child(parent.syntax_element(), EcmaScriptGrammarField::Left)
                    == Some(child.syntax_element())
            } else {
                false
            };
            if !binds {
                return None;
            }
            through_pattern = true;
            child = parent;
        }
        None
    }

    /// A simple parameter of a named function or method, with its JavaScript method class.
    fn ordinary_parameter<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
    ) -> Option<(Node<'tree>, bool)> {
        let parameter = visited.syntax_element();
        let name = if parameter.kind_id() == self.identifier {
            parameter
        } else if self.formal_parameter_kinds.contains(&parameter.kind_id()) {
            parameter.child_by_field_id(self.pattern?.get())?
        } else {
            return None;
        };
        if parameter.has_error() || name.kind_id() != self.identifier || name.is_missing() {
            return None;
        }
        let list = visited.parent()?;
        if list.syntax_element().kind() != "formal_parameters" {
            return None;
        }
        let function = list.parent()?.syntax_element();
        let direct = function.kind() == FUNCTION_DECLARATION_KIND;
        let generator = self.formal_parameter_kinds.is_empty()
            && function.kind() == GENERATOR_FUNCTION_DECLARATION_KIND;
        let method = function.kind_id() == self.method_definition
            && !self.parameter_property_marked(parameter);
        let signature = matches!(
            function.kind(),
            FUNCTION_SIGNATURE_KIND | METHOD_SIGNATURE_KIND | ABSTRACT_METHOD_SIGNATURE_KIND
        );
        if !(direct || generator || method || signature) {
            return None;
        }
        self.child(function, EcmaScriptGrammarField::Name)?;
        if !signature {
            self.child(function, EcmaScriptGrammarField::Body)?;
        }
        Some((name, method && self.formal_parameter_kinds.is_empty()))
    }

    /// The name a constructor parameter property declares: `name` in
    /// `constructor(public readonly name: string)`. Only a parameter carrying
    /// an accessibility modifier or `readonly` declares one.
    fn parameter_property<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
        text: &str,
    ) -> Option<Node<'tree>> {
        let syntax_element = visited.syntax_element();
        if !self
            .formal_parameter_kinds
            .contains(&syntax_element.kind_id())
        {
            return None;
        }
        if !self.parameter_property_marked(syntax_element) {
            return None;
        }
        let method = visited.parent()?.parent()?.syntax_element();
        let constructor = method.kind_id() == self.method_definition
            && self
                .child(method, EcmaScriptGrammarField::Name)
                .and_then(|name| text.get(name.byte_range()))
                == Some(CONSTRUCTOR_NAME);
        if !constructor {
            return None;
        }
        let name = syntax_element.child_by_field_id(self.pattern?.get())?;
        (name.kind_id() == self.identifier).then_some(name)
    }

    fn parameter_property_marked(&self, parameter: Node<'_>) -> bool {
        named_children(parameter).any(|child| Some(child.kind_id()) == self.accessibility_modifier)
            || self
                .readonly_token
                .is_some_and(|token| EcmaScriptRules::has_token(parameter, &[token]))
    }

    /// Whether `syntax_element` is an enum member written without a value, `B` in
    /// `enum Kind { A = 1, B }`.
    fn is_bare_enum_member(&self, syntax_element: Node<'_>, parent: Option<Node<'_>>) -> bool {
        syntax_element.kind_id() == self.property_identifier
            && self.enum_body.is_some()
            && parent.map(|parent| parent.kind_id()) == self.enum_body
    }

    /// The `object` of a `member_expression`; `None` for any other syntax element.
    fn member_object<'tree>(&self, syntax_element: Node<'tree>) -> Option<Node<'tree>> {
        if syntax_element.kind_id() != self.member_expression {
            return None;
        }
        self.child(syntax_element, EcmaScriptGrammarField::Object)
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

/// Every named child of `syntax element`, in order, read in one cursor walk.
fn named_children(syntax_element: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    let mut cursor = syntax_element.walk();
    let children: Vec<_> = syntax_element.named_children(&mut cursor).collect();
    children.into_iter()
}

/// What a module exports without an `export` wrapping the declaration, read
/// from its top-level statements before the walk.
///
/// The read visits the program's statements, their export clauses and
/// assignment chains, and the properties of an object literal exported
/// whole, each syntax element once, so its work is linear in the source
/// `source_bytes_max` admits. An assignment nested in a block or a function
/// is not read.
#[derive(Debug, Default)]
struct ModuleExports<'text> {
    /// Names an export gives to module-scope declarations.
    names: BTreeSet<&'text str>,
    /// Ids of the object literals the module exports whole; a method written
    /// in one is exported.
    objects: BTreeSet<usize>,
    /// Anonymous functions and classes exported under the default name.
    default_values: BTreeSet<usize>,
    /// Exact named bindings from top-level `CommonJS` object exports.
    bindings: BTreeMap<usize, SyntaxExportBinding>,
    /// Ids of the `CommonJS` assignments whose own declaration the module exports.
    declared: BTreeSet<usize>,
    /// Ids of the `CommonJS` assignments whose declaration qualifies under `exports`, since
    /// a declaration at module scope takes its name.
    shadowing: BTreeSet<usize>,
    /// Ids of the object literals a `CommonJS` export holds under a declaration of its
    /// own: their members are exported.
    members: BTreeSet<usize>,
}

impl<'text> ModuleExports<'text> {
    /// Reads what `program`'s top-level statements export by name or as a
    /// whole object literal.
    fn read(
        program: Node<'_>,
        text: &'text str,
        kinds: &EcmaScriptKinds,
    ) -> Result<Self, RiftError> {
        let mut exports = Self::default();
        for statement in named_children(program) {
            exports.record_default(statement, kinds);
            for exported in kinds.statement_exports(statement, text) {
                exports.add(exported, text, kinds);
            }
            if let Some(object) = kinds
                .assignment_export(statement, text)
                .filter(|syntax_element| syntax_element.kind_id() == kinds.object_literal)
            {
                exports.record_bindings(statement, object, text, kinds)?;
            }
        }
        Ok(exports)
    }

    fn record_default(&mut self, statement: Node<'_>, kinds: &EcmaScriptKinds) {
        if statement.kind_id() != kinds.export_statement
            || !EcmaScriptRules::has_token(statement, &[kinds.default_token])
        {
            return;
        }
        let mut value = kinds
            .child(statement, EcmaScriptGrammarField::Value)
            .or_else(|| {
                named_children(statement).find(|child| {
                    child.kind_id() == kinds.class_expression
                        || kinds.function_values.contains(&child.kind_id())
                })
            });
        while value.is_some_and(|value| value.kind_id() == kinds.parenthesized) {
            value = value.and_then(|value| value.named_child(0));
        }
        if let Some(value) = value.filter(|value| {
            kinds.child(*value, EcmaScriptGrammarField::Name).is_none()
                && (value.kind_id() == kinds.class_expression
                    || kinds.function_values.contains(&value.kind_id()))
        }) {
            self.default_values.insert(value.id());
        }
    }

    /// Records the settled properties of an exported object literal: a shorthand name, and
    /// a `name: value` pair no later property of the literal can replace.
    fn record_bindings(
        &mut self,
        statement: Node<'_>,
        object: Node<'_>,
        text: &str,
        kinds: &EcmaScriptKinds,
    ) -> Result<(), RiftError> {
        let range = extract::byte_range(statement)?;
        let properties = named_children(object).collect::<Vec<_>>();
        let mut later = BTreeSet::new();
        let mut settled = true;
        for property in properties.into_iter().rev() {
            if property.kind_id() == kinds.shorthand_property_identifier {
                later.extend(text.get(property.byte_range()));
                let name = extract::byte_range(property)?;
                self.bindings.insert(
                    property.id(),
                    SyntaxExportBinding {
                        kind: SyntaxExportKind::Named,
                        range,
                        local: Some(name),
                        exported: Some(name),
                        source: None,
                        container: None,
                        type_only: false,
                    },
                );
                continue;
            }
            let key = match property.kind() {
                "comment" => continue,
                "method_definition" => kinds.child(property, EcmaScriptGrammarField::Name),
                _ if property.kind_id() == kinds.pair => {
                    property.child_by_field_id(kinds.key.get())
                }
                _ => None,
            };
            // A spread or a computed key can replace every property written before it.
            let Some(key) = key.filter(|key| key.kind() != "computed_property_name") else {
                settled = false;
                continue;
            };
            let name = text.get(key.byte_range()).unwrap_or_default();
            let unquoted = name.trim_matches(['"', '\'']);
            settled &= !name.contains('\\');
            let last = later.insert(unquoted) && settled;
            if property.kind_id() != kinds.pair
                || key.kind_id() != kinds.property_identifier
                || name == "__proto__"
                || !last
            {
                continue;
            }
            let exported = extract::byte_range(key)?;
            let local = kinds
                .property_export(property)
                .map_or(Ok(exported), extract::byte_range)?;
            self.bindings.insert(
                property.id(),
                SyntaxExportBinding {
                    kind: SyntaxExportKind::Named,
                    range,
                    local: Some(local),
                    exported: Some(exported),
                    source: None,
                    container: None,
                    type_only: false,
                },
            );
        }
        Ok(())
    }

    /// Adds what the `CommonJS` assignments of the module body export.
    fn add_commonjs(&mut self, exports: &commonjs::CommonJsExports<'_>, text: &'text str) {
        self.names.extend(
            exports
                .named
                .iter()
                .filter_map(|name| text.get(name.byte_range())),
        );
        self.declared.extend(&exports.declared);
        self.members.extend(&exports.objects);
        self.shadowing.extend(&exports.shadowing);
    }

    /// Records one exported syntax element: an identifier's name, or an object literal
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

/// Resolves one syntax element kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned ECMAScript grammar must define syntax_element kind used by symbol \
         extraction: kind={kind}"
    );
    id
}

/// Resolves one anonymous token's kind id, proving the pinned grammar defines
/// it.
fn token_id(language: &tree_sitter::Language, token: &str) -> u16 {
    let id = language.id_for_node_kind(token, false);
    assert!(
        id != 0,
        "pinned ECMAScript grammar must define token used by symbol \
         extraction: token={token}"
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
    /// The name range of a named function a module-scope assignment gives to
    /// `module.exports`: the function is declared under the name it is written with.
    fn default_function_name(&self, assignment: Node<'_>) -> Option<crate::ByteRange> {
        let binding = self.exports.bindings.get(&assignment.id())?;
        let function = self.kinds.expression_value(
            self.kinds
                .child(assignment, EcmaScriptGrammarField::Right)?,
        );
        let written = self.kinds.child(function, EcmaScriptGrammarField::Name)?;
        let written = extract::byte_range(written).ok()?;
        (binding.kind == SyntaxExportKind::Default && binding.local == Some(written))
            .then_some(written)
    }

    /// The name an exported function assignment declares: the written name of a named
    /// function given to `module.exports`, else the export name.
    fn export_function_name<'text>(
        &self,
        assignment: Node<'_>,
        text: &'text str,
    ) -> Option<&'text str> {
        let name = self.kinds.export_function_name(assignment, text)?;
        let written = self.default_function_name(assignment).and_then(|range| {
            text.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
        });
        Some(written.unwrap_or(name))
    }

    /// The declaration an export assignment renders, exported when its assignment is.
    fn assigned_declaration(
        &self,
        assignment: Node<'_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let Some((name, kind, value)) = self.assigned_export(assignment, text) else {
            return Ok(None);
        };
        let body = if kind == EcmaScriptSymbolKind::Variable {
            Some(value)
        } else {
            self.kinds.child(value, EcmaScriptGrammarField::Body)
        };
        let mut kinds = kind.kinds();
        if self.exports.declared.contains(&assignment.id()) {
            kinds.push(SymbolKind::Public);
        }
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind: kind.word(),
            kinds,
            visibility: None,
            body_range: body.map(extract::byte_range).transpose()?,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        }))
    }

    /// The declaration an export assignment makes: its name, its kind and the value that
    /// declares it. A function is declared wherever it is written. A class or any other
    /// value is declared by a recorded export of the module alone.
    fn assigned_export<'tree, 'source>(
        &self,
        assignment: Node<'tree>,
        text: &'source str,
    ) -> Option<(&'source str, EcmaScriptSymbolKind, Node<'tree>)> {
        if assignment.kind_id() != self.kinds.assignment_expression {
            return None;
        }
        let binding = self.exports.bindings.get(&assignment.id());
        if let Some((_, value)) = self.kinds.export_definition(assignment, text) {
            let class = value.kind_id() == self.kinds.class_expression;
            if class && binding.is_none() {
                return None;
            }
            let kind = if class {
                EcmaScriptSymbolKind::Class
            } else {
                EcmaScriptSymbolKind::Function
            };
            return Some((self.export_function_name(assignment, text)?, kind, value));
        }
        let binding = binding.filter(|binding| {
            binding.kind == SyntaxExportKind::Named && binding.local == binding.exported
        })?;
        let left = self.kinds.child(assignment, EcmaScriptGrammarField::Left)?;
        // A write through an alias declares the member it writes, never a module name.
        self.kinds.export_target(left, text)?;
        let property = self.kinds.child(left, EcmaScriptGrammarField::Property)?;
        (binding.exported == extract::byte_range(property).ok()).then_some(())?;
        Some((
            text.get(property.byte_range())?,
            EcmaScriptSymbolKind::Variable,
            self.kinds.assigned_value(assignment)?,
        ))
    }

    /// The export statement directly carrying this declaration.
    fn declaration_export<'tree>(&self, visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
        let mut parent = visited.parent()?;
        if self.kinds.is_declaration_statement(parent.syntax_element()) {
            parent = parent.parent()?;
        }
        if self.kinds.ambient_declaration == Some(parent.syntax_element().kind_id()) {
            parent = parent.parent()?;
        }
        (parent.syntax_element().kind_id() == self.kinds.export_statement)
            .then(|| parent.syntax_element())
    }

    /// Whether a direct grammar token marks this export form.
    fn has_token(syntax_element: Node<'_>, tokens: &[u16]) -> bool {
        let mut cursor = syntax_element.walk();
        syntax_element
            .children(&mut cursor)
            .any(|child| tokens.contains(&child.kind_id()))
    }

    /// One clause's exact local and exported tokens, including a module source.
    fn specifier_binding(
        &self,
        visited: Visited<'_, '_>,
        qualification: &str,
    ) -> Result<Option<SyntaxExportBinding>, RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(statement) = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.export_clause)
            .and_then(Visited::parent)
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.export_statement)
            .map(Visited::syntax_element)
        else {
            return Ok(None);
        };
        let Some(local) = self
            .kinds
            .child(syntax_element, EcmaScriptGrammarField::Name)
        else {
            return Ok(None);
        };
        let exported = self
            .kinds
            .child(syntax_element, EcmaScriptGrammarField::Alias)
            .unwrap_or(local);
        Ok(Some(SyntaxExportBinding {
            kind: SyntaxExportKind::Named,
            range: extract::byte_range(statement)?,
            local: Some(extract::byte_range(local)?),
            exported: Some(extract::byte_range(exported)?),
            source: self
                .kinds
                .child(statement, EcmaScriptGrammarField::Source)
                .map(extract::byte_range)
                .transpose()?,
            container: (!qualification.is_empty()).then(|| qualification.to_owned()),
            type_only: Self::has_token(syntax_element, &self.kinds.type_tokens)
                || Self::has_token(statement, &self.kinds.type_tokens),
        }))
    }

    /// One declaration's export, without dropping later variable declarators.
    fn declaration_binding(
        &self,
        visited: Visited<'_, '_>,
        qualification: &str,
    ) -> Result<Option<SyntaxExportBinding>, RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(kind) = self.kinds.symbol_kind(syntax_element) else {
            return Ok(None);
        };
        let Some(statement) = self.declaration_export(visited) else {
            return Ok(None);
        };
        let local = self
            .kinds
            .child(syntax_element, EcmaScriptGrammarField::Name)
            .filter(|name| {
                kind != EcmaScriptSymbolKind::Variable || name.kind_id() == self.kinds.identifier
            })
            .map(extract::byte_range)
            .transpose()?;
        let is_default = kind != EcmaScriptSymbolKind::Variable
            && Self::has_token(statement, &[self.kinds.default_token]);
        if local.is_none() && !is_default {
            return Ok(None);
        }
        Ok(Some(SyntaxExportBinding {
            kind: if is_default {
                SyntaxExportKind::Default
            } else {
                SyntaxExportKind::Named
            },
            range: extract::byte_range(statement)?,
            local: local.or(if is_default {
                Some(extract::byte_range(syntax_element)?)
            } else {
                None
            }),
            exported: (!is_default).then_some(local).flatten(),
            source: None,
            container: (!qualification.is_empty()).then(|| qualification.to_owned()),
            type_only: matches!(
                kind,
                EcmaScriptSymbolKind::Interface | EcmaScriptSymbolKind::TypeAlias
            ),
        }))
    }

    /// Statement forms without a named clause or a direct declaration.
    fn statement_binding(
        &self,
        syntax_element: Node<'_>,
        qualification: &str,
    ) -> Result<Option<SyntaxExportBinding>, RiftError> {
        let source = self
            .kinds
            .child(syntax_element, EcmaScriptGrammarField::Source);
        let namespace = named_children(syntax_element)
            .find(|child| child.kind_id() == self.kinds.namespace_export);
        let is_default = Self::has_token(syntax_element, &[self.kinds.default_token]);
        let kind = if source.is_some() && namespace.is_some() {
            SyntaxExportKind::Namespace
        } else if source.is_some() && Self::has_token(syntax_element, &[self.kinds.star_token]) {
            SyntaxExportKind::All
        } else if is_default
            && !named_children(syntax_element).any(|child| self.kinds.symbol_kind(child).is_some())
        {
            SyntaxExportKind::Default
        } else {
            return Ok(None);
        };
        let local = (kind == SyntaxExportKind::Default)
            .then(|| {
                self.kinds
                    .child(syntax_element, EcmaScriptGrammarField::Value)
            })
            .flatten()
            .map(|value| self.kinds.expression_value(value));
        let exported = namespace.and_then(|namespace| namespace.named_child(0));
        Ok(Some(SyntaxExportBinding {
            kind,
            range: extract::byte_range(syntax_element)?,
            local: local.map(extract::byte_range).transpose()?,
            exported: exported.map(extract::byte_range).transpose()?,
            source: source.map(extract::byte_range).transpose()?,
            container: (!qualification.is_empty()).then(|| qualification.to_owned()),
            type_only: Self::has_token(syntax_element, &self.kinds.type_tokens),
        }))
    }

    /// The declared name's text: the grammar `name` field. A `variable`
    /// requires a plain identifier name; a destructuring pattern declares no
    /// single name.
    fn declaration_name(
        &self,
        syntax_element: Node<'_>,
        kind: EcmaScriptSymbolKind,
        text: &str,
    ) -> Option<String> {
        let name = self.kinds.name_syntax_element(syntax_element)?;
        if kind == EcmaScriptSymbolKind::Variable && name.kind_id() != self.kinds.identifier {
            return None;
        }
        text.get(name.byte_range()).map(Into::into)
    }

    fn parameter_syntax<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
    ) -> Option<(Node<'tree>, &'static str)> {
        type_parameters::declared_name(visited)
            .map(|name| (name, TYPE_PARAMETER_KIND))
            .or_else(|| {
                type_parameters::named_owner(visited, type_parameters::function)
                    .map(|name| (name, FUNCTION_TYPE_PARAMETER_KIND))
            })
            .or_else(|| {
                type_parameters::named_owner(visited, type_parameters::method)
                    .map(|name| (name, METHOD_TYPE_PARAMETER_KIND))
            })
            .or_else(|| {
                type_parameters::named_owner(visited, type_signature_parameters::callable_signature)
                    .map(|name| (name, SIGNATURE_TYPE_PARAMETER_KIND))
            })
            .or_else(|| {
                type_signature_parameters::callable_parameter_name(visited)
                    .map(|name| (name, CALLABLE_TYPE_PARAMETER_KIND))
            })
            .or_else(|| {
                type_signature_parameters::index_parameter_name(visited)
                    .map(|name| (name, INDEX_SIGNATURE_PARAMETER_KIND))
            })
            .or_else(|| {
                if self.kinds.formal_parameter_kinds.is_empty() {
                    let written = visited.syntax_element();
                    wrapped_parameters::declared_name(written)
                        .map(|name| (name, WRAPPED_PARAMETER_KIND))
                        .or_else(|| {
                            wrapped_parameters::destructured_name(written, false)
                                .map(|name| (name, DESTRUCTURED_PARAMETER_KIND))
                        })
                } else {
                    wrapped_parameters::destructured_name(visited.syntax_element(), true)
                        .map(|name| (name, TYPED_DESTRUCTURED_PARAMETER_KIND))
                }
            })
            .or_else(|| {
                self.kinds
                    .ordinary_parameter(visited)
                    .map(|(name, method)| {
                        let kind = if method {
                            METHOD_PARAMETER_KIND
                        } else {
                            PARAMETER_KIND
                        };
                        (name, kind)
                    })
            })
    }

    fn parameter_declaration(&self, visited: Visited<'_, '_>, text: &str) -> Option<Declaration> {
        let (name, kind) = self.parameter_syntax(visited)?;
        let name = text.get(name.byte_range())?;
        let type_parameter = matches!(
            kind,
            TYPE_PARAMETER_KIND
                | METHOD_TYPE_PARAMETER_KIND
                | FUNCTION_TYPE_PARAMETER_KIND
                | SIGNATURE_TYPE_PARAMETER_KIND
        );
        if !name.is_ascii() || name.contains('\\') || (!type_parameter && name == "this") {
            return None;
        }
        let kinds = if type_parameter {
            vec![SymbolKind::Type, SymbolKind::TypeParameter]
        } else {
            vec![SymbolKind::Value, SymbolKind::Parameter]
        };
        Some(Declaration {
            name: name.to_owned(),
            kind,
            kinds,
            visibility: None,
            body_range: None,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        })
    }

    /// One declaration the walk reaches through a syntax element outside the kind
    /// table: a name a destructuring pattern binds, a constructor parameter
    /// property, an enum member without a value, or the anonymous class a
    /// module exports by default. `None` for every other syntax element.
    fn bound_declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let syntax_element = visited.syntax_element();
        if let Some(parameter) = self.parameter_declaration(visited, text) {
            return Ok(Some(parameter));
        }
        let parent = visited.parent().map(Visited::syntax_element);
        let (name, kind, kinds) = if let Some(declarator) = self.kinds.pattern_declarator(visited) {
            let name = text.get(syntax_element.byte_range()).unwrap_or_default();
            let mut kinds = EcmaScriptSymbolKind::Variable.kinds();
            let named = self.exports.names.contains(name) && self.at_module_scope(declarator);
            if self.wrapped_by_export(declarator) || named {
                kinds.push(SymbolKind::Public);
            }
            (name, EcmaScriptSymbolKind::Variable, kinds)
        } else if parent.is_some_and(|parent| {
            parent.kind_id() == self.kinds.catch_clause
                && parent.child_by_field_id(self.kinds.parameter.get()) == Some(syntax_element)
        }) {
            let name = text.get(syntax_element.byte_range()).unwrap_or_default();
            (
                name,
                EcmaScriptSymbolKind::Variable,
                EcmaScriptSymbolKind::Variable.kinds(),
            )
        } else if let Some(name) = self.kinds.parameter_property(visited, text) {
            let name = text.get(name.byte_range()).unwrap_or_default();
            (
                name,
                EcmaScriptSymbolKind::Field,
                EcmaScriptSymbolKind::Field.kinds(),
            )
        } else if self.kinds.is_bare_enum_member(syntax_element, parent) {
            let name = text.get(syntax_element.byte_range()).unwrap_or_default();
            let kind = EcmaScriptSymbolKind::EnumMember;
            (name, kind, kind.kinds())
        } else if self.exports.default_values.contains(&syntax_element.id()) {
            let kind = if syntax_element.kind_id() == self.kinds.class_expression {
                EcmaScriptSymbolKind::Class
            } else {
                EcmaScriptSymbolKind::Function
            };
            let mut kinds = kind.kinds();
            kinds.push(SymbolKind::Public);
            (DEFAULT_EXPORT_NAME, kind, kinds)
        } else {
            return Ok(None);
        };
        if name.is_empty() {
            return Ok(None);
        }
        let (documentation, documentation_ranges) =
            self.attached_jsdoc(self.statement(visited), text)?;
        let body_range = if matches!(
            kind,
            EcmaScriptSymbolKind::Class | EcmaScriptSymbolKind::Function
        ) {
            self.body_range(syntax_element, kind)?
        } else {
            None
        };
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind: kind.word(),
            kinds,
            visibility: self.accessibility(syntax_element, text),
            body_range,
            documentation,
            documentation_ranges,
        }))
    }

    /// Whether the module exports the declaration `syntax_element` names `name`: an
    /// `export_statement` wraps it, an export names it at module scope, or it
    /// is a method of an object literal the module exports whole.
    fn exported(&self, visited: Visited<'_, '_>, name: &str) -> bool {
        let wrapped = self.wrapped_by_export(visited);
        let named = self.exports.names.contains(name) && self.at_module_scope(visited);
        let in_exported_object = visited.parent().is_some_and(|parent| {
            let object = parent.syntax_element().id();
            self.exports.objects.contains(&object) || self.exports.members.contains(&object)
        });
        wrapped || named || in_exported_object
    }

    /// The statement carrying `syntax element`: the declaration itself, the
    /// `lexical_declaration` or `variable_declaration` holding it as its
    /// first declarator, its TypeScript `ambient_declaration`, the
    /// `export_statement` wrapping them, and the decorators the TypeScript
    /// grammar writes as siblings before a method. A
    /// `JSDoc` block written above `export` or `const` stands before this
    /// syntax element, since the grammar makes `export` the declaration's previous
    /// sibling.
    fn statement<'walk, 'tree>(&self, visited: Visited<'walk, 'tree>) -> Visited<'walk, 'tree> {
        let mut front = visited;
        if let Some(parent) = front.parent().filter(|parent| {
            self.kinds.is_declaration_statement(parent.syntax_element())
                && parent.syntax_element().named_child(0) == Some(front.syntax_element())
        }) {
            front = parent;
        }
        if let Some(parent) = front.parent().filter(|parent| {
            self.kinds.ambient_declaration == Some(parent.syntax_element().kind_id())
        }) {
            front = parent;
        }
        if let Some(parent) = front
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.export_statement)
        {
            front = parent;
        }
        while let Some(previous) = front
            .previous_sibling()
            .filter(|previous| previous.syntax_element().kind_id() == self.kinds.decorator)
        {
            front = previous;
        }
        front
    }

    /// The `JSDoc` comments directly attached before `front`, nearest first:
    /// a run of `/**` comment siblings, each separated from the next by
    /// whitespace holding at most one line break. Each step reads the previous
    /// sibling the shared walk recorded, so the walk costs the run's length.
    fn jsdoc_run<'tree>(&self, front: Visited<'_, 'tree>, text: &str) -> Vec<Node<'tree>> {
        let mut run = Vec::new();
        let mut front = front;
        while let Some(previous) = front.previous_sibling() {
            if !self.attaches(previous.syntax_element(), front.syntax_element(), text) {
                break;
            }
            run.push(previous.syntax_element());
            front = previous;
        }
        run
    }

    /// Whether `previous` is a `JSDoc` comment attached to `front`: a `/**`
    /// block separated from it by whitespace holding at most one line break.
    fn attaches(&self, previous: Node<'_>, front: Node<'_>, text: &str) -> bool {
        let is_jsdoc = previous.kind_id() == self.kinds.comment
            && text
                .get(previous.byte_range())
                .is_some_and(|comment| comment.trim_start().starts_with(JSDOC_OPEN));
        let adjacent = text
            .get(previous.end_byte()..front.start_byte())
            .is_some_and(|gap| {
                gap.chars().all(char::is_whitespace)
                    && gap.bytes().filter(|byte| *byte == LINE_FEED).count() <= 1
            });
        is_jsdoc && adjacent
    }

    /// The attached `JSDoc` before `statement` as one documentation block, and
    /// each comment's range; empty when nothing attaches or every block is
    /// empty once its markers are stripped.
    fn attached_jsdoc(
        &self,
        statement: Visited<'_, '_>,
        text: &str,
    ) -> Result<(Vec<Documentation>, Vec<ByteRange>), RiftError> {
        let mut run = self.jsdoc_run(statement, text);
        run.reverse();
        let mut blocks = Vec::with_capacity(run.len());
        let mut ranges = Vec::with_capacity(run.len());
        for comment in run {
            let written = text
                .get(comment.byte_range())
                .map(jsdoc_text)
                .unwrap_or_default();
            if !written.is_empty() {
                blocks.push(written);
            }
            ranges.push(extract::byte_range(comment)?);
        }
        if blocks.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        let documentation = Documentation {
            format: DocumentationFormat::Markdown,
            text: blocks.join(LineEnding::Lf.as_str()),
        };
        Ok((vec![documentation], ranges))
    }

    /// Whether an `export_statement` wraps the declaration: its direct
    /// parent, or the declaration and TypeScript `declare` wrappers
    /// directly carrying it.
    fn wrapped_by_export(&self, visited: Visited<'_, '_>) -> bool {
        self.declaration_export(visited).is_some()
    }

    /// Whether `syntax element` declares at module scope: its statement - the syntax element
    /// itself, or a declarator's declaration statement - sits directly in the
    /// program.
    fn at_module_scope(&self, visited: Visited<'_, '_>) -> bool {
        let statement = visited
            .parent()
            .filter(|parent| self.kinds.is_declaration_statement(parent.syntax_element()))
            .unwrap_or(visited);
        statement
            .parent()
            .is_some_and(|parent| parent.syntax_element().kind_id() == self.kinds.program)
    }

    /// The authored `accessibility_modifier` text on `syntax element`; `None` when the
    /// grammar spells none or the declaration carries none.
    fn accessibility(&self, syntax_element: Node<'_>, text: &str) -> Option<String> {
        let modifier = self.kinds.accessibility_modifier?;
        let mut cursor = syntax_element.walk();
        syntax_element
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == modifier)
            .and_then(|child| text.get(child.byte_range()))
            .map(Into::into)
    }

    /// The kind's body or value field span; `None` when this syntax element omits the
    /// field - a bodyless signature, a valueless declarator.
    fn body_range(
        &self,
        syntax_element: Node<'_>,
        kind: EcmaScriptSymbolKind,
    ) -> Result<Option<ByteRange>, RiftError> {
        let Some(body) =
            syntax_element.child_by_field_id(self.kinds.field(kind.body_field()).get())
        else {
            return Ok(None);
        };
        extract::byte_range(body).map(Some)
    }
}

impl GrammarRules for EcmaScriptRules<'_> {
    fn export_binding(
        &self,
        visited: Visited<'_, '_>,
        _text: &str,
        qualification: &str,
    ) -> Result<Option<SyntaxExportBinding>, RiftError> {
        let syntax_element = visited.syntax_element();
        if let Some(binding) = self.exports.bindings.get(&syntax_element.id()) {
            Ok(Some(binding.clone()))
        } else if syntax_element.kind_id() == self.kinds.export_specifier {
            self.specifier_binding(visited, qualification)
        } else if syntax_element.kind_id() == self.kinds.export_statement {
            self.statement_binding(syntax_element, qualification)
        } else {
            self.declaration_binding(visited, qualification)
        }
    }

    fn name_range(&self, syntax_element: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        if self.exports.objects.contains(&syntax_element.id()) {
            return extract::byte_range(syntax_element).map(Some);
        }
        if let Some(written) = self.default_function_name(syntax_element) {
            return Ok(Some(written));
        }
        let named_by_itself = syntax_element.kind_id() == self.kinds.identifier
            || syntax_element.kind_id() == self.kinds.shorthand_property_identifier_pattern
            || syntax_element.kind_id() == self.kinds.property_identifier;
        if named_by_itself {
            return extract::byte_range(syntax_element).map(Some);
        }
        let parameter_name = self
            .kinds
            .formal_parameter_kinds
            .contains(&syntax_element.kind_id())
            .then_some(self.kinds.pattern)
            .flatten()
            .and_then(|pattern| syntax_element.child_by_field_id(pattern.get()));
        let wrapped_parameter = self
            .kinds
            .formal_parameter_kinds
            .is_empty()
            .then(|| wrapped_parameters::declared_name(syntax_element))
            .flatten();
        let name = self
            .kinds
            .name_syntax_element(syntax_element)
            .or(parameter_name)
            .or(wrapped_parameter)
            .or_else(|| {
                (syntax_element.kind_id() == self.kinds.assignment_expression)
                    .then(|| {
                        self.kinds
                            .child(syntax_element, EcmaScriptGrammarField::Left)
                    })
                    .flatten()
                    .and_then(|left| self.kinds.child(left, EcmaScriptGrammarField::Property))
            });
        name.map(extract::byte_range).transpose()
    }

    fn renders_signature(&self, visited: Visited<'_, '_>) -> bool {
        self.kinds.symbol_kind(visited.syntax_element()) != Some(EcmaScriptSymbolKind::Variable)
    }

    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let syntax_element = visited.syntax_element();
        if self.exports.objects.contains(&syntax_element.id()) {
            return Ok(Some(Declaration {
                name: DEFAULT_EXPORT_NAME.to_owned(),
                kind: OBJECT_KIND,
                kinds: vec![
                    SymbolKind::Value,
                    SymbolKind::MemberContainer,
                    SymbolKind::Public,
                ],
                visibility: None,
                body_range: Some(extract::byte_range(syntax_element)?),
                documentation: Vec::new(),
                documentation_ranges: Vec::new(),
            }));
        }
        if let Some(declaration) = self.assigned_declaration(syntax_element, text)? {
            return Ok(Some(declaration));
        }
        let Some(kind) = self.kinds.symbol_kind(syntax_element) else {
            return self.bound_declaration(visited, text);
        };
        if self.kinds.is_type_literal_member(visited) {
            return Ok(None);
        }
        let Some(name) = self.declaration_name(syntax_element, kind, text) else {
            return Ok(None);
        };
        let mut kinds = kind.kinds();
        if matches!(
            kind,
            EcmaScriptSymbolKind::Variable
                | EcmaScriptSymbolKind::Field
                | EcmaScriptSymbolKind::Property
        ) && self.kinds.declaration_type.is_some_and(|field| {
            syntax_element
                .child_by_field_id(field.get())
                .is_some_and(|annotation| {
                    Some(annotation.kind_id()) == self.kinds.type_annotation
                        && annotation.named_child(0).is_some_and(|declared| {
                            Some(declared.kind_id()) == self.kinds.function_type
                        })
                })
        }) {
            kinds.push(SymbolKind::Callable);
        }
        if matches!(
            kind,
            EcmaScriptSymbolKind::Variable
                | EcmaScriptSymbolKind::Field
                | EcmaScriptSymbolKind::Property
        ) && !kinds.contains(&SymbolKind::Callable)
            && self
                .kinds
                .child(syntax_element, EcmaScriptGrammarField::Value)
                .is_some_and(|value| {
                    self.kinds
                        .function_values
                        .contains(&self.kinds.asserted_value(value).kind_id())
                })
        {
            kinds.push(SymbolKind::Callable);
        }
        if Self::has_token(syntax_element, &[self.kinds.static_token]) {
            kinds.push(SymbolKind::Static);
        }
        if self
            .kinds
            .abstract_declarations
            .contains(&syntax_element.kind_id())
        {
            kinds.push(SymbolKind::Abstract);
        }
        if self.exported(visited, &name) {
            kinds.push(SymbolKind::Public);
        }
        let (documentation, documentation_ranges) =
            self.attached_jsdoc(self.statement(visited), text)?;
        Ok(Some(Declaration {
            name,
            kind: kind.word(),
            kinds,
            visibility: self.accessibility(syntax_element, text),
            body_range: self.body_range(syntax_element, kind)?,
            documentation,
            documentation_ranges,
        }))
    }

    fn child_qualification(&self, syntax_element: Node<'_>, text: &str, parent: &str) -> String {
        if self.kinds.is_global_augmentation(syntax_element) {
            return GLOBAL_AUGMENTATION_NAME.to_owned();
        }
        self.container_name(syntax_element, text).map_or_else(
            || parent.to_owned(),
            |name| extract::qualify(self.qualification_separator(), parent, &name),
        )
    }

    fn container_name(&self, syntax_element: Node<'_>, text: &str) -> Option<String> {
        if let Some((name, _, _)) = self.assigned_export(syntax_element, text) {
            return Some(if self.exports.shadowing.contains(&syntax_element.id()) {
                extract::qualify(self.qualification_separator(), EXPORTS_NAME, name)
            } else {
                name.to_owned()
            });
        }
        if self.exports.objects.contains(&syntax_element.id()) {
            return Some(DEFAULT_EXPORT_NAME.to_owned());
        }
        if self.exports.default_values.contains(&syntax_element.id()) {
            return Some(DEFAULT_EXPORT_NAME.to_owned());
        }
        let kind = self.kinds.symbol_kind(syntax_element)?;
        if !kind.opens_scope() {
            return None;
        }
        let name = self.kinds.name_syntax_element(syntax_element)?;
        if kind == EcmaScriptSymbolKind::Variable && name.kind_id() != self.kinds.identifier {
            return None;
        }
        text.get(name.byte_range()).map(Into::into)
    }

    /// A constructor parameter property belongs to the class: it drops the
    /// `constructor` scope its parameter list sits in. An export assignment whose name a
    /// declaration at module scope takes belongs to `exports`.
    fn declaration_qualification(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
        qualification: &str,
    ) -> Option<String> {
        if self
            .exports
            .shadowing
            .contains(&visited.syntax_element().id())
        {
            return Some(extract::qualify(
                self.qualification_separator(),
                qualification,
                EXPORTS_NAME,
            ));
        }
        self.kinds.parameter_property(visited, text)?;
        let class = qualification
            .strip_suffix(CONSTRUCTOR_NAME)?
            .strip_suffix(self.qualification_separator())?;
        Some(class.to_owned())
    }

    fn declaration_syntax_element<'tree>(&self, visited: Visited<'_, 'tree>) -> Node<'tree> {
        self.parameter_syntax(visited)
            .map_or_else(|| visited.syntax_element(), |(name, _)| name)
    }

    /// Starts the declaration at the statement carrying it, extended over the
    /// `JSDoc` comments attached before that statement.
    fn declaration_start(&self, visited: Visited<'_, '_>, text: &str) -> usize {
        if let Some((name, _)) = self.parameter_syntax(visited) {
            return name.start_byte();
        }
        let statement = self.statement(visited);
        self.jsdoc_run(statement, text)
            .last()
            .map_or(statement.syntax_element().start_byte(), Node::start_byte)
    }

    fn qualification_separator(&self) -> &'static str {
        "."
    }
}

/// Reads the export statements whose owner is an authored global augmentation.
/// The global scope has no retained declaration to stand in for the source witness.
pub(crate) fn global_export_ranges(
    dialect: crate::TypeScriptDialect,
    source: &str,
    limits: SyntaxLimits,
) -> Result<BTreeSet<ByteRange>, RiftError> {
    let grammar = dialect.grammar();
    let kinds = dialect.kinds();
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|_| errors::syntax::facts_kind_invalid().error())?;
    let (tree, _) = recovery::parse(&mut parser, source)
        .ok_or_else(|| errors::syntax::facts_structure_invalid().error())?;
    let root = tree.root_node();
    let mut ranges = BTreeSet::new();
    let mut pending = vec![(root, 0_usize, false, false)];
    let mut cursor = root.walk();
    let mut visited = 0;
    while let Some((syntax_element, depth, parent_is_global, parent_is_global_body)) = pending.pop()
    {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::facts_depth_exceeded().fail();
        }
        if visited + pending.len() + syntax_element.named_child_count()
            > limits.syntax_elements_max()
        {
            return errors::syntax::facts_count_exceeded().fail();
        }
        if parent_is_global_body && syntax_element.kind_id() == kinds.export_statement {
            ranges.insert(extract::byte_range(syntax_element)?);
        }
        let global = kinds.is_global_augmentation(syntax_element);
        let global_body = parent_is_global && syntax_element.kind() == "statement_block";
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| {
            (child, depth + 1, global, global_body)
        });
    }
    Ok(ranges)
}

/// One `/** ... */` comment's written text: the delimiters, each line's
/// leading whitespace and `*`, and one space after it stripped, and blank
/// edge lines dropped. `@` tags stay as written.
fn jsdoc_text(comment: &str) -> String {
    let inner = comment
        .trim()
        .strip_prefix(JSDOC_OPEN)
        .and_then(|rest| rest.strip_suffix(BLOCK_COMMENT_CLOSE))
        .unwrap_or_default();
    let lines: Vec<&str> = lines_inclusive(inner)
        .map(|line| {
            let line = without_ending(line).trim_start();
            let line = line.strip_prefix(JSDOC_LINE_MARKER).unwrap_or(line);
            line.strip_prefix(' ').unwrap_or(line).trim_end()
        })
        .collect();
    let first = lines.iter().position(|line| !line.is_empty());
    let last = lines.iter().rposition(|line| !line.is_empty());
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].join(LineEnding::Lf.as_str()),
        _ => String::new(),
    }
}

/// Parses one source through a pinned ECMAScript grammar and extracts its
/// named syntax elements and declarations, the shared `analyze` behind all three
/// providers.
///
/// # Errors
///
/// Returns [`RiftError`] for an oversized source, an incompatible
/// grammar, cancellation, or an exceeded tree bound.
pub(crate) fn analyze(
    language: &Language,
    grammar: &tree_sitter::Language,
    kinds: &'static EcmaScriptKinds,
    limits: SyntaxLimits,
    source: SyntaxSource<'_>,
) -> Result<SyntaxDocument, RiftError> {
    analyze_included(language, grammar, kinds, limits, source, &[])
}

/// Parses original source bytes within ordered ranges supplied by the host grammar.
pub(crate) fn analyze_included(
    language: &Language,
    grammar: &tree_sitter::Language,
    kinds: &'static EcmaScriptKinds,
    limits: SyntaxLimits,
    source: SyntaxSource<'_>,
    ranges: &[tree_sitter::Range],
) -> Result<SyntaxDocument, RiftError> {
    if source.text.len() > limits.source_bytes_max() {
        return errors::syntax::source_too_large()
            .path(source.path)
            .source_bytes(source.text.len())
            .source_bytes_max(limits.source_bytes_max())
            .fail();
    }
    let mut parser = parser_for_ranges(grammar, ranges)?;
    // A host grammar's ranges already select what this parser reads.
    let parsed = if ranges.is_empty() {
        recovery::parse(&mut parser, source.text)
    } else {
        parser.parse(source.text, None).map(|tree| (tree, false))
    };
    let (tree, recovered) =
        parsed.ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let mut rules = EcmaScriptRules {
        kinds,
        exports: ModuleExports::read(tree.root_node(), source.text, kinds)?,
    };
    let commonjs = commonjs::record(
        tree.root_node(),
        source,
        limits,
        kinds,
        language.name == "javascript"
            && ranges.is_empty()
            && !recovered
            && !tree.root_node().has_error(),
        &mut rules.exports.bindings,
    )?;
    rules.exports.add_commonjs(&commonjs, source.text);
    let mut capture = ranges.is_empty().then(|| {
        if language.name == "typescript" {
            crate::static_analysis::Capture::for_edge_typescript(
                source.text,
                match language.dialect.as_deref() {
                    Some("tsx") => crate::typescript::TypeScriptDialect::Tsx,
                    _ => crate::typescript::TypeScriptDialect::TypeScript,
                },
            )
        } else {
            crate::static_analysis::Capture::for_javascript(source.text)
        }
    });
    if let Some(capture) = &mut capture
        && !recovered
        && !tree.root_node().has_error()
    {
        capture.record_commonjs_export(commonjs.closed_export);
    }
    let ((syntax_elements, mut symbols, export_bindings), visits) = extract::extract_with_visits(
        tree.root_node(),
        source,
        limits,
        language,
        &rules,
        capture.as_mut(),
    )?;
    let parameters = take_parameters(&mut symbols);
    let mut static_facts = capture.map(crate::static_analysis::Capture::finish);
    if let Some(facts) = &mut static_facts {
        let aliases = members::record(
            &rules,
            source.text,
            language,
            facts,
            &visits,
            &commonjs.assignments,
            &mut symbols,
        )?;
        facts.set_member_aliases(aliases);
        if !recovered && !tree.root_node().has_error() {
            append_package_parameters(
                language,
                tree.root_node(),
                source.text,
                &mut symbols,
                parameters,
                facts,
            );
        }
    }
    symbols.sort_by_key(|symbol| symbol.range.start);
    let variance_errors_only = language.name == "typescript"
        && ranges.is_empty()
        && !recovered
        && tree.root_node().has_error()
        && variance::accounts_for_parse_errors(&mut parser, tree.root_node(), source.text, limits);
    Ok(SyntaxDocument::new(
        language.clone(),
        source.path.clone(),
        syntax_elements,
        symbols,
        // A recovered tree holds no error node; the source still failed to parse whole.
        recovered || tree.root_node().has_error(),
    )
    .with_source_witness(source.text)
    .with_variance_errors_only(variance_errors_only)
    .with_export_bindings(export_bindings)
    .with_static_facts(static_facts)
    .with_syntax_limits(limits))
}

fn parser_for_ranges(
    grammar: &tree_sitter::Language,
    ranges: &[tree_sitter::Range],
) -> Result<Parser, RiftError> {
    let mut parser = Parser::new();
    parser.set_language(grammar).map_err(|_| {
        errors::syntax::incompatible_grammar()
            .grammar_abi_version(grammar.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error()
    })?;
    assert!(
        parser.set_included_ranges(ranges).is_ok(),
        "host grammar ranges must be ordered and non-overlapping: ranges={ranges:?}",
    );
    Ok(parser)
}

fn append_package_parameters(
    language: &Language,
    root: Node<'_>,
    source: &str,
    symbols: &mut Vec<crate::SyntaxSymbol>,
    candidates: ParameterDeclarations,
    facts: &crate::StaticFacts,
) {
    let ParameterDeclarations {
        values: parameters,
        types: type_parameters,
        callable_types,
        index_signatures,
        wrapped,
        destructured,
        method_types,
        function_types,
        signature_types,
        typed_patterns,
    } = candidates;
    let scoped = if language.name == "typescript" {
        overload_parameters::capture(symbols, &parameters, facts, root)
    } else {
        scoped_function_parameters::capture(symbols, &parameters, facts)
    };
    let shorthand = if language.name == "javascript" {
        shorthand_parameters::capture(symbols, &parameters, facts)
    } else {
        Vec::new()
    };
    append_parameters(symbols, parameters, facts);
    symbols.extend(scoped);
    symbols.extend(shorthand);
    callback_parameters::append(symbols, facts, source, language.name == "javascript");
    checked_parameters::append_under_named_owner(
        symbols,
        type_parameters,
        facts,
        type_parameters::structured,
    );
    type_signature_parameters::append_callable_types(root, symbols, callable_types, facts);
    type_signature_parameters::append_indexes(root, symbols, index_signatures);
    checked_parameters::append_under_named_owner(
        symbols,
        wrapped,
        facts,
        wrapped_parameters::accepted_owner,
    );
    checked_parameters::append_under_named_owner(
        symbols,
        destructured,
        facts,
        wrapped_parameters::accepted_owner,
    );
    checked_parameters::append_under_named_owner(
        symbols,
        method_types,
        facts,
        type_parameters::method,
    );
    checked_parameters::append_under_named_owner(
        symbols,
        function_types,
        facts,
        type_parameters::function,
    );
    type_signature_parameters::append_callable_type_parameters(
        root,
        symbols,
        signature_types,
        facts,
    );
    checked_parameters::append_under_named_owner(
        symbols,
        typed_patterns,
        facts,
        wrapped_parameters::accepted_owner,
    );
}

#[derive(Default)]
struct ParameterDeclarations {
    values: Vec<crate::SyntaxSymbol>,
    types: Vec<crate::SyntaxSymbol>,
    callable_types: Vec<crate::SyntaxSymbol>,
    index_signatures: Vec<crate::SyntaxSymbol>,
    wrapped: Vec<crate::SyntaxSymbol>,
    destructured: Vec<crate::SyntaxSymbol>,
    function_types: Vec<crate::SyntaxSymbol>,
    signature_types: Vec<crate::SyntaxSymbol>,
    typed_patterns: Vec<crate::SyntaxSymbol>,
    method_types: Vec<crate::SyntaxSymbol>,
}

/// Holds optional parameters aside while existing member facts are resolved.
fn take_parameters(symbols: &mut Vec<crate::SyntaxSymbol>) -> ParameterDeclarations {
    let mut candidates = ParameterDeclarations::default();
    for symbol in symbols.extract_if(.., |symbol| symbol.is_ecmascript_package_binding()) {
        let class = if symbol.is_typescript_type_parameter() {
            &mut candidates.types
        } else if symbol.is_typescript_callable_type_parameter() {
            &mut candidates.callable_types
        } else if symbol.is_typescript_index_signature_parameter() {
            &mut candidates.index_signatures
        } else if symbol.is_javascript_wrapped_parameter() {
            &mut candidates.wrapped
        } else if symbol.is_javascript_destructured_parameter() {
            &mut candidates.destructured
        } else if symbol.is_typescript_method_type_parameter() {
            &mut candidates.method_types
        } else if symbol.is_typescript_function_type_parameter() {
            &mut candidates.function_types
        } else if symbol.is_typescript_signature_type_parameter() {
            &mut candidates.signature_types
        } else if symbol.is_typescript_destructured_parameter() {
            &mut candidates.typed_patterns
        } else {
            &mut candidates.values
        };
        class.push(symbol);
    }
    candidates
}

/// An ordinary parameter adds no competing facet of an existing lexical declaration.
fn append_parameters(
    symbols: &mut Vec<crate::SyntaxSymbol>,
    parameters: Vec<crate::SyntaxSymbol>,
    facts: &crate::StaticFacts,
) {
    if parameters.is_empty() {
        return;
    }
    let mut counts = BTreeMap::new();
    for symbol in symbols.iter().chain(&parameters) {
        *counts
            .entry(symbol.qualified_name.as_str())
            .or_insert(0_usize) += 1;
    }
    let signatures = symbols
        .iter()
        .filter(|symbol| symbol.syntax_element_kind == Some(FUNCTION_SIGNATURE_KIND))
        .filter_map(|symbol| symbol.name_range)
        .collect::<BTreeSet<_>>();
    let anchors = facts
        .parameters()
        .iter()
        .filter_map(|parameter| Some((parameter.name, parameter.owner?)))
        .collect::<BTreeMap<_, _>>();
    let retained = parameters
        .iter()
        .filter(|symbol| {
            counts.get(symbol.qualified_name.as_str()) == Some(&1)
                && symbol
                    .container
                    .as_deref()
                    .and_then(|owner| counts.get(owner))
                    == Some(&1)
        })
        .filter(|symbol| rift_core::is_portable_name(&symbol.qualified_name))
        .filter(|symbol| {
            symbol
                .name_range
                .is_some_and(|name| anchors.contains_key(&name))
        })
        .filter(|symbol| rift_core::is_portable_name(&symbol.name))
        .map(|symbol| symbol.name_range)
        .collect::<BTreeSet<_>>();
    symbols.extend(parameters.into_iter().filter_map(|mut symbol| {
        if !retained.contains(&symbol.name_range) {
            return None;
        }
        if signatures.contains(anchors.get(&symbol.name_range?)?) {
            symbol.kind = SIGNATURE_PARAMETER_KIND;
        }
        Some(symbol)
    }));
}

/// Portable structural kinds for one ECMAScript grammar syntax element kind, shared
/// by all three providers: every declaration kind the rules interpret maps
/// to `Declaration`, and the grammar's suffix conventions classify the rest.
pub(crate) fn syntax_element_kinds(kind: &str) -> Vec<SyntaxKind> {
    match kind {
        FUNCTION_DECLARATION_KIND
        | GENERATOR_FUNCTION_DECLARATION_KIND
        | CLASS_DECLARATION_KIND
        | METHOD_DEFINITION_KIND
        | VARIABLE_DECLARATOR_KIND
        | INTERFACE_DECLARATION_KIND
        | ENUM_DECLARATION_KIND
        | TYPE_ALIAS_DECLARATION_KIND
        | INTERNAL_MODULE_KIND
        | MODULE_KIND
        | FIELD_DEFINITION_KIND
        | PUBLIC_FIELD_DEFINITION_KIND
        | ABSTRACT_CLASS_DECLARATION_KIND
        | ENUM_ASSIGNMENT_KIND => vec![SyntaxKind::Declaration, SyntaxKind::Definition],
        FUNCTION_SIGNATURE_KIND
        | METHOD_SIGNATURE_KIND
        | PROPERTY_SIGNATURE_KIND
        | ABSTRACT_METHOD_SIGNATURE_KIND => vec![SyntaxKind::Declaration],
        LEXICAL_DECLARATION_KIND | VARIABLE_DECLARATION_KIND => {
            vec![SyntaxKind::Declaration, SyntaxKind::Statement]
        }
        EXPORT_STATEMENT_KIND => vec![SyntaxKind::Export, SyntaxKind::Statement],
        "import_statement" => vec![SyntaxKind::Import, SyntaxKind::Statement],
        "statement_block" => vec![SyntaxKind::Block],
        CLASS_BODY_KIND | INTERFACE_BODY_KIND | "enum_body" => vec![SyntaxKind::Body],
        "required_parameter" | "optional_parameter" => vec![SyntaxKind::Parameter],
        "decorator" => vec![SyntaxKind::Annotation],
        "comment" | "html_comment" => vec![SyntaxKind::Comment],
        "type_annotation" => vec![SyntaxKind::TypeExpression],
        "arrow_function" | "jsx_element" | "jsx_self_closing_element" => {
            vec![SyntaxKind::Expression]
        }
        suffixed => suffix_kinds(suffixed),
    }
}

/// The grammar's spelling conventions for kinds outside the named table.
fn suffix_kinds(kind: &str) -> Vec<SyntaxKind> {
    let mut kinds = Vec::new();
    if kind.ends_with("_statement") {
        kinds.push(SyntaxKind::Statement);
    }
    if kind.ends_with("_expression") {
        kinds.push(SyntaxKind::Expression);
    }
    if kind.ends_with("_type") {
        kinds.push(SyntaxKind::TypeExpression);
    }
    kinds
}

pub(crate) fn restored_symbol_kind(kinds: &EcmaScriptKinds, name: &str) -> Option<&'static str> {
    if name == OBJECT_KIND {
        return Some(OBJECT_KIND);
    }
    if name == PARAMETER_KIND {
        return Some(PARAMETER_KIND);
    }
    if name == CALLBACK_PARAMETER_KIND {
        return Some(CALLBACK_PARAMETER_KIND);
    }
    if name == CALLBACK_PATTERN_PARAMETER_KIND {
        return Some(CALLBACK_PATTERN_PARAMETER_KIND);
    }
    if name == OVERLOAD_PARAMETER_KIND {
        return Some(OVERLOAD_PARAMETER_KIND);
    }
    if name == METHOD_PARAMETER_KIND {
        return Some(METHOD_PARAMETER_KIND);
    }
    if name == SIGNATURE_PARAMETER_KIND {
        return Some(SIGNATURE_PARAMETER_KIND);
    }
    if name == SCOPED_FUNCTION_PARAMETER_KIND {
        return Some(SCOPED_FUNCTION_PARAMETER_KIND);
    }
    if name == SHORTHAND_PARAMETER_KIND && kinds.formal_parameter_kinds.is_empty() {
        return Some(SHORTHAND_PARAMETER_KIND);
    }
    if name == CALLABLE_TYPE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(CALLABLE_TYPE_PARAMETER_KIND);
    }
    if name == INDEX_SIGNATURE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(INDEX_SIGNATURE_PARAMETER_KIND);
    }
    if name == WRAPPED_PARAMETER_KIND && kinds.formal_parameter_kinds.is_empty() {
        return Some(WRAPPED_PARAMETER_KIND);
    }
    if name == DESTRUCTURED_PARAMETER_KIND && kinds.formal_parameter_kinds.is_empty() {
        return Some(DESTRUCTURED_PARAMETER_KIND);
    }
    if name == METHOD_TYPE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(METHOD_TYPE_PARAMETER_KIND);
    }
    if name == FUNCTION_TYPE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(FUNCTION_TYPE_PARAMETER_KIND);
    }
    if name == SIGNATURE_TYPE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(SIGNATURE_TYPE_PARAMETER_KIND);
    }
    if name == TYPED_DESTRUCTURED_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(TYPED_DESTRUCTURED_PARAMETER_KIND);
    }
    if name == TYPE_PARAMETER_KIND && !kinds.formal_parameter_kinds.is_empty() {
        return Some(TYPE_PARAMETER_KIND);
    }
    kinds
        .declarations
        .iter()
        .map(|(_, kind)| kind.word())
        .find(|word| *word == name)
}

#[cfg(test)]
mod declaration_tests;

#[cfg(test)]
mod export_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolution asserts every kind and field id is non-zero, so resolving
    /// each pinned grammar's table is the proof the vocabulary exists.
    #[test]
    fn test_kind_tables_resolve_on_every_pinned_grammar() {
        let javascript =
            EcmaScriptKinds::resolve_javascript(&tree_sitter_javascript::LANGUAGE.into());
        assert_eq!(javascript.declarations.len(), 8);
        assert!(javascript.accessibility_modifier.is_none());

        for grammar in [
            tree_sitter_typescript::language_typescript(),
            tree_sitter_typescript::language_tsx(),
        ] {
            let typescript = EcmaScriptKinds::resolve_typescript(&grammar);
            assert_eq!(typescript.declarations.len(), 19);
            assert_eq!(typescript.member_signatures.len(), 2);
            assert!(typescript.accessibility_modifier.is_some());
        }
    }

    #[test]
    #[should_panic(
        expected = "must define syntax_element kind used by symbol extraction: \
                               kind=interface_declaration"
    )]
    fn test_typescript_table_refuses_a_grammar_without_the_typescript_kinds() {
        let _ = EcmaScriptKinds::resolve_typescript(&tree_sitter_javascript::LANGUAGE.into());
    }

    /// Every kind word a declaration carries on the wire, pinned.
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
            (EcmaScriptSymbolKind::Field, "property"),
            (EcmaScriptSymbolKind::EnumMember, "enum_member"),
        ];
        for (kind, word) in words {
            assert_eq!(kind.word(), word);
        }
    }

    /// Every declaration kind the rules interpret carries the `Declaration`
    /// kind, so the syntax element table stays exhaustive over the interpreted
    /// vocabulary.
    #[test]
    fn test_syntax_element_kinds_classify_every_interpreted_declaration_kind() {
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
            FIELD_DEFINITION_KIND,
            PUBLIC_FIELD_DEFINITION_KIND,
            ABSTRACT_CLASS_DECLARATION_KIND,
            ABSTRACT_METHOD_SIGNATURE_KIND,
            ENUM_ASSIGNMENT_KIND,
            LEXICAL_DECLARATION_KIND,
            VARIABLE_DECLARATION_KIND,
        ] {
            assert!(
                syntax_element_kinds(kind).contains(&SyntaxKind::Declaration),
                "kind {kind} must classify as a declaration"
            );
        }
    }

    #[test]
    fn test_syntax_element_kinds_classify_structure_boundaries_and_suffix_conventions() {
        assert_eq!(
            syntax_element_kinds("export_statement"),
            [SyntaxKind::Export, SyntaxKind::Statement]
        );
        assert_eq!(
            syntax_element_kinds("import_statement"),
            [SyntaxKind::Import, SyntaxKind::Statement]
        );
        assert_eq!(syntax_element_kinds("statement_block"), [SyntaxKind::Block]);
        assert_eq!(syntax_element_kinds("class_body"), [SyntaxKind::Body]);
        assert_eq!(syntax_element_kinds("interface_body"), [SyntaxKind::Body]);
        assert_eq!(syntax_element_kinds("enum_body"), [SyntaxKind::Body]);
        assert_eq!(
            syntax_element_kinds("required_parameter"),
            [SyntaxKind::Parameter]
        );
        assert_eq!(syntax_element_kinds("decorator"), [SyntaxKind::Annotation]);
        assert_eq!(syntax_element_kinds("comment"), [SyntaxKind::Comment]);
        assert_eq!(
            syntax_element_kinds("type_annotation"),
            [SyntaxKind::TypeExpression]
        );
        assert_eq!(
            syntax_element_kinds("jsx_element"),
            [SyntaxKind::Expression]
        );
        assert_eq!(
            syntax_element_kinds("return_statement"),
            [SyntaxKind::Statement]
        );
        assert_eq!(
            syntax_element_kinds("binary_expression"),
            [SyntaxKind::Expression]
        );
        assert_eq!(
            syntax_element_kinds("predefined_type"),
            [SyntaxKind::TypeExpression]
        );
        assert_eq!(syntax_element_kinds("identifier"), []);
    }
}
