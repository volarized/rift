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
//! - An assignment declares a `variable` symbol in its enclosing module,
//!   class, or function. Each name a tuple or list target binds declares, and so
//!   does each target of a chained assignment. A `type` statement declares a
//!   `type_alias` under the same rule.
//! - A `def` directly in a class body declares a `method` of the class. An
//!   attribute a method assigns through its first parameter (`self.name =
//!   name`) declares a `variable` of the class.
//! - A definition's docstring - the body's leading string expression - is
//!   carried as plain-text documentation with the quotes stripped. A
//!   module-level `name.__doc__ = "literal"` documents the module-level
//!   definition it names instead, without a documentation range, since the
//!   string sits outside the definition's span. A literal string statement directly after
//!   an assignment that declares one variable documents that variable.
//! - A module-level `from module import name as name`, the typing
//!   convention's explicit re-export, records an export binding; the
//!   namespace places it as an alias in the importing module.
//! - A module-level `__all__` written as string literals records an export
//!   binding for the assigned name, then one for each listed name.
//! - Python states no authored visibility, so `visibility` stays `None`;
//!   the leading-underscore convention is a reading habit, not syntax.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::OnceLock;

use rift_error::errors;
use rift_protocol::read::{Documentation, DocumentationFormat, Language, SymbolKind, SyntaxKind};
use tree_sitter::{Node, Parser};

use crate::document::SyntaxDocument;
use crate::extract::{self, Declaration, GrammarRules, Visited};
use crate::failure::RiftError;
use crate::provider::{SyntaxLimits, SyntaxProvider, SyntaxSource};

mod declared_exports;
mod overload;
mod variable_scopes;

#[cfg(test)]
mod declaration_tests;
#[cfg(test)]
mod name_tests;

/// Grammar spelling of a function definition, `async def` included.
const FUNCTION_KIND: &str = "function_definition";
/// Grammar spelling of a class definition.
const CLASS_KIND: &str = "class_definition";
/// Grammar spelling of the syntax element wrapping a decorated `def` or `class`.
const DECORATED_KIND: &str = "decorated_definition";
/// Grammar spelling of a suite of statements.
const BLOCK_KIND: &str = "block";
/// Grammar spelling of the module root.
const MODULE_KIND: &str = "module";
/// Provider-local name of the module itself, separate from Python identifiers.
pub(crate) const MODULE_QUALIFIED_NAME: &str = ".";
/// Grammar spelling of a comment before a module docstring.
const COMMENT_KIND: &str = "comment";
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
/// Grammar field holding an assignment's value.
const RIGHT_FIELD: &str = "right";
/// Grammar field holding an attribute's object, `self` in `self.name`.
const OBJECT_FIELD: &str = "object";
/// Grammar field holding an attribute's name, `name` in `self.name`.
const ATTRIBUTE_FIELD: &str = "attribute";
/// Grammar field holding a definition's parameter list.
const PARAMETERS_FIELD: &str = "parameters";
/// Grammar spelling of an attribute access, `self.name`.
const ATTRIBUTE_KIND: &str = "attribute";
/// Grammar spelling of a `type` statement.
const TYPE_ALIAS_STATEMENT_KIND: &str = "type_alias_statement";
/// Grammar spelling of a type expression, the name side of a `type`
/// statement.
const TYPE_KIND: &str = "type";
/// Grammar spelling of one decorator line.
const DECORATOR_KIND: &str = "decorator";
/// Grammar spelling of a call, `retry(2)` in `@retry(2)`.
const CALL_KIND: &str = "call";
/// Grammar spelling of a typed parameter, `self: Self`.
const TYPED_PARAMETER_KIND: &str = "typed_parameter";
/// Grammar spelling of the string's opening delimiter with its prefix.
const STRING_START_KIND: &str = "string_start";
/// Grammar spelling of a `from module import name` statement.
const IMPORT_FROM_KIND: &str = "import_from_statement";
/// Grammar spelling of one `name as alias` import.
const ALIASED_IMPORT_KIND: &str = "aliased_import";
/// Grammar field holding the module a `from` import reads.
const MODULE_NAME_FIELD: &str = "module_name";
/// Grammar field holding the name an aliased import binds.
const ALIAS_FIELD: &str = "alias";
/// Grammar spelling of an augmented assignment, `names += more`.
const AUGMENTED_ASSIGNMENT_KIND: &str = "augmented_assignment";
/// Grammar field holding an augmented assignment's operator.
const OPERATOR_FIELD: &str = "operator";
/// Grammar field holding a call's arguments.
const ARGUMENTS_FIELD: &str = "arguments";
/// Sequence expressions an `__all__` list may be written as.
const SEQUENCE_KINDS: [&str; 3] = ["list", "tuple", "expression_list"];
/// Loops whose `right` field is the iterated value.
const LOOP_KINDS: [&str; 2] = ["for_statement", "for_in_clause"];
/// Syntax elements that read a name they hold and cannot write it.
const READING_KINDS: [&str; 6] = [
    "binary_operator",
    "comparison_operator",
    "if_statement",
    "list_splat",
    "not_operator",
    "return_statement",
];
/// Grammar spelling of a call's arguments.
const ARGUMENT_LIST_KIND: &str = "argument_list";
/// Grammar field holding a call's callee.
const FUNCTION_FIELD: &str = "function";
/// Targets that bind every name they hold: `a, b`, `(a, b)`, `[a, *b]`.
const TARGET_PATTERN_KINDS: [&str; 4] = [
    "pattern_list",
    "tuple_pattern",
    "list_pattern",
    "list_splat_pattern",
];
/// Compound statements whose suites still declare at the enclosing module
/// or class level.
const DECLARING_STATEMENT_KINDS: [&str; 2] = ["if_statement", "try_statement"];
/// The clauses of a declaring compound statement holding a suite.
const DECLARING_CLAUSE_KINDS: [&str; 4] = [
    "elif_clause",
    "else_clause",
    "except_clause",
    "finally_clause",
];

/// The method name that constructs its class.
const CONSTRUCTOR_NAME: &str = "__init__";
/// The attribute a `__doc__` assignment documents through.
const DOC_ATTRIBUTE: &str = "__doc__";
/// Decorators that bind a method to its class rather than an instance.
const STATIC_DECORATORS: [&str; 2] = ["staticmethod", "classmethod"];
/// Decorators that make a method read like a field.
const PROPERTY_DECORATORS: [&str; 6] = [
    "property",
    "cached_property",
    "abstractproperty",
    "getter",
    "setter",
    "deleter",
];
/// String prefix letter that makes a literal a formatted string.
const FORMATTED_PREFIXES: [char; 2] = ['f', 'F'];

/// The kind word a Python function carries on the wire.
const FUNCTION_KIND_WORD: &str = "function";
/// The kind word a Python class carries on the wire.
const CLASS_KIND_WORD: &str = "class";
/// The kind word a Python variable carries on the wire.
const VARIABLE_KIND_WORD: &str = "variable";
/// The kind word a function declared in a class body carries on the wire.
const METHOD_KIND_WORD: &str = "method";
/// The kind word a `type` statement carries on the wire.
const TYPE_ALIAS_KIND_WORD: &str = "type_alias";

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
    comment: u16,
    expression_statement: u16,
    string: u16,
    string_content: u16,
    assignment: u16,
    identifier: u16,
    attribute: u16,
    type_alias_statement: u16,
    type_expression: u16,
    decorator: u16,
    call: u16,
    typed_parameter: u16,
    string_start: u16,
    import_from: u16,
    aliased_import: u16,
    target_patterns: Vec<u16>,
    declaring_statements: Vec<u16>,
    declaring_clauses: Vec<u16>,
    augmented_assignment: u16,
    sequence_kinds: Vec<u16>,
    loop_kinds: Vec<u16>,
    reading_kinds: Vec<u16>,
    argument_list: u16,
    function_field: NonZeroU16,
    operator: NonZeroU16,
    arguments: NonZeroU16,
    name: NonZeroU16,
    body: NonZeroU16,
    left: NonZeroU16,
    right: NonZeroU16,
    object: NonZeroU16,
    attribute_name: NonZeroU16,
    parameters: NonZeroU16,
    module_name: NonZeroU16,
    alias: NonZeroU16,
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
            comment: kind_id(language, COMMENT_KIND),
            expression_statement: kind_id(language, EXPRESSION_STATEMENT_KIND),
            string: kind_id(language, STRING_KIND),
            string_content: kind_id(language, STRING_CONTENT_KIND),
            assignment: kind_id(language, ASSIGNMENT_KIND),
            identifier: kind_id(language, IDENTIFIER_KIND),
            attribute: kind_id(language, ATTRIBUTE_KIND),
            type_alias_statement: kind_id(language, TYPE_ALIAS_STATEMENT_KIND),
            type_expression: kind_id(language, TYPE_KIND),
            decorator: kind_id(language, DECORATOR_KIND),
            call: kind_id(language, CALL_KIND),
            typed_parameter: kind_id(language, TYPED_PARAMETER_KIND),
            string_start: kind_id(language, STRING_START_KIND),
            import_from: kind_id(language, IMPORT_FROM_KIND),
            aliased_import: kind_id(language, ALIASED_IMPORT_KIND),
            target_patterns: TARGET_PATTERN_KINDS
                .map(|kind| kind_id(language, kind))
                .to_vec(),
            declaring_statements: DECLARING_STATEMENT_KINDS
                .map(|kind| kind_id(language, kind))
                .to_vec(),
            declaring_clauses: DECLARING_CLAUSE_KINDS
                .map(|kind| kind_id(language, kind))
                .to_vec(),
            augmented_assignment: kind_id(language, AUGMENTED_ASSIGNMENT_KIND),
            sequence_kinds: SEQUENCE_KINDS.map(|kind| kind_id(language, kind)).to_vec(),
            loop_kinds: LOOP_KINDS.map(|kind| kind_id(language, kind)).to_vec(),
            reading_kinds: READING_KINDS.map(|kind| kind_id(language, kind)).to_vec(),
            argument_list: kind_id(language, ARGUMENT_LIST_KIND),
            function_field: field_id(language, FUNCTION_FIELD),
            operator: field_id(language, OPERATOR_FIELD),
            arguments: field_id(language, ARGUMENTS_FIELD),
            name: field_id(language, NAME_FIELD),
            body: field_id(language, BODY_FIELD),
            left: field_id(language, LEFT_FIELD),
            right: field_id(language, RIGHT_FIELD),
            object: field_id(language, OBJECT_FIELD),
            attribute_name: field_id(language, ATTRIBUTE_FIELD),
            parameters: field_id(language, PARAMETERS_FIELD),
            module_name: field_id(language, MODULE_NAME_FIELD),
            alias: field_id(language, ALIAS_FIELD),
        }
    }
}

/// Resolves one syntax element kind id, proving the pinned grammar defines it.
fn kind_id(language: &tree_sitter::Language, kind: &str) -> u16 {
    let id = language.id_for_node_kind(kind, true);
    assert!(
        id != 0,
        "pinned Python grammar must define syntax_element kind used by symbol \
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

/// Where an assignment or a `type` statement declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclarationScope {
    /// Module level, the suites of its `if` and `try` statements included.
    Module,
    /// Directly in a class body, the suites of its `if` and `try`
    /// statements included.
    Class,
    /// Inside a function body.
    Local,
}

impl DeclarationScope {
    /// Portable kinds a variable declared in this scope carries.
    fn variable_kinds(self) -> Vec<SymbolKind> {
        match self {
            Self::Module | Self::Local => vec![SymbolKind::Value],
            Self::Class => vec![SymbolKind::Value, SymbolKind::Member],
        }
    }
}

/// One literal string's content pieces and their ranges.
type StringText = (String, Vec<crate::ByteRange>);

/// The pinned grammar's decisions for the shared bounded walk.
#[derive(Debug)]
struct PythonRules {
    kinds: &'static PythonKinds,
    overloads: overload::OverloadContext,
    /// Documentation module-level `name.__doc__ = "literal"` assignments
    /// give, by the name they document; the last assignment wins, as it
    /// does when the module runs.
    assigned_docs: BTreeMap<String, StringText>,
}

impl PythonRules {
    /// The spelled name behind a definition's `name` field; `None` when the
    /// tree is missing it.
    fn field_name(&self, syntax_element: Node<'_>, text: &str) -> Option<String> {
        let name = syntax_element.child_by_field_id(self.kinds.name.get())?;
        text.get(name.byte_range()).map(str::to_owned)
    }

    /// A string literal's content without its quotes, and each piece's
    /// range; empty for a string without content.
    fn string_text(&self, string: Node<'_>, text: &str) -> Result<StringText, RiftError> {
        let mut content = String::new();
        let mut ranges = Vec::new();
        let mut cursor = string.walk();
        for child in string.named_children(&mut cursor) {
            if child.kind_id() == self.kinds.string_content
                && let Some(piece) = text.get(child.byte_range())
            {
                content.push_str(piece);
                ranges.push(extract::byte_range(child)?);
            }
        }
        Ok((content, ranges))
    }

    /// The definition's docstring: its body suite's leading string
    /// expression, quotes stripped. `None` when the body opens with
    /// anything else.
    fn docstring(&self, syntax_element: Node<'_>, text: &str) -> Result<StringText, RiftError> {
        let Some(body) = syntax_element.child_by_field_id(self.kinds.body.get()) else {
            return Ok((String::new(), Vec::new()));
        };
        let Some(first) = body.named_child(0) else {
            return Ok((String::new(), Vec::new()));
        };
        if first.kind_id() != self.kinds.expression_statement {
            return Ok((String::new(), Vec::new()));
        }
        let Some(string) = first
            .named_child(0)
            .filter(|expression| expression.kind_id() == self.kinds.string)
        else {
            return Ok((String::new(), Vec::new()));
        };
        self.string_text(string, text)
    }

    /// One definition's declaration facts, shared by `def` and `class`.
    fn definition_declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
        kind: &'static str,
        kinds: Vec<SymbolKind>,
    ) -> Result<Option<Declaration>, RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(name) = self.field_name(syntax_element, text) else {
            return Ok(None);
        };
        let body_range = match syntax_element.child_by_field_id(self.kinds.body.get()) {
            Some(body) => Some(extract::byte_range(body)?),
            None => None,
        };
        let (content, documentation_ranges) = self.docstring(syntax_element, text)?;
        let (documentation, documentation_ranges) = if content.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            (
                vec![Documentation {
                    format: DocumentationFormat::Plain,
                    text: content,
                }],
                documentation_ranges,
            )
        };
        Ok(Some(Declaration {
            name,
            kind,
            kinds,
            visibility: None,
            body_range,
            documentation,
            documentation_ranges,
        }))
    }

    /// The file's module declaration and leading literal docstring.
    fn module_declaration(
        &self,
        syntax_element: Node<'_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let mut cursor = syntax_element.walk();
        let string = syntax_element
            .named_children(&mut cursor)
            .find(|child| child.kind_id() != self.kinds.comment)
            .and_then(|statement| literal_string(self.kinds, statement, text));
        let (content, documentation_ranges) = match string {
            Some(string) => self.string_text(string, text)?,
            None => (String::new(), Vec::new()),
        };
        let documentation = if content.is_empty() {
            Vec::new()
        } else {
            vec![Documentation {
                format: DocumentationFormat::Plain,
                text: content,
            }]
        };
        Ok(Some(Declaration {
            name: MODULE_KIND.to_owned(),
            kind: MODULE_KIND,
            kinds: vec![SymbolKind::Namespace, SymbolKind::Module],
            visibility: None,
            body_range: None,
            documentation,
            documentation_ranges,
        }))
    }

    /// Whether a `def` sits directly in a class body, decorated or not.
    fn is_method(&self, function: Visited<'_, '_>) -> bool {
        let block = function
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.decorated)
            .map_or(function.parent(), Visited::parent);
        block.is_some_and(|block| {
            block.syntax_element().kind_id() == self.kinds.block
                && block.parent().is_some_and(|definition| {
                    definition.syntax_element().kind_id() == self.kinds.class
                })
        })
    }

    /// The last name segment of each decorator on a decorated definition:
    /// `cached_property` in `@functools.cached_property`, `retry` in
    /// `@retry(2)`.
    fn decorator_names<'text>(
        &self,
        visited: Visited<'_, '_>,
        text: &'text str,
    ) -> Vec<&'text str> {
        let Some(decorated) = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.decorated)
        else {
            return Vec::new();
        };
        let mut cursor = decorated.syntax_element().walk();
        let decorators: Vec<_> = decorated
            .syntax_element()
            .named_children(&mut cursor)
            .filter(|child| child.kind_id() == self.kinds.decorator)
            .collect();
        decorators
            .into_iter()
            .filter_map(|decorator| {
                let mut expression = decorator.named_child(0)?;
                if expression.kind_id() == self.kinds.call {
                    expression = expression.named_child(0)?;
                }
                if expression.kind_id() == self.kinds.attribute {
                    expression = expression.child_by_field_id(self.kinds.attribute_name.get())?;
                }
                (expression.kind_id() == self.kinds.identifier)
                    .then(|| text.get(expression.byte_range()))
                    .flatten()
            })
            .collect()
    }

    /// The categories a method carries beside `value`, `callable`, and
    /// `member`: `constructor` for `__init__`, `static` for a static or class
    /// method, `property` for a property accessor.
    fn method_kinds(&self, visited: Visited<'_, '_>, text: &str) -> Vec<SymbolKind> {
        let mut kinds = vec![SymbolKind::Value, SymbolKind::Callable, SymbolKind::Member];
        if self.field_name(visited.syntax_element(), text).as_deref() == Some(CONSTRUCTOR_NAME) {
            kinds.push(SymbolKind::Constructor);
        }
        let decorators = self.decorator_names(visited, text);
        if decorators
            .iter()
            .any(|name| STATIC_DECORATORS.contains(name))
        {
            kinds.push(SymbolKind::Static);
        }
        if decorators
            .iter()
            .any(|name| PROPERTY_DECORATORS.contains(name))
        {
            kinds.push(SymbolKind::Property);
        }
        kinds
    }

    /// The enclosing module, class, or function of a statement.
    ///
    /// Each step climbs one recorded parent, so the work is bounded by the
    /// tree's depth.
    fn statement_scope(&self, statement: Visited<'_, '_>) -> Option<DeclarationScope> {
        let mut holder = statement.parent()?;
        loop {
            let id = holder.syntax_element().kind_id();
            if id == self.kinds.module {
                return Some(DeclarationScope::Module);
            }
            if id != self.kinds.block {
                return None;
            }
            let owner = holder.parent()?;
            let owner_id = owner.syntax_element().kind_id();
            if owner_id == self.kinds.class {
                return Some(DeclarationScope::Class);
            }
            if owner_id == self.kinds.function {
                return Some(DeclarationScope::Local);
            }
            let compound = if self.kinds.declaring_clauses.contains(&owner_id) {
                owner.parent()?
            } else {
                owner
            };
            if !self
                .kinds
                .declaring_statements
                .contains(&compound.syntax_element().kind_id())
            {
                holder = owner.parent()?;
                continue;
            }
            holder = compound.parent()?;
        }
    }

    /// The statement holding an assignment: its `expression_statement`,
    /// reached through every assignment of a chain such as `a = b = 1`.
    fn assignment_statement<'walk, 'tree>(
        &self,
        assignment: Visited<'walk, 'tree>,
    ) -> Option<Visited<'walk, 'tree>> {
        let mut current = assignment;
        loop {
            let parent = current.parent()?;
            let id = parent.syntax_element().kind_id();
            if id == self.kinds.expression_statement {
                return Some(parent);
            }
            let is_value = id == self.kinds.assignment
                && parent
                    .syntax_element()
                    .child_by_field_id(self.kinds.right.get())
                    == Some(current.syntax_element());
            if !is_value {
                return None;
            }
            current = parent;
        }
    }

    /// The assignment a target syntax element binds through: the target is the
    /// assignment's `left` side, or sits inside the tuple and list patterns
    /// that form it. With `through_pattern`, only a target inside a pattern
    /// counts.
    fn target_assignment<'walk, 'tree>(
        &self,
        target: Visited<'walk, 'tree>,
        through_pattern: bool,
    ) -> Option<Visited<'walk, 'tree>> {
        let mut child = target;
        let mut patterned = false;
        loop {
            let parent = child.parent()?;
            let id = parent.syntax_element().kind_id();
            if id == self.kinds.assignment {
                let binds = parent
                    .syntax_element()
                    .child_by_field_id(self.kinds.left.get())
                    == Some(child.syntax_element())
                    && (patterned || !through_pattern);
                return binds.then_some(parent);
            }
            if !self.kinds.target_patterns.contains(&id) {
                return None;
            }
            patterned = true;
            child = parent;
        }
    }

    /// A variable declared by a statement in `scope`.
    fn variable(name: &str, scope: DeclarationScope) -> Declaration {
        Declaration {
            name: name.to_owned(),
            kind: VARIABLE_KIND_WORD,
            kinds: scope.variable_kinds(),
            visibility: None,
            body_range: None,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        }
    }

    /// The declared variable behind an assignment whose target is one plain
    /// identifier.
    fn assignment_declaration(&self, visited: Visited<'_, '_>, text: &str) -> Option<Declaration> {
        let target = visited
            .syntax_element()
            .child_by_field_id(self.kinds.left.get())?;
        if target.kind_id() != self.kinds.identifier {
            return None;
        }
        let scope = self.statement_scope(self.assignment_statement(visited)?)?;
        Some(Self::variable(text.get(target.byte_range())?, scope))
    }

    /// The declared variable behind one name a tuple or list target binds.
    fn pattern_declaration(&self, visited: Visited<'_, '_>, text: &str) -> Option<Declaration> {
        let assignment = self.target_assignment(visited, true)?;
        let scope = self.statement_scope(self.assignment_statement(assignment)?)?;
        Some(Self::variable(
            text.get(visited.syntax_element().byte_range())?,
            scope,
        ))
    }

    /// The method an instance attribute target belongs to: the visited
    /// `attribute` is an assignment target, its object is the first
    /// parameter of the nearest enclosing `def`, and that `def` is a method.
    fn attribute_method<'walk, 'tree>(
        &self,
        visited: Visited<'walk, 'tree>,
        text: &str,
    ) -> Option<Visited<'walk, 'tree>> {
        let syntax_element = visited.syntax_element();
        if syntax_element.kind_id() != self.kinds.attribute {
            return None;
        }
        let object = syntax_element.child_by_field_id(self.kinds.object.get())?;
        let name = syntax_element.child_by_field_id(self.kinds.attribute_name.get())?;
        if object.kind_id() != self.kinds.identifier || name.kind_id() != self.kinds.identifier {
            return None;
        }
        let assignment = self.target_assignment(visited, false)?;
        let mut current = self.assignment_statement(assignment)?;
        let function = loop {
            let parent = current.parent()?;
            let id = parent.syntax_element().kind_id();
            if id == self.kinds.function {
                break parent;
            }
            if id == self.kinds.class || id == self.kinds.module {
                return None;
            }
            current = parent;
        };
        if !self.is_method(function) {
            return None;
        }
        let receiver = self.first_parameter(function.syntax_element(), text)?;
        (text.get(object.byte_range()) == Some(receiver)).then_some(function)
    }

    /// A `def`'s first parameter name, `self` in `def open(self, path)`.
    fn first_parameter<'text>(&self, function: Node<'_>, text: &'text str) -> Option<&'text str> {
        let parameters = function.child_by_field_id(self.kinds.parameters.get())?;
        let mut first = parameters.named_child(0)?;
        if first.kind_id() == self.kinds.typed_parameter {
            first = first.named_child(0)?;
        }
        (first.kind_id() == self.kinds.identifier)
            .then(|| text.get(first.byte_range()))
            .flatten()
    }

    /// The `type_alias` a `type` statement declares in an accepted scope.
    fn type_alias_declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let Some(name) = self.type_alias_name(visited.syntax_element()) else {
            return Ok(None);
        };
        if self.statement_scope(visited).is_none() {
            return Ok(None);
        }
        let Some(name) = text.get(name.byte_range()) else {
            return Ok(None);
        };
        let body_range = visited
            .syntax_element()
            .child_by_field_id(self.kinds.right.get())
            .map(extract::byte_range)
            .transpose()?;
        Ok(Some(Declaration {
            name: name.to_owned(),
            kind: TYPE_ALIAS_KIND_WORD,
            kinds: vec![SymbolKind::Type, SymbolKind::Alias],
            visibility: None,
            body_range,
            documentation: Vec::new(),
            documentation_ranges: Vec::new(),
        }))
    }

    /// The identifier a `type` statement names, `Pair` in
    /// `type Pair = tuple[int, int]`; `None` for a generic left side.
    fn type_alias_name<'tree>(&self, statement: Node<'tree>) -> Option<Node<'tree>> {
        if statement.kind_id() != self.kinds.type_alias_statement {
            return None;
        }
        let left = statement.child_by_field_id(self.kinds.left.get())?;
        let name = if left.kind_id() == self.kinds.type_expression {
            left.named_child(0)?
        } else {
            left
        };
        (name.kind_id() == self.kinds.identifier).then_some(name)
    }
}

/// Literal documentation each module-level `name.__doc__ = "..."` assigns,
/// by name. A formatted string documents nothing, since its text depends on
/// running the module. Reads the module's top-level statements only.
fn assigned_docs(
    rules: &PythonRules,
    root: Node<'_>,
    text: &str,
) -> Result<BTreeMap<String, StringText>, RiftError> {
    let kinds = rules.kinds;
    let mut docs = BTreeMap::new();
    let mut cursor = root.walk();
    let statements: Vec<_> = root.named_children(&mut cursor).collect();
    for statement in statements {
        if statement.kind_id() != kinds.expression_statement {
            continue;
        }
        let Some(assignment) = statement
            .named_child(0)
            .filter(|syntax_element| syntax_element.kind_id() == kinds.assignment)
        else {
            continue;
        };
        let (Some(left), Some(right)) = (
            assignment.child_by_field_id(kinds.left.get()),
            assignment.child_by_field_id(kinds.right.get()),
        ) else {
            continue;
        };
        let (Some(object), Some(attribute)) = (
            left.child_by_field_id(kinds.object.get()),
            left.child_by_field_id(kinds.attribute_name.get()),
        ) else {
            continue;
        };
        let documents = left.kind_id() == kinds.attribute
            && object.kind_id() == kinds.identifier
            && text.get(attribute.byte_range()) == Some(DOC_ATTRIBUTE)
            && right.kind_id() == kinds.string;
        if !documents {
            continue;
        }
        let formatted = right
            .named_child(0)
            .filter(|start| start.kind_id() == kinds.string_start)
            .and_then(|start| text.get(start.byte_range()))
            .is_none_or(|start| start.contains(FORMATTED_PREFIXES));
        if formatted {
            continue;
        }
        let Some(name) = text.get(object.byte_range()) else {
            continue;
        };
        docs.insert(name.to_owned(), rules.string_text(right, text)?);
    }
    Ok(docs)
}

/// The literal string a statement holds alone, as in a docstring; `None`
/// for any other statement and for a formatted string, whose text depends on
/// running the module.
fn literal_string<'tree>(
    kinds: &PythonKinds,
    statement: Node<'tree>,
    text: &str,
) -> Option<Node<'tree>> {
    if statement.kind_id() != kinds.expression_statement || statement.named_child_count() != 1 {
        return None;
    }
    let string = statement
        .named_child(0)
        .filter(|expression| expression.kind_id() == kinds.string)?;
    let formatted = string
        .named_child(0)
        .filter(|start| start.kind_id() == kinds.string_start)
        .and_then(|start| text.get(start.byte_range()))
        .is_none_or(|start| start.contains(['f', 'F', 'b', 'B']));
    (!formatted).then_some(string)
}

/// Documents each variable that one assignment declares alone with the
/// literal string statement directly after it, the attribute docstring
/// convention: `LIMIT = 10` followed by `"""Most rows per page."""`. The
/// declaration's span extends over that string, as it does over attached doc
/// comments in other languages. A declaration that already carries
/// documentation keeps it.
///
/// Runs after the bounded walk accepted the tree, and reads each statement
/// list once, so the work is linear in the tree's size.
fn attach_attribute_docs(
    rules: &PythonRules,
    root: Node<'_>,
    text: &str,
    symbols: &mut [crate::SyntaxSymbol],
) -> Result<(), RiftError> {
    let documented = rules.attribute_docs(root, text)?;
    for symbol in symbols {
        if symbol.kind != VARIABLE_KIND_WORD || !symbol.documentation.is_empty() {
            continue;
        }
        if let Some(((content, ranges), end)) = documented.get(&symbol.item_range) {
            symbol.documentation = vec![Documentation {
                format: DocumentationFormat::Plain,
                text: content.clone(),
            }]
            .into();
            symbol.documentation_ranges.clone_from(ranges);
            symbol.range.end = symbol.range.end.max(*end);
        }
    }
    Ok(())
}

type AttributeDocs = BTreeMap<crate::ByteRange, (StringText, u64)>;

impl PythonRules {
    fn attach_assigned_docs(&self, symbols: &mut [crate::SyntaxSymbol]) {
        for symbol in symbols {
            if symbol.container.is_some() || symbol.kind == MODULE_KIND {
                continue;
            }
            let Some((content, _)) = self.assigned_docs.get(&symbol.name) else {
                continue;
            };
            symbol.documentation = if content.is_empty() {
                Vec::new()
            } else {
                vec![Documentation {
                    format: DocumentationFormat::Plain,
                    text: content.clone(),
                }]
            }
            .into();
            // Assigned documentation sits outside the declaration's source range.
            symbol.documentation_ranges.clear();
        }
    }

    fn single_assignment(&self, assignment: Node<'_>) -> bool {
        let kinds = self.kinds;
        if assignment.kind_id() != kinds.assignment {
            return false;
        }
        let single = assignment
            .child_by_field_id(kinds.left.get())
            .is_some_and(|target| [kinds.identifier, kinds.attribute].contains(&target.kind_id()));
        let chained = assignment
            .child_by_field_id(kinds.right.get())
            .is_some_and(|value| value.kind_id() == kinds.assignment);
        single && !chained
    }

    fn attribute_block_docs(
        &self,
        block: Node<'_>,
        text: &str,
        documented: &mut AttributeDocs,
    ) -> Result<(), RiftError> {
        let mut cursor = block.walk();
        let mut previous: Option<Node<'_>> = None;
        for statement in block.named_children(&mut cursor) {
            let last = previous.replace(statement);
            let Some(assignment) = last
                .filter(|last| last.kind_id() == self.kinds.expression_statement)
                .and_then(|last| last.named_child(0))
                .filter(|assignment| self.single_assignment(*assignment))
            else {
                continue;
            };
            let Some(string) = literal_string(self.kinds, statement, text) else {
                continue;
            };
            let (content, ranges) = self.string_text(string, text)?;
            if content.is_empty() {
                continue;
            }
            let end = extract::byte_range(statement)?.end;
            documented.insert(extract::byte_range(assignment)?, ((content, ranges), end));
        }
        Ok(())
    }

    /// Reads every accepted syntax element and statement list once.
    fn attribute_docs(&self, root: Node<'_>, text: &str) -> Result<AttributeDocs, RiftError> {
        let mut documented = BTreeMap::new();
        let mut cursor = root.walk();
        'walk: loop {
            let syntax_element = cursor.node();
            if [self.kinds.module, self.kinds.block].contains(&syntax_element.kind_id()) {
                self.attribute_block_docs(syntax_element, text, &mut documented)?;
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    continue 'walk;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
            }
        }
        Ok(documented)
    }
}

impl GrammarRules for PythonRules {
    /// A module-level `from module import name as name`, which re-exports
    /// `name` by the typing convention, binds `local` to the imported name,
    /// `exported` to the alias, and `source` to the module as written.
    fn export_binding(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
        _qualification: &str,
    ) -> Result<Option<crate::SyntaxExportBinding>, RiftError> {
        let syntax_element = visited.syntax_element();
        if syntax_element.kind_id() != self.kinds.aliased_import {
            return Ok(None);
        }
        let Some(statement) = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.kinds.import_from)
        else {
            return Ok(None);
        };
        if self.statement_scope(statement) != Some(DeclarationScope::Module) {
            return Ok(None);
        }
        let (Some(name), Some(alias), Some(module)) = (
            syntax_element.child_by_field_id(self.kinds.name.get()),
            syntax_element.child_by_field_id(self.kinds.alias.get()),
            statement
                .syntax_element()
                .child_by_field_id(self.kinds.module_name.get()),
        ) else {
            return Ok(None);
        };
        let spelled = |syntax_element: Node<'_>| text.get(syntax_element.byte_range());
        if name.named_child_count() != 1
            || spelled(name).is_none()
            || spelled(name) != spelled(alias)
        {
            return Ok(None);
        }
        Ok(Some(crate::SyntaxExportBinding {
            kind: crate::SyntaxExportKind::Named,
            range: extract::byte_range(statement.syntax_element())?,
            local: Some(extract::byte_range(name)?),
            exported: Some(extract::byte_range(alias)?),
            source: Some(extract::byte_range(module)?),
            container: None,
            type_only: false,
        }))
    }

    fn python_overload(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<crate::PythonOverload>, RiftError> {
        if visited.syntax_element().kind_id() != self.kinds.function {
            return Ok(None);
        }
        self.overloads
            .state(visited.syntax_element(), text)
            .map(Some)
    }

    fn name_range(&self, syntax_element: Node<'_>) -> Result<Option<crate::ByteRange>, RiftError> {
        let name = if syntax_element.kind_id() == self.kinds.identifier {
            Some(syntax_element)
        } else if syntax_element.kind_id() == self.kinds.attribute {
            syntax_element.child_by_field_id(self.kinds.attribute_name.get())
        } else if syntax_element.kind_id() == self.kinds.type_alias_statement {
            self.type_alias_name(syntax_element)
        } else if syntax_element.kind_id() == self.kinds.assignment {
            syntax_element
                .child_by_field_id(self.kinds.left.get())
                .filter(|target| target.kind_id() == self.kinds.identifier)
        } else {
            syntax_element.child_by_field_id(self.kinds.name.get())
        };
        name.map(extract::byte_range).transpose()
    }

    fn declaration(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
    ) -> Result<Option<Declaration>, RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        if id == self.kinds.module {
            return self.module_declaration(syntax_element, text);
        }
        if id == self.kinds.function {
            if self.is_method(visited) {
                let kinds = self.method_kinds(visited, text);
                return self.definition_declaration(visited, text, METHOD_KIND_WORD, kinds);
            }
            return self.definition_declaration(
                visited,
                text,
                FUNCTION_KIND_WORD,
                vec![SymbolKind::Value, SymbolKind::Callable],
            );
        }
        if id == self.kinds.class {
            return self.definition_declaration(
                visited,
                text,
                CLASS_KIND_WORD,
                vec![
                    SymbolKind::Type,
                    SymbolKind::MemberContainer,
                    SymbolKind::Constructible,
                    SymbolKind::Extensible,
                ],
            );
        }
        if id == self.kinds.assignment {
            return Ok(self.assignment_declaration(visited, text));
        }
        if id == self.kinds.identifier {
            return Ok(self.pattern_declaration(visited, text));
        }
        if id == self.kinds.attribute {
            let Some(_) = self.attribute_method(visited, text) else {
                return Ok(None);
            };
            let name = syntax_element
                .child_by_field_id(self.kinds.attribute_name.get())
                .and_then(|name| text.get(name.byte_range()));
            return Ok(name.map(|name| Self::variable(name, DeclarationScope::Class)));
        }
        if id == self.kinds.type_alias_statement {
            return self.type_alias_declaration(visited, text);
        }
        Ok(None)
    }

    /// An instance attribute belongs to the class: it drops the scope of the
    /// method assigning it.
    fn declaration_qualification(
        &self,
        visited: Visited<'_, '_>,
        text: &str,
        qualification: &str,
    ) -> Option<String> {
        let method = self.attribute_method(visited, text)?;
        let name = self.field_name(method.syntax_element(), text)?;
        let class = qualification
            .strip_suffix(name.as_str())?
            .strip_suffix(SCOPE_QUALIFICATION_SEPARATOR)?;
        Some(class.to_owned())
    }

    /// An instance attribute assigned alone spans its whole assignment, as
    /// a one-name assignment does; one inside a tuple target spans itself.
    fn declaration_syntax_element<'tree>(&self, visited: Visited<'_, 'tree>) -> Node<'tree> {
        let syntax_element = visited.syntax_element();
        if syntax_element.kind_id() != self.kinds.attribute {
            return syntax_element;
        }
        visited
            .parent()
            .map(Visited::syntax_element)
            .filter(|parent| {
                parent.kind_id() == self.kinds.assignment
                    && parent.child_by_field_id(self.kinds.left.get()) == Some(syntax_element)
            })
            .unwrap_or(syntax_element)
    }

    fn container_name(&self, syntax_element: Node<'_>, text: &str) -> Option<String> {
        if syntax_element.kind_id() != self.kinds.function
            && syntax_element.kind_id() != self.kinds.class
        {
            return None;
        }
        self.field_name(syntax_element, text)
    }

    /// A decorated definition starts at its first decorator, so the whole
    /// declaration removes with its decorators.
    fn declaration_start(&self, visited: Visited<'_, '_>, _text: &str) -> usize {
        match visited.parent().map(Visited::syntax_element) {
            Some(parent) if parent.kind_id() == self.kinds.decorated => parent.start_byte(),
            _ => visited.syntax_element().start_byte(),
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
}

impl Default for PythonSyntaxProvider {
    fn default() -> Self {
        Self {
            language: Language {
                name: "python".to_owned(),
                dialect: None,
            },
        }
    }
}

/// Returns the process-wide resolved kind table, computing it once.
fn python_kinds() -> &'static PythonKinds {
    static KINDS: OnceLock<PythonKinds> = OnceLock::new();
    KINDS.get_or_init(|| PythonKinds::resolve(&tree_sitter_python::LANGUAGE.into()))
}

/// A parser speaking the pinned Python grammar.
fn python_parser() -> Result<Parser, RiftError> {
    let grammar = tree_sitter_python::LANGUAGE.into();
    let mut parser = Parser::new();
    parser.set_language(&grammar).map_err(|_| {
        errors::syntax::incompatible_grammar()
            .grammar_abi_version(grammar.abi_version())
            .runtime_abi_min(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION)
            .runtime_abi_max(tree_sitter::LANGUAGE_VERSION)
            .error()
    })?;
    Ok(parser)
}

impl SyntaxProvider for PythonSyntaxProvider {
    fn language(&self) -> &Language {
        &self.language
    }

    fn analyze(
        &self,
        source: SyntaxSource<'_>,
        limits: SyntaxLimits,
    ) -> Result<SyntaxDocument, RiftError> {
        limits.admit_source(source)?;
        let mut parser = python_parser()?;
        let tree = parser
            .parse(source.text, None)
            .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
        let mut rules = PythonRules {
            kinds: python_kinds(),
            overloads: overload::OverloadContext::new(tree.root_node(), source.text, limits)?,
            assigned_docs: BTreeMap::new(),
        };
        rules.assigned_docs = assigned_docs(&rules, tree.root_node(), source.text)?;
        let mut capture = crate::static_analysis::Capture::for_edge_python(source.text);
        let (syntax_elements, mut symbols, mut export_bindings) = extract::extract_with_capture(
            tree.root_node(),
            source,
            limits,
            &self.language,
            &rules,
            Some(&mut capture),
        )?;
        if let Some(module) = symbols
            .first_mut()
            .filter(|symbol| symbol.kind == MODULE_KIND)
        {
            MODULE_QUALIFIED_NAME.clone_into(&mut module.qualified_name);
            // The root of a file without tokens is an empty range after its spaces.
            if module.item_range.start == module.item_range.end {
                let file = crate::ByteRange {
                    start: 0,
                    end: u64::try_from(source.text.len()).map_err(|overflow| {
                        crate::failure::position_overflow(tree.root_node(), overflow)
                    })?,
                };
                module.range = file;
                module.item_range = file;
            }
        }
        rules.attach_assigned_docs(&mut symbols);
        attach_attribute_docs(&rules, tree.root_node(), source.text, &mut symbols)?;
        export_bindings.extend(declared_exports::declared_exports(
            python_kinds(),
            tree.root_node(),
            source.text,
        )?);
        export_bindings.sort_by_key(|binding| binding.range.start);
        let static_facts = capture.finish_python(tree.root_node())?;
        variable_scopes::apply(&mut symbols, &static_facts, source.text, limits)?;
        Ok(SyntaxDocument::new(
            self.language.clone(),
            source.path.clone(),
            syntax_elements,
            symbols,
            tree.root_node().has_error(),
        )
        .with_export_bindings(export_bindings)
        .with_source_witness(source.text)
        .with_static_facts(Some(static_facts))
        .with_syntax_limits(limits))
    }

    fn syntax_element_kinds(&self, kind: &str) -> Vec<SyntaxKind> {
        let mut kinds = Vec::new();
        if kind == MODULE_KIND || kind.ends_with("_definition") {
            kinds.extend([SyntaxKind::Declaration, SyntaxKind::Definition]);
        }
        if kind == BLOCK_KIND {
            kinds.push(SyntaxKind::Block);
        }
        if kind.ends_with("_statement") {
            kinds.push(SyntaxKind::Statement);
        }
        if kind.contains("comment") {
            kinds.push(SyntaxKind::Comment);
        }
        if kind == DECORATOR_KIND {
            kinds.push(SyntaxKind::Annotation);
        }
        kinds
    }
}

pub(crate) fn restored_symbol_kind(name: &str) -> Option<&'static str> {
    [
        FUNCTION_KIND_WORD,
        CLASS_KIND_WORD,
        VARIABLE_KIND_WORD,
        METHOD_KIND_WORD,
        TYPE_ALIAS_KIND_WORD,
        MODULE_KIND,
    ]
    .into_iter()
    .find(|word| *word == name)
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;

    use super::*;
    use crate::ByteRange;

    fn text_at(text: &str, range: ByteRange) -> &str {
        let start = usize::try_from(range.start).expect("range starts within source");
        let end = usize::try_from(range.end).expect("range ends within source");
        &text[start..end]
    }

    fn analyze(text: &str) -> SyntaxDocument {
        let path = ProjectPath::new("src/service.py").expect("valid fixture path");
        PythonSyntaxProvider::default()
            .analyze(SyntaxSource { path: &path, text }, SyntaxLimits::default())
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
        assert_eq!(function.kinds, [SymbolKind::Value, SymbolKind::Callable]);
        assert_eq!(function.visibility, None);
        let class = symbol(&document, "Widget");
        assert_eq!(class.kind, "class");
        assert_eq!(
            class.kinds,
            [
                SymbolKind::Type,
                SymbolKind::MemberContainer,
                SymbolKind::Constructible,
                SymbolKind::Extensible,
            ]
        );
    }

    #[test]
    fn overload_alias_and_qualified_imports_keep_original_ranges() {
        for text in [
            "from typing import overload as ov\n@ov\ndef call(value: int): ...\ndef call(value): return value\n",
            "import typing as types\n@types.overload\ndef call(value: int): ...\ndef call(value): return value\n",
            "from typing_extensions import overload\nclass Target:\n    @overload\n    def call(self, value: int): ...\n",
        ] {
            let document = analyze(text);
            let function = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == "call")
                .expect("overload declaration");
            let Some(crate::PythonOverload::Overload {
                import_statement,
                module,
                imported,
                binding,
                decorator,
            }) = function.python_overload.as_ref()
            else {
                panic!("captured overload import and decorator");
            };
            assert!(import_statement.end <= decorator.start);
            let slice = |range: crate::ByteRange| {
                &text[usize::try_from(range.start).expect("start")
                    ..usize::try_from(range.end).expect("end")]
            };
            assert!(matches!(slice(*module), "typing" | "typing_extensions"));
            if let Some(imported) = imported {
                assert_eq!(slice(*imported), "overload");
            }
            assert!(!slice(*binding).is_empty());
            assert!(
                slice(*decorator).ends_with(slice(*binding))
                    || slice(*decorator).ends_with(".overload")
            );
        }
    }

    #[test]
    fn unrelated_shadowed_conditional_and_nested_overload_decorators_stay_unknown() {
        for text in [
            "from unrelated import overload\n@overload\ndef call(): ...\n",
            "from typing import overload\noverload = other\n@overload\ndef call(): ...\n",
            "if enabled:\n    from typing import overload\n@overload\ndef call(): ...\n",
            "from typing import overload\ndef outer():\n    @overload\n    def call(): ...\n",
            "from typing import overload\n@unrelated\n@overload\ndef call(): ...\n",
        ] {
            let document = analyze(text);
            let function = document
                .symbols()
                .iter()
                .find(|symbol| symbol.name == "call")
                .expect("retained function");
            assert_eq!(
                function.python_overload,
                Some(crate::PythonOverload::Unknown)
            );
        }
        let document = analyze("def call(): return 1\n");
        assert_eq!(
            symbol(&document, "call").python_overload,
            Some(crate::PythonOverload::Ordinary)
        );
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
        assert_eq!(symbol(&document, "Widget.open").kind, "method");
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
        let text = "def serve():\n    \"\"\"Answers one request.\"\"\"\n    pass\n\
             \n\nclass Widget:\n    'One drawn control.'\n";
        let document = analyze(text);
        let function = symbol(&document, "serve");
        assert_eq!(
            function.documentation.as_ref(),
            [Documentation {
                format: DocumentationFormat::Plain,
                text: "Answers one request.".to_owned(),
            }]
        );
        let class = symbol(&document, "Widget");
        assert_eq!(class.documentation[0].text, "One drawn control.");
        assert_eq!(
            text_at(text, function.documentation_ranges[0]),
            "Answers one request."
        );
    }

    #[test]
    fn test_a_body_without_a_docstring_carries_no_documentation() {
        for text in [
            "def serve():\n    return 1\n",
            "def serve():\n    ''\n",
            "def serve():\n",
        ] {
            let document = analyze(text);
            assert!(symbol(&document, "serve").documentation.is_empty());
            assert_eq!(symbol(&document, "serve").documentation_ranges, []);
        }
    }

    #[test]
    fn test_module_and_class_assignments_declare_variables() {
        let document = analyze(
            "LIMIT = 16\n\nclass Widget:\n    retries = 2\n    def open(self):\n        local = 1\n",
        );
        assert_eq!(symbol(&document, "LIMIT").kind, "variable");
        assert_eq!(symbol(&document, "LIMIT").kinds, [SymbolKind::Value]);
        assert_eq!(symbol(&document, "Widget.retries").kind, "variable");
        assert_eq!(symbol(&document, "Widget.open.local").kind, "variable");
    }

    #[test]
    fn test_assignment_declarations_keep_their_name_ranges() {
        let source = "value = make()\nclass Widget:\n    retries = 2\n";
        let document = analyze(source);
        for name in ["value", "Widget.retries"] {
            let declaration = symbol(&document, name);
            let range = declaration
                .name_range
                .expect("an assignment has a name token");
            assert_eq!(
                source.get(
                    usize::try_from(range.start).expect("name start")
                        ..usize::try_from(range.end).expect("name end")
                ),
                Some(declaration.name.as_str())
            );
        }
    }

    #[test]
    fn test_module_level_attribute_targets_declare_nothing() {
        let document = analyze("a, b = 1, 2\nself.count = 3\n");
        let names = document
            .symbols()
            .iter()
            .map(|symbol| symbol.qualified_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [".", "a", "b"],
            "an attribute outside a method declares nothing"
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
        let limits = SyntaxLimits::new(16, 250_000, 512).expect("positive fixture bounds");
        let error = PythonSyntaxProvider::default()
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: &text,
                },
                limits,
            )
            .expect_err("a source past the byte bound must refuse");
        assert!(error.to_string().contains("source"), "{error}");
    }

    #[test]
    fn test_syntax_element_kinds_cover_definitions_blocks_statements_and_comments() {
        let provider = PythonSyntaxProvider::default();
        assert_eq!(
            provider.syntax_element_kinds("function_definition"),
            [SyntaxKind::Declaration, SyntaxKind::Definition]
        );
        assert_eq!(provider.syntax_element_kinds("block"), [SyntaxKind::Block]);
        assert_eq!(
            provider.syntax_element_kinds("expression_statement"),
            [SyntaxKind::Statement]
        );
        assert_eq!(
            provider.syntax_element_kinds("comment"),
            [SyntaxKind::Comment]
        );
        assert_eq!(
            provider.syntax_element_kinds("decorator"),
            [SyntaxKind::Annotation]
        );
        assert_eq!(provider.syntax_element_kinds("identifier"), []);
    }

    #[test]
    fn test_provider_declares_language() {
        let provider = PythonSyntaxProvider::default();
        assert_eq!(provider.language().name, "python");
        assert_eq!(provider.language().dialect, None);
    }
}
