use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBinding, StaticBindingKind, StaticCallee, StaticDeclaredReturn, StaticFactsParts,
    StaticFunctionContext, StaticMember, StaticMemberCopy, StaticParameter, StaticReexport,
    StaticReference, binding, call, enclosing, field, identifier, kind,
};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};

mod documentation;
mod member_callbacks;

pub(super) use documentation::Documentation;

pub(super) struct Kinds {
    identifier: u16,
    property_identifier: u16,
    shorthand: u16,
    excluded_values: Vec<u16>,
    receivers: [u16; 2],
    private_property: u16,
    type_identifier: Option<u16>,
    nested_type: Option<u16>,
    type_parameter: Option<u16>,
    type_shapes: Option<TypeShapeKinds>,
    annotation: Option<u16>,
    asserted: Option<u16>,
    generic: Option<u16>,
    type_alias: Option<u16>,
    type_field: Option<u16>,
    parameter_wrappers: Vec<u16>,
    patterns: [u16; 3],
    shorthand_pattern: u16,
    pair_pattern: u16,
    /// An object literal and one of its `key: value` members.
    object_literal: [u16; 2],
    default_patterns: [u16; 2],
    pattern: Option<u16>,
    call: u16,
    member: u16,
    subscript: u16,
    parenthesized: u16,
    new: u16,
    string: u16,
    variable: u16,
    var: u16,
    for_in: u16,
    loop_kind: u16,
    assignment: [u16; 2],
    update: u16,
    function: [u16; 2],
    named_expression: [u16; 3],
    class: u16,
    edge_types: Vec<u16>,
    parameters: u16,
    argument_list: u16,
    return_statement: u16,
    arrow: u16,
    import: u16,
    import_clause: u16,
    import_specifier: u16,
    namespace_import: u16,
    export_specifier: u16,
    export: u16,
    module_syntax: ModuleSyntaxKinds,
    with: u16,
    catch: u16,
    scopes: [u16; 8],
    function_scopes: [u16; 8],
    name: u16,
    value: u16,
    callee: u16,
    object: u16,
    property: u16,
    constructor: u16,
    arguments: u16,
    source: u16,
    alias: u16,
    left: u16,
    right: u16,
    argument: u16,
    parameter: u16,
}

struct ModuleSyntaxKinds {
    meta_property: u16,
    await_expression: u16,
    await_token: u16,
}

impl ModuleSyntaxKinds {
    fn resolve(grammar: &tree_sitter::Language) -> Self {
        let await_token = grammar.id_for_node_kind("await", false);
        assert!(
            await_token != 0,
            "the pinned ECMAScript grammar must define the await token"
        );
        Self {
            meta_property: kind(grammar, "meta_property"),
            await_expression: kind(grammar, "await_expression"),
            await_token,
        }
    }
}

/// Grammar kinds read from TypeScript type declarations only.
struct TypeShapeKinds {
    interface: u16,
    /// Call and construct signatures, then function and constructor types.
    signatures: [u16; 4],
    /// Bodies that can hold a signature member, then the parenthesized type wrapper.
    signature_holders: [u16; 3],
    /// The `get` keyword of an accessor.
    getter: u16,
    /// Field holding a callable's declared return type.
    return_type: u16,
    /// A method with a body and a `return` statement.
    method: u16,
    return_statement: u16,
    /// Field holding an arrow function's or a function's body.
    body: u16,
    /// Fields holding a type parameter's constraint and a pattern pair's key.
    constraint: u16,
    key: u16,
    /// A member of an object type with a name and a type.
    property_signature: u16,
    /// A union of written types.
    union: u16,
    /// An array type, a `readonly` type operator and a generic's type argument list.
    array: [u16; 3],
}

impl TypeShapeKinds {
    fn resolve(grammar: &tree_sitter::Language) -> Self {
        let id = |name| kind(grammar, name);
        Self {
            interface: id("interface_declaration"),
            signatures: [
                id("call_signature"),
                id("construct_signature"),
                id("function_type"),
                id("constructor_type"),
            ],
            signature_holders: [
                id("interface_body"),
                id("object_type"),
                id("parenthesized_type"),
            ],
            getter: {
                let id = grammar.id_for_node_kind("get", false);
                assert_ne!(
                    id, 0,
                    "shipped grammar must define accessor keyword: kind=get"
                );
                id
            },
            return_type: field(grammar, "return_type"),
            method: id("method_definition"),
            return_statement: id("return_statement"),
            body: field(grammar, "body"),
            constraint: field(grammar, "constraint"),
            key: field(grammar, "key"),
            property_signature: id("property_signature"),
            union: id("union_type"),
            array: [id("array_type"), id("readonly_type"), id("type_arguments")],
        }
    }
}

pub(super) fn kinds() -> &'static Kinds {
    static KINDS: OnceLock<Kinds> = OnceLock::new();
    KINDS.get_or_init(|| Kinds::resolve_edge(&crate::javascript::javascript_grammar()))
}

pub(super) fn edge_typescript_kinds(
    dialect: crate::typescript::TypeScriptDialect,
) -> &'static Kinds {
    static TYPESCRIPT: OnceLock<Kinds> = OnceLock::new();
    static TSX: OnceLock<Kinds> = OnceLock::new();
    let cache = match dialect {
        crate::typescript::TypeScriptDialect::TypeScript => &TYPESCRIPT,
        crate::typescript::TypeScriptDialect::Tsx => &TSX,
    };
    cache.get_or_init(|| Kinds::resolve_edge(&dialect.grammar()))
}

fn edge_type_kinds(grammar: &tree_sitter::Language) -> Vec<u16> {
    if grammar.id_for_node_kind("type_identifier", true) == 0 {
        return Vec::new();
    }
    [
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "internal_module",
        "module",
    ]
    .into_iter()
    .map(|name| kind(grammar, name))
    .collect()
}

fn function_declaration_kinds(grammar: &tree_sitter::Language) -> [u16; 2] {
    [
        kind(grammar, "function_declaration"),
        kind(grammar, "generator_function_declaration"),
    ]
}

fn named_expression_kinds(grammar: &tree_sitter::Language) -> [u16; 3] {
    [
        kind(grammar, "function_expression"),
        kind(grammar, "generator_function"),
        kind(grammar, "class"),
    ]
}

fn scope_kinds(grammar: &tree_sitter::Language, program: u16) -> [u16; 8] {
    let id = |name| kind(grammar, name);
    [
        program,
        id("statement_block"),
        id("switch_body"),
        id("for_statement"),
        id("for_in_statement"),
        id("catch_clause"),
        id("class_static_block"),
        id("class_body"),
    ]
}

fn excluded_value_kinds(grammar: &tree_sitter::Language) -> Vec<u16> {
    [
        "import_statement",
        "export_clause",
        "namespace_export",
        "type_query",
        "nested_type_identifier",
    ]
    .into_iter()
    .filter_map(|name| {
        let kind = grammar.id_for_node_kind(name, true);
        (kind != 0).then_some(kind)
    })
    .collect()
}

impl Kinds {
    fn resolve_edge(grammar: &tree_sitter::Language) -> Self {
        let id = |name| kind(grammar, name);
        let field = |name| field(grammar, name);
        let program = id("program");
        let function = function_declaration_kinds(grammar);
        let named_expression = named_expression_kinds(grammar);
        let arrow = id("arrow_function");
        let typescript = grammar.id_for_node_kind("type_identifier", true) != 0;
        let function_scopes = [
            program,
            function[0],
            function[1],
            named_expression[0],
            named_expression[1],
            arrow,
            id("method_definition"),
            id("class_static_block"),
        ];
        Kinds {
            identifier: id("identifier"),
            property_identifier: id("property_identifier"),
            shorthand: id("shorthand_property_identifier"),
            excluded_values: excluded_value_kinds(grammar),
            receivers: [id("this"), id("super")],
            private_property: id("private_property_identifier"),
            type_identifier: typescript.then(|| id("type_identifier")),
            nested_type: typescript.then(|| id("nested_type_identifier")),
            type_parameter: typescript.then(|| id("type_parameter")),
            type_shapes: typescript.then(|| TypeShapeKinds::resolve(grammar)),
            annotation: typescript.then(|| id("type_annotation")),
            asserted: typescript.then(|| id("as_expression")),
            generic: typescript.then(|| id("generic_type")),
            type_alias: typescript.then(|| id("type_alias_declaration")),
            type_field: typescript.then(|| field("type")),
            parameter_wrappers: if typescript {
                vec![id("required_parameter"), id("optional_parameter")]
            } else {
                Vec::new()
            },
            patterns: [
                id("object_pattern"),
                id("array_pattern"),
                id("rest_pattern"),
            ],
            shorthand_pattern: id("shorthand_property_identifier_pattern"),
            pair_pattern: id("pair_pattern"),
            object_literal: [id("object"), id("pair")],
            default_patterns: [id("assignment_pattern"), id("object_assignment_pattern")],
            pattern: typescript.then(|| field("pattern")),
            call: id("call_expression"),
            member: id("member_expression"),
            subscript: id("subscript_expression"),
            parenthesized: id("parenthesized_expression"),
            new: id("new_expression"),
            string: id("string"),
            variable: id("variable_declarator"),
            var: id("variable_declaration"),
            for_in: id("for_in_statement"),
            loop_kind: field("kind"),
            assignment: [
                id("assignment_expression"),
                id("augmented_assignment_expression"),
            ],
            update: id("update_expression"),
            function,
            named_expression,
            class: id("class_declaration"),
            edge_types: edge_type_kinds(grammar),
            parameters: id("formal_parameters"),
            argument_list: id("arguments"),
            return_statement: id("return_statement"),
            arrow,
            import: id("import_statement"),
            import_clause: id("import_clause"),
            import_specifier: id("import_specifier"),
            namespace_import: id("namespace_import"),
            export_specifier: id("export_specifier"),
            export: id("export_statement"),
            module_syntax: ModuleSyntaxKinds::resolve(grammar),
            with: id("with_statement"),
            catch: id("catch_clause"),
            scopes: scope_kinds(grammar, program),
            function_scopes,
            name: field("name"),
            value: field("value"),
            callee: field("function"),
            object: field("object"),
            property: field("property"),
            constructor: field("constructor"),
            arguments: field("arguments"),
            source: field("source"),
            alias: field("alias"),
            left: field("left"),
            right: field("right"),
            argument: field("argument"),
            parameter: field("parameter"),
        }
    }
}

impl Kinds {
    pub(super) fn record(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        documentation: &mut Documentation,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        self.record_documented_function(syntax_element, source, documentation, facts)?;
        self.record_object_member(visited, facts)?;
        self.record_returned_object(visited, facts)?;
        self.record_spread_value(syntax_element, facts)?;
        self.record_module_syntax(visited, source, facts);
        self.record_direct_write(visited, source, facts)?;
        self.record_reference(visited, facts)?;
        self.record_member(syntax_element, facts)?;
        self.record_declared_type(syntax_element, source, facts)?;
        self.record_member_type_shape(syntax_element, source, facts)?;
        self.record_type_signature(syntax_element, facts)?;
        self.record_return_type(syntax_element, source, facts)?;
        self.record_function(visited, source, facts)?;
        if id == self.call {
            self.record_call(visited, source, facts)?;
        } else if id == self.new {
            if let Some(callee) = syntax_element.child_by_field_id(self.constructor) {
                facts
                    .calls
                    .push(call(syntax_element, callee, StaticCallee::Constructor)?);
            }
        } else if id == self.variable {
            self.record_variable(visited, source, facts)?;
        } else if self.type_parameter == Some(id) {
            self.record_type_parameter(visited, facts)?;
        } else if self.function.contains(&id) || id == self.class || self.edge_types.contains(&id) {
            self.record_declaration(visited, source, facts)?;
        } else if id == self.import_specifier || self.is_default_import(visited) {
            self.record_import(visited, source, facts)?;
        } else if id == self.namespace_import {
            self.record_namespace_import(visited, source, facts)?;
        } else if id == self.export_specifier {
            self.record_reexport(visited, source, facts)?;
        } else {
            self.record_shadow_or_mutation(visited, source, facts)?;
        }
        Ok(())
    }

    /// Records a declared name whose initializer is an object literal that ends in a spread
    /// of a name: `const o = { a: 1, ...base }`. The members the spread value has are the
    /// last ones written, so they are the members of `o` that value declares.
    fn record_spread_value(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if element.kind_id() != self.variable {
            return Ok(());
        }
        let Some(name) = element
            .child_by_field_id(self.name)
            .filter(|name| name.kind_id() == self.identifier)
        else {
            return Ok(());
        };
        let mut value = element.child_by_field_id(self.value);
        // Each step descends within the admitted syntax depth.
        while let Some(inner) = value.filter(|node| node.kind_id() == self.parenthesized) {
            value = inner.named_child(0);
        }
        let Some(object) = value.filter(|node| node.kind_id() == self.object_literal[0]) else {
            return Ok(());
        };
        let mut cursor = object.walk();
        let last = object
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .last();
        let spread = last
            .filter(|child| child.kind() == "spread_element")
            .and_then(|child| child.named_child(0))
            .filter(|argument| argument.kind_id() == self.identifier && !argument.is_missing());
        if let Some(spread) = spread {
            facts
                .spread_values
                .push((byte_range(name)?, byte_range(spread)?));
        }
        Ok(())
    }

    /// Records what a `return` returns, or what an arrow function's expression body is:
    /// the object literal, or none for any other value. Keyed by the name of the
    /// declaration the function belongs to.
    fn record_returned_object(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let (function, value) = if element.kind_id() == self.return_statement {
            // The syntax walk has admitted every enclosing statement's depth.
            let mut enclosing = visited.parent();
            while let Some(candidate) = enclosing {
                let id = candidate.syntax_element().kind_id();
                if id == self.arrow
                    || id == self.function_scopes[6]
                    || self.function.contains(&id)
                    || self.named_expression[..2].contains(&id)
                {
                    break;
                }
                enclosing = candidate.parent();
            }
            let Some(function) = enclosing else {
                return Ok(());
            };
            let mut cursor = element.walk();
            let value = element
                .named_children(&mut cursor)
                .find(|child| !child.is_extra());
            (function, value)
        } else if element.kind_id() == self.arrow {
            let body = self.type_shapes.as_ref().map_or_else(
                || element.child_by_field_name("body"),
                |shapes| element.child_by_field_id(shapes.body),
            );
            match body {
                Some(body) if body.kind() != "statement_block" => (visited, Some(body)),
                _ => return Ok(()),
            }
        } else {
            return Ok(());
        };
        let function_element = function.syntax_element();
        let owner = if function_element.kind_id() == self.arrow
            || self.named_expression[..2].contains(&function_element.kind_id())
        {
            // A function value belongs to the declaration it initializes.
            function
                .parent()
                .map(Visited::syntax_element)
                .filter(|parent| parent.child_by_field_id(self.value) == Some(function_element))
                .and_then(|parent| parent.child_by_field_id(self.name))
        } else {
            function_element.child_by_field_id(self.name)
        };
        let Some(owner) = owner else {
            return Ok(());
        };
        let mut value = value;
        // Each step descends within the admitted syntax depth.
        while let Some(inner) = value.filter(|node| node.kind_id() == self.parenthesized) {
            value = inner.named_child(0);
        }
        let object = value
            .filter(|node| node.kind_id() == self.object_literal[0])
            .map(byte_range)
            .transpose()?;
        facts.returned_objects.push((byte_range(owner)?, object));
        Ok(())
    }

    /// Records a function written as a member of an object literal: a method, or a
    /// `function` expression given as a `key: value` member. Arrow functions take `this`
    /// from their surroundings and are not recorded.
    fn record_object_member(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let id = element.kind_id();
        let object = if id == self.function_scopes[6] {
            visited.parent()
        } else if self.named_expression[..2].contains(&id) {
            visited
                .parent()
                .filter(|pair| {
                    pair.syntax_element().kind_id() == self.object_literal[1]
                        && pair.syntax_element().child_by_field_id(self.value) == Some(element)
                })
                .and_then(Visited::parent)
        } else {
            return Ok(());
        }
        .map(Visited::syntax_element);
        if let Some(object) = object.filter(|object| object.kind_id() == self.object_literal[0]) {
            facts
                .object_members
                .push((byte_range(element)?, byte_range(object)?));
        }
        Ok(())
    }

    /// Records a write of the `CommonJS` module value in a statement of the source file itself,
    /// with no condition and no function around it.
    fn record_direct_write(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        if element.kind_id() != self.assignment[0]
            || !element
                .child_by_field_id(self.left)
                .and_then(|left| text(left, source))
                .is_some_and(|left| matches!(left, "module.exports" | "exports"))
        {
            return Ok(());
        }
        // The syntax walk has admitted every enclosing element's depth.
        let mut parent = visited.parent();
        while let Some(candidate) = parent {
            let kind = candidate.syntax_element().kind();
            if kind == "program" {
                facts.commonjs_direct_writes.push(byte_range(element)?);
                break;
            }
            if !matches!(
                kind,
                "assignment_expression"
                    | "expression_statement"
                    | "variable_declarator"
                    | "variable_declaration"
                    | "lexical_declaration"
            ) {
                break;
            }
            parent = candidate.parent();
        }
        Ok(())
    }

    fn record_module_syntax(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) {
        if facts.javascript_module_syntax {
            return;
        }
        let element = visited.syntax_element();
        let id = element.kind_id();
        if [self.import, self.export].contains(&id)
            || (id == self.module_syntax.meta_property
                && element.child(0).and_then(|first| text(first, source)) == Some("import"))
        {
            facts.javascript_module_syntax = true;
            return;
        }
        let await_loop = id == self.for_in && {
            let mut cursor = element.walk();
            // Immediate children are bounded by the admitted source and syntax selection.
            element
                .children(&mut cursor)
                .any(|child| child.kind_id() == self.module_syntax.await_token)
        };
        if (id == self.module_syntax.await_expression || await_loop)
            && enclosing(visited, &self.function_scopes)
                .is_some_and(|scope| scope.syntax_element().kind_id() == self.function_scopes[0])
        {
            facts.javascript_module_syntax = true;
        }
    }

    fn wrapper_name(name: Option<&str>) -> bool {
        matches!(
            name,
            Some("require" | "module" | "exports" | "__dirname" | "__filename")
        )
    }

    fn record_member(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = element.kind_id();
        if ![self.member, self.subscript].contains(&id) {
            return Ok(());
        }
        let Some(receiver) = element
            .child_by_field_id(self.object)
            .filter(|receiver| !receiver.is_missing())
        else {
            return Ok(());
        };
        let member = element.child_by_field_id(self.property).filter(|member| {
            [self.property_identifier, self.private_property].contains(&member.kind_id())
                && !member.is_missing()
        });
        facts.members.push(StaticMember {
            range: byte_range(element)?,
            receiver: byte_range(receiver)?,
            member: member.map(byte_range).transpose()?,
        });
        Ok(())
    }

    fn record_function(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        if element.kind() == "class_static_block" {
            facts.static_blocks.push(byte_range(element)?);
        }
        if element.kind_id() != self.arrow
            && !self.named_expression[..2].contains(&element.kind_id())
        {
            return Ok(());
        }
        let range = byte_range(element)?;
        if element.kind_id() != self.arrow {
            facts.function_scopes.push(range);
        }
        let mut value = visited;
        // The syntax walk has admitted every enclosing expression's depth.
        loop {
            facts
                .function_values
                .push((range, byte_range(value.syntax_element())?));
            let Some(parent) = value.parent() else {
                break;
            };
            if !matches!(
                parent.syntax_element().kind(),
                "parenthesized_expression"
                    | "as_expression"
                    | "type_assertion"
                    | "satisfies_expression"
                    | "non_null_expression"
            ) {
                break;
            }
            value = parent;
        }
        if let Some(context) = self.function_context(value)? {
            facts.function_contexts.push((range, context));
        }
        self.record_asserted_callback(element, value, source, facts)?;
        self.record_member_callback(element, value, facts)
    }

    /// Types the parameters of a callback passed to a member of a value asserted to an
    /// inline object type: in `(x as { m: (fn: (v: T) => void) => void }).m((v) => ...)`
    /// the parameter `v` is declared `T`. The asserted type, the member's function type and
    /// the callback's function type must all be written; positions without a named type, and
    /// callback parameters with their own annotation, type nothing.
    fn record_asserted_callback(
        &self,
        function: Node<'_>,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let value = visited.syntax_element();
        let Some(arguments) = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.argument_list)
        else {
            return Ok(());
        };
        let Some(callee) = arguments
            .parent()
            .map(Visited::syntax_element)
            .filter(|call| call.kind_id() == self.call)
            .and_then(|call| call.child_by_field_id(self.callee))
            .filter(|callee| callee.kind_id() == self.member)
        else {
            return Ok(());
        };
        let arguments = arguments.syntax_element();
        let mut cursor = arguments.walk();
        let Some(index) = arguments
            .named_children(&mut cursor)
            .filter(|argument| !argument.is_extra())
            .position(|argument| argument == value)
        else {
            return Ok(());
        };
        let member = callee
            .child_by_field_id(self.property)
            .and_then(|property| text(property, source));
        let mut object = callee.child_by_field_id(self.object);
        // Each step descends within the admitted syntax depth.
        while let Some(inner) = object.filter(|node| node.kind_id() == self.parenthesized) {
            object = inner.named_child(0);
        }
        let asserted = object
            .filter(|node| node.kind() == "as_expression")
            .and_then(|node| node.named_child(1))
            .map(|written| Self::unparenthesized(written, shapes));
        let Some((asserted, member)) = asserted
            .filter(|written| written.kind_id() == shapes.signature_holders[1])
            .zip(member)
        else {
            return Ok(());
        };
        let mut cursor = asserted.walk();
        let signature = asserted
            .named_children(&mut cursor)
            .filter(|child| child.kind_id() == shapes.property_signature)
            .find(|child| {
                child
                    .child_by_field_id(self.name)
                    .and_then(|name| text(name, source))
                    == Some(member)
            });
        let callback = signature
            .and_then(|signature| self.written_function_type(signature, shapes))
            .and_then(|member_type| self.function_type_parameter(member_type, index))
            .and_then(|parameter| self.written_function_type(parameter, shapes));
        let Some(callback) = callback else {
            return Ok(());
        };
        let own = self.callback_parameters(function);
        for (position, parameter) in own.into_iter().enumerate() {
            let Some(parameter) = self.unannotated_callback_parameter(parameter) else {
                continue;
            };
            let written = self
                .function_type_parameter(callback, position)
                .and_then(|written| {
                    self.type_field
                        .and_then(|field| written.child_by_field_id(field))
                })
                .and_then(|annotation| annotation.named_child(0))
                .map(|written| self.nullable_type(written, source))
                .and_then(|written| self.named_type(written));
            if let Some(written) = written {
                facts
                    .declared_types
                    .push((byte_range(parameter)?, byte_range(written)?));
            }
        }
        Ok(())
    }

    /// The name of a callback parameter to which a written contextual type can apply.
    fn unannotated_callback_parameter<'tree>(&self, parameter: Node<'tree>) -> Option<Node<'tree>> {
        if self
            .type_field
            .is_some_and(|field| parameter.child_by_field_id(field).is_some())
        {
            return None;
        }
        let name = if self.parameter_wrappers.contains(&parameter.kind_id()) {
            parameter.child_by_field_id(self.pattern?)?
        } else {
            parameter
        };
        (name.kind_id() == self.identifier).then_some(name)
    }

    /// The parameters of a function value: its single unparenthesized parameter, or the
    /// entries of its parameter list.
    fn callback_parameters<'tree>(&self, function: Node<'tree>) -> Vec<Node<'tree>> {
        if let Some(single) = function.child_by_field_id(self.parameter) {
            return vec![single];
        }
        let mut cursor = function.walk();
        let Some(list) = function
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == self.parameters)
        else {
            return Vec::new();
        };
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .collect()
    }

    /// The function type written as the type of `holder` (a member signature or a
    /// parameter), through parentheses.
    fn written_function_type<'tree>(
        &self,
        holder: Node<'tree>,
        shapes: &TypeShapeKinds,
    ) -> Option<Node<'tree>> {
        let annotation = self
            .type_field
            .and_then(|field| holder.child_by_field_id(field))?
            .named_child(0)?;
        Some(Self::unparenthesized(annotation, shapes))
            .filter(|written| written.kind_id() == shapes.signatures[2])
    }

    /// The parameter at `index` of a written function type.
    fn function_type_parameter<'tree>(
        &self,
        function_type: Node<'tree>,
        index: usize,
    ) -> Option<Node<'tree>> {
        let mut cursor = function_type.walk();
        let list = function_type
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == self.parameters)?;
        let mut cursor = list.walk();
        list.named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .nth(index)
    }

    /// The type a parenthesized type writes.
    fn unparenthesized<'tree>(mut written: Node<'tree>, shapes: &TypeShapeKinds) -> Node<'tree> {
        // Each step descends within the admitted syntax depth.
        while written.kind_id() == shapes.signature_holders[2] {
            let Some(inner) = written.named_child(0) else {
                break;
            };
            written = inner;
        }
        written
    }

    /// The written place that can type the function value `value`: the declaration it
    /// initializes, the call it is an argument of, or the function that returns it.
    fn function_context(
        &self,
        visited: Visited<'_, '_>,
    ) -> Result<Option<StaticFunctionContext>, RiftError> {
        let value = visited.syntax_element();
        let Some(parent) = visited.parent() else {
            return Ok(None);
        };
        let parent_element = parent.syntax_element();
        // An argument position types a callback in either grammar: a declared parameter of
        // the callee, or the element type of an array a standard array method walks.
        if parent_element.kind_id() == self.argument_list
            && let Some(call) = parent.parent()
        {
            let mut cursor = parent_element.walk();
            let index = parent_element
                .named_children(&mut cursor)
                .filter(|argument| !argument.is_extra())
                .position(|argument| argument == value);
            return Ok(match index.and_then(|index| u32::try_from(index).ok()) {
                Some(index) => Some(StaticFunctionContext::Argument {
                    call: byte_range(call.syntax_element())?,
                    index,
                }),
                None => None,
            });
        }
        let Some(shapes) = &self.type_shapes else {
            return Ok(None);
        };
        if parent_element.child_by_field_id(self.value) == Some(value)
            && let Some(name) = parent_element.child_by_field_id(self.name)
        {
            return Ok(Some(StaticFunctionContext::Initializer(byte_range(name)?)));
        }
        if parent_element.kind_id() == self.arrow
            && parent_element.child_by_field_id(shapes.body) == Some(value)
        {
            return Ok(Some(StaticFunctionContext::Returned(byte_range(
                parent_element,
            )?)));
        }
        if parent_element.kind_id() != shapes.return_statement {
            return Ok(None);
        }
        // The syntax walk has admitted every enclosing statement's depth.
        let mut enclosing = parent.parent();
        while let Some(function) = enclosing {
            let id = function.syntax_element().kind_id();
            if id == self.arrow
                || id == shapes.method
                || self.function.contains(&id)
                || self.named_expression[..2].contains(&id)
            {
                return Ok(Some(StaticFunctionContext::Returned(byte_range(
                    function.syntax_element(),
                )?)));
            }
            enclosing = function.parent();
        }
        Ok(None)
    }

    /// Records the named type a function's return annotation writes.
    fn record_return_type(
        &self,
        element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let Some(mut annotation) = element.child_by_field_id(shapes.return_type) else {
            return Ok(());
        };
        if self.annotation == Some(annotation.kind_id()) {
            let mut cursor = annotation.walk();
            let Some(inner) = annotation
                .named_children(&mut cursor)
                .find(|child| !child.is_extra() && !child.is_missing())
            else {
                return Ok(());
            };
            annotation = inner;
        }
        let annotation = self.nullable_type(annotation, source);
        let owner = self.function_owner(element)?;
        if let Some((name, written)) = owner.zip(self.element_type(annotation, source)) {
            facts.element_returns.push((name, byte_range(written)?));
            return Ok(());
        }
        if annotation.kind_id() == shapes.signature_holders[1] {
            // An inline object type: each named member types the value under that key.
            let Some(owner) = owner else {
                return Ok(());
            };
            let mut cursor = annotation.walk();
            for member in annotation.named_children(&mut cursor) {
                if member.kind_id() != shapes.property_signature {
                    continue;
                }
                let written = self
                    .type_field
                    .and_then(|field| member.child_by_field_id(field))
                    .and_then(|annotation| annotation.named_child(0))
                    .and_then(|written| self.named_type(written));
                if let Some((name, written)) = member.child_by_field_id(self.name).zip(written) {
                    facts.declared_returns.push(StaticDeclaredReturn {
                        owner,
                        function: byte_range(element)?,
                        member: Some(byte_range(name)?),
                        annotation: byte_range(written)?,
                    });
                }
            }
            return Ok(());
        }
        if let Some(annotation) = self.named_type(annotation) {
            facts
                .return_types
                .push((byte_range(element)?, byte_range(annotation)?));
            if let Some(owner) = owner {
                facts.declared_returns.push(StaticDeclaredReturn {
                    owner,
                    function: byte_range(element)?,
                    member: None,
                    annotation: byte_range(annotation)?,
                });
            }
        }
        Ok(())
    }

    fn record_type_parameter(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let parameter = visited.syntax_element();
        let name = parameter.child_by_field_id(self.name);
        let scope = visited
            .parent()
            .and_then(Visited::parent)
            .map(Visited::syntax_element);
        if let Some((name, scope)) = name.zip(scope) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Shadow,
            )?);
        }
        if let Some(scope) = scope {
            self.record_type_parameter_errors(parameter, scope, facts)?;
        }
        // A value typed by the parameter has at least the members its constraint names.
        let constraint = self
            .type_shapes
            .as_ref()
            .and_then(|shapes| parameter.child_by_field_id(shapes.constraint))
            .and_then(|constraint| constraint.named_child(0))
            .and_then(|written| self.named_type(written));
        if let Some((name, constraint)) = name.zip(constraint) {
            facts
                .declared_types
                .push((byte_range(name)?, byte_range(constraint)?));
        }
        Ok(())
    }

    /// The one type a union writes besides `null` and `undefined`, or the written type
    /// itself: `T | undefined` and `(T | null)` are written as `T`.
    ///
    /// `Partial<T>`, `Readonly<T>` and `Required<T>` keep every member name of `T`, so
    /// they are written as `T` too. No other utility type is.
    fn nullable_type<'tree>(&self, written: Node<'tree>, source: &str) -> Node<'tree> {
        let mut written = self.nullable_member(written, source);
        // Each argument lies inside the written type the syntax walk has admitted.
        while let Some(inner) = self.projected_argument(written, source) {
            written = self.nullable_member(inner, source);
        }
        written
    }

    /// The single type argument of `Partial<T>`, `Readonly<T>` or `Required<T>`.
    fn projected_argument<'tree>(&self, written: Node<'tree>, source: &str) -> Option<Node<'tree>> {
        let shapes = self.type_shapes.as_ref()?;
        if self.generic != Some(written.kind_id())
            || !matches!(
                written
                    .child_by_field_id(self.name)
                    .and_then(|name| text(name, source)),
                Some("Partial" | "Readonly" | "Required")
            )
        {
            return None;
        }
        let mut cursor = written.walk();
        let arguments = written
            .named_children(&mut cursor)
            .find(|child| child.kind_id() == shapes.array[2])?;
        let mut cursor = arguments.walk();
        let mut named = arguments
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra());
        let first = named.next()?;
        named.next().is_none().then_some(first)
    }

    fn nullable_member<'tree>(&self, mut written: Node<'tree>, source: &str) -> Node<'tree> {
        let Some(shapes) = &self.type_shapes else {
            return written;
        };
        // The syntax walk has admitted every enclosing type's depth.
        while written.kind_id() == shapes.signature_holders[2] {
            let Some(inner) = written.named_child(0) else {
                return written;
            };
            written = inner;
        }
        if written.kind_id() != shapes.union {
            return written;
        }
        let mut named = None;
        let mut pending = vec![written];
        // Each member lies inside the written type the syntax walk has admitted.
        while let Some(member) = pending.pop() {
            if member.kind_id() == shapes.union || member.kind_id() == shapes.signature_holders[2] {
                let mut cursor = member.walk();
                pending.extend(
                    member
                        .named_children(&mut cursor)
                        .filter(|child| !child.is_extra()),
                );
                continue;
            }
            if matches!(text(member, source), Some("null" | "undefined")) {
                continue;
            }
            if named.replace(member).is_some() {
                return written;
            }
        }
        named.unwrap_or(written)
    }

    /// The named element type an array type writes: `T[]`, `readonly T[]`, `Array<T>` and
    /// `ReadonlyArray<T>`, with `T` a type name or a generic type's name.
    fn element_type<'tree>(&self, written: Node<'tree>, source: &str) -> Option<Node<'tree>> {
        let shapes = self.type_shapes.as_ref()?;
        let written = if written.kind_id() == shapes.array[1] {
            written.named_child(0)?
        } else {
            written
        };
        let element = if written.kind_id() == shapes.array[0] {
            written.named_child(0)?
        } else if self.generic == Some(written.kind_id())
            && matches!(
                written
                    .child_by_field_id(self.name)
                    .and_then(|name| text(name, source)),
                Some("Array" | "ReadonlyArray")
            )
        {
            let mut cursor = written.walk();
            let arguments = written
                .named_children(&mut cursor)
                .find(|child| child.kind_id() == shapes.array[2])?;
            let mut cursor = arguments.walk();
            let mut named = arguments
                .named_children(&mut cursor)
                .filter(|child| !child.is_extra());
            let first = named.next()?;
            if named.next().is_some() {
                return None;
            }
            first
        } else {
            return None;
        };
        self.named_type(element)
    }

    /// The name a written type refers to: a type name, or the name of a generic type.
    fn named_type<'tree>(&self, written: Node<'tree>) -> Option<Node<'tree>> {
        let written = if self.generic == Some(written.kind_id()) {
            written.child_by_field_id(self.name)?
        } else {
            written
        };
        (!written.is_missing()
            && (self.type_identifier == Some(written.kind_id())
                || self.nested_type == Some(written.kind_id())))
        .then_some(written)
    }

    /// Names the parser left inside an error of a type parameter list.
    ///
    /// The grammar has no variance annotation, so `out T` parses as a parameter named `out`
    /// followed by an error holding `T`, or as an error holding `out` before the parameter.
    /// Each such name is declared by the list: it shadows outer declarations for the whole
    /// declaration and never resolves outside it. Errors between two parameters are read
    /// with the parameter that follows them, trailing errors with the last parameter.
    fn record_type_parameter_errors(
        &self,
        parameter: Node<'_>,
        scope: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let mut errors = Vec::new();
        let mut cursor = parameter.walk();
        errors.extend(parameter.children(&mut cursor).filter(Node::is_error));
        // Sibling walks stop at the next parameter, so each is bounded by the list.
        let mut before = parameter.prev_sibling();
        while let Some(sibling) = before.filter(|sibling| sibling.is_error() || !sibling.is_named())
        {
            errors.extend(sibling.is_error().then_some(sibling));
            before = sibling.prev_sibling();
        }
        let mut trailing = Vec::new();
        let mut after = parameter.next_sibling();
        while let Some(sibling) = after.filter(|sibling| sibling.is_error() || !sibling.is_named())
        {
            trailing.extend(sibling.is_error().then_some(sibling));
            after = sibling.next_sibling();
        }
        if after.is_none() {
            errors.append(&mut trailing);
        }
        let available_from = byte_range(scope)?.start;
        for error in errors {
            let mut names = error.walk();
            for name in error.named_children(&mut names) {
                if name.kind_id() == self.identifier || self.type_identifier == Some(name.kind_id())
                {
                    facts.bindings.push(binding(
                        name,
                        scope,
                        available_from,
                        StaticBindingKind::Shadow,
                    )?);
                }
            }
        }
        Ok(())
    }

    fn record_call(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(callee) = syntax_element
            .child_by_field_id(self.callee)
            .filter(|syntax_element| !syntax_element.is_missing())
        else {
            return Ok(());
        };
        let shape = if callee.kind_id() == self.identifier {
            if text(callee, source) == Some("eval") {
                let root = enclosing(visited, &self.scopes[..1])
                    .map_or(syntax_element, Visited::syntax_element);
                facts.unsupported_bindings.push(byte_range(root)?);
            }
            StaticCallee::Direct
        } else if callee.kind_id() == self.receivers[1] {
            StaticCallee::Constructor
        } else {
            self.member(callee)?
        };
        facts.calls.push(call(syntax_element, callee, shape)?);
        Ok(())
    }

    fn member(&self, syntax_element: Node<'_>) -> Result<StaticCallee, RiftError> {
        if syntax_element.kind_id() != self.member {
            return Ok(StaticCallee::Unsupported);
        }
        // A call result and a constructed value are receivers too: a declared return type
        // or the constructed class names their members.
        let receiver = syntax_element
            .child_by_field_id(self.object)
            .filter(|receiver| {
                self.receiver(*receiver) || [self.call, self.new].contains(&receiver.kind_id())
            });
        let member = syntax_element
            .child_by_field_id(self.property)
            .filter(|member| {
                [self.property_identifier, self.private_property].contains(&member.kind_id())
            });
        match receiver.zip(member) {
            Some((receiver, member)) => Ok(StaticCallee::Member {
                receiver: byte_range(receiver)?,
                member: byte_range(member)?,
            }),
            None => Ok(StaticCallee::Unsupported),
        }
    }

    fn receiver(&self, receiver: Node<'_>) -> bool {
        [self.identifier, self.member].contains(&receiver.kind_id())
            || self.receivers.contains(&receiver.kind_id())
    }

    fn record_reference(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let id = element.kind_id();
        let lexical = [self.identifier, self.shorthand, self.shorthand_pattern].contains(&id)
            && self.value_reference(visited);
        if id == self.member || lexical {
            let access = self.access_target(visited);
            let parent = access.parent().map(Visited::syntax_element);
            let access = access.syntax_element();
            let assigned = parent.is_some_and(|parent| {
                let assignment = self.assignment.contains(&parent.kind_id())
                    || (parent.kind_id() == self.for_in
                        && parent.child_by_field_id(self.loop_kind).is_none());
                assignment && parent.child_by_field_id(self.left) == Some(access)
            });
            let updated = parent.is_some_and(|parent| parent.kind_id() == self.update);
            let augmented = parent.is_some_and(|parent| parent.kind_id() == self.assignment[1]);
            let target = byte_range(element)?;
            if !assigned || augmented || updated {
                facts.references.push(StaticReference {
                    target,
                    role: rift_core::ReferenceRole::Read,
                });
            }
            if assigned || updated {
                facts.references.push(StaticReference {
                    target,
                    role: rift_core::ReferenceRole::Write,
                });
            }
        } else if self.type_identifier == Some(id) || self.nested_type == Some(id) {
            let parent = visited.parent().map(Visited::syntax_element);
            let declared = parent.is_some_and(|parent| {
                parent.child_by_field_id(self.name) == Some(element)
                    && (self.edge_types.contains(&parent.kind_id())
                        || parent.kind_id() == self.class
                        || self.type_parameter == Some(parent.kind_id()))
            });
            let nested = parent.is_some_and(|parent| self.nested_type == Some(parent.kind_id()));
            if !declared && !nested {
                facts.references.push(StaticReference {
                    target: byte_range(element)?,
                    role: rift_core::ReferenceRole::Type,
                });
            }
        }
        Ok(())
    }

    fn value_reference(&self, mut visited: Visited<'_, '_>) -> bool {
        if enclosing(visited, &self.excluded_values).is_some() {
            return false;
        }
        while let Some(recorded) = visited.parent() {
            let (current, parent) = (visited.syntax_element(), recorded.syntax_element());
            let id = parent.kind_id();
            if parent.child_by_field_id(self.name) == Some(current)
                || id == self.parameters
                || (id == self.arrow && parent.child_by_field_id(self.parameter) == Some(current))
                || (self.parameter_wrappers.contains(&id)
                    && self
                        .pattern
                        .and_then(|field| parent.child_by_field_id(field))
                        == Some(current))
                || (id == self.for_in
                    && parent.child_by_field_id(self.loop_kind).is_some()
                    && parent.child_by_field_id(self.left) == Some(current))
            {
                return false;
            }
            if !self.pattern_parent(current, parent) {
                break;
            }
            visited = recorded;
        }
        true
    }

    fn pattern_parent(&self, child: Node<'_>, parent: Node<'_>) -> bool {
        let id = parent.kind_id();
        self.patterns.contains(&id)
            || (id == self.pair_pattern && parent.child_by_field_id(self.value) == Some(child))
            || (self.default_patterns.contains(&id)
                && parent.child_by_field_id(self.left) == Some(child))
    }

    fn access_target<'walk, 'tree>(&self, visited: Visited<'walk, 'tree>) -> Visited<'walk, 'tree> {
        let mut current = visited;
        while let Some(parent) = current.parent() {
            if !self.pattern_parent(current.syntax_element(), parent.syntax_element()) {
                break;
            }
            current = parent;
        }
        current
    }

    fn record_declared_type(
        &self,
        element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = element.child_by_field_id(self.name).or_else(|| {
            self.pattern
                .and_then(|field| element.child_by_field_id(field))
        });
        let annotation = if self.type_alias == Some(element.kind_id()) {
            element.child_by_field_id(self.value)
        } else {
            self.type_field
                .and_then(|field| element.child_by_field_id(field))
                .or_else(|| self.getter_return_type(element))
                .or_else(|| self.asserted_type(element))
        };
        let Some((name, mut annotation)) = name.zip(annotation) else {
            return Ok(());
        };
        if self.annotation == Some(annotation.kind_id()) {
            let mut cursor = annotation.walk();
            let Some(inner) = annotation
                .named_children(&mut cursor)
                .find(|child| !child.is_extra() && !child.is_missing())
            else {
                return Ok(());
            };
            annotation = inner;
        }
        annotation = self.nullable_type(annotation, source);
        if let Some(element) = self.element_type(annotation, source) {
            facts
                .element_types
                .push((byte_range(name)?, byte_range(element)?));
            return Ok(());
        }
        if self.generic == Some(annotation.kind_id()) {
            let Some(inner) = annotation.child_by_field_id(self.name) else {
                return Ok(());
            };
            annotation = inner;
        }
        if name.is_missing() || annotation.is_missing() {
            return Ok(());
        }
        if let Some(shapes) = &self.type_shapes {
            // The syntax walk has admitted every enclosing type's depth.
            let mut written = annotation;
            while written.kind_id() == shapes.signature_holders[2] {
                let Some(inner) = written.named_child(0) else {
                    break;
                };
                written = inner;
            }
            if written.kind_id() == shapes.signatures[2] {
                facts
                    .function_types
                    .push((byte_range(name)?, byte_range(written)?));
                return Ok(());
            }
        }
        if (self.type_identifier == Some(annotation.kind_id())
            || self.nested_type == Some(annotation.kind_id()))
            && ([
                self.identifier,
                self.property_identifier,
                self.private_property,
            ]
            .contains(&name.kind_id())
                || self.type_identifier == Some(name.kind_id()))
        {
            facts
                .declared_types
                .push((byte_range(name)?, byte_range(annotation)?));
        }
        Ok(())
    }

    /// The type the initializer of a variable without its own annotation asserts:
    /// `const name = value as T` declares `name` as `T`. In `value as any as T` the outer
    /// assertion decides.
    fn asserted_type<'tree>(&self, element: Node<'tree>) -> Option<Node<'tree>> {
        if element.kind_id() != self.variable {
            return None;
        }
        let mut value = element.child_by_field_id(self.value)?;
        // Each step descends within the admitted syntax depth.
        while value.kind_id() == self.parenthesized {
            value = value.named_child(0)?;
        }
        if Some(value.kind_id()) != self.asserted {
            return None;
        }
        value.named_child(1)
    }

    /// The declared return type of a `get` accessor: the type its property reads as.
    fn getter_return_type<'tree>(&self, element: Node<'tree>) -> Option<Node<'tree>> {
        let shapes = self.type_shapes.as_ref()?;
        let mut cursor = element.walk();
        element
            .children(&mut cursor)
            .any(|child| child.kind_id() == shapes.getter)
            .then(|| element.child_by_field_id(shapes.return_type))
            .flatten()
    }

    /// Records the type declaration whose own shape is this signature.
    ///
    /// `interface I { (): void }`, `type F = () => void` and `type C = new () => T` state
    /// that a value of the named type can be called or constructed. A signature nested
    /// deeper, such as a member's type, says nothing about the declaration itself.
    fn record_type_signature(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let (signatures, holders) = (shapes.signatures, shapes.signature_holders);
        let Some(position) = signatures
            .iter()
            .position(|kind| *kind == element.kind_id())
        else {
            return Ok(());
        };
        let constructs = position % 2 == 1;
        let member = position < 2;
        let mut holder = element.parent();
        if member {
            // A signature member belongs to an interface body or an aliased object type.
            let Some(body) = holder.filter(|body| holders[..2].contains(&body.kind_id())) else {
                return Ok(());
            };
            holder = body.parent();
        }
        // The syntax walk has admitted every enclosing type's depth.
        while let Some(wrapper) = holder.filter(|wrapper| wrapper.kind_id() == holders[2]) {
            holder = wrapper.parent();
        }
        let Some(declaration) = holder else {
            return Ok(());
        };
        let declares = self.type_alias == Some(declaration.kind_id())
            || (member && shapes.interface == declaration.kind_id());
        if !declares {
            return Ok(());
        }
        if let Some(name) = declaration
            .child_by_field_id(self.name)
            .filter(|name| !name.is_missing())
        {
            facts.type_signatures.push((byte_range(name)?, constructs));
            if position == 0 {
                // A call signature states the parameters a value of the type is called with.
                facts
                    .function_types
                    .push((byte_range(name)?, byte_range(element)?));
            }
        }
        Ok(())
    }

    fn record_declaration(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let scope = enclosing(visited, &self.scopes).map(Visited::syntax_element);
        let name = syntax_element
            .child_by_field_id(self.name)
            .filter(|syntax_element| !syntax_element.is_missing());
        if let Some((name, scope)) = name.zip(scope) {
            if syntax_element.kind_id() == self.class
                && scope.kind_id() == self.scopes[0]
                && Self::wrapper_name(text(name, source))
            {
                facts.javascript_module_syntax = true;
            }
            let available = if syntax_element.kind_id() == self.class
                || self.edge_types.first() == Some(&syntax_element.kind_id())
            {
                byte_range(syntax_element)?.start
            } else {
                byte_range(scope)?.start
            };
            facts
                .bindings
                .push(binding(name, scope, available, StaticBindingKind::Local)?);
        }
        Ok(())
    }

    fn record_variable(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(parent) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let is_var = parent.kind_id() == self.var;
        let scopes = if is_var {
            &self.function_scopes
        } else {
            &self.scopes
        };
        let Some(scope) = enclosing(visited, scopes).map(Visited::syntax_element) else {
            return Ok(());
        };
        let Some(name) = syntax_element.child_by_field_id(self.name) else {
            return Ok(());
        };
        if name.kind_id() != self.identifier {
            let start = facts.bindings.len();
            self.record_destructured(name, syntax_element, facts)?;
            if !self.record_required_names(name, syntax_element, scope, source, facts)? {
                self.record_parameter(name, scope, facts)?;
            }
            if !is_var && scope.kind_id() == self.scopes[0] {
                facts.javascript_module_syntax |= facts.bindings[start..].iter().any(|binding| {
                    let start = usize::try_from(binding.name.start).ok();
                    let end = usize::try_from(binding.name.end).ok();
                    Self::wrapper_name(
                        start
                            .zip(end)
                            .and_then(|(start, end)| source.get(start..end)),
                    )
                });
            }
            return Ok(());
        }
        if !is_var && scope.kind_id() == self.scopes[0] && Self::wrapper_name(text(name, source)) {
            facts.javascript_module_syntax = true;
        }
        let kind = match syntax_element.child_by_field_id(self.value) {
            Some(value) => self.initializer(value, source)?,
            _ => StaticBindingKind::Local,
        };
        let kind = match kind {
            StaticBindingKind::JavaScriptInstance { constructor }
                if parent
                    .child_by_field_id(self.loop_kind)
                    .is_some_and(|kind| kind.kind() == "const" && !kind.is_missing()) =>
            {
                StaticBindingKind::JavaScriptConstInstance { constructor }
            }
            _ => kind,
        };
        if let Some(call) = syntax_element
            .child_by_field_id(self.value)
            .filter(|value| value.kind_id() == self.call)
        {
            // The name takes the whole result of the call.
            facts
                .destructured
                .push((byte_range(name)?, None, byte_range(call)?));
        }
        facts
            .bindings
            .push(binding(name, scope, byte_range(syntax_element)?.end, kind)?);
        if let Some(function) = syntax_element
            .child_by_field_id(self.value)
            .and_then(|value| self.function_value(value))
        {
            let name = byte_range(name)?;
            let function = byte_range(function)?;
            // Include the authored name without moving the start of the binding's availability.
            facts.bindings.push(StaticBinding {
                name,
                scope: ByteRange {
                    start: name.start,
                    end: function.end,
                },
                available_from: function.start,
                kind: StaticBindingKind::Local,
            });
        }
        Ok(())
    }

    /// Binds the names an object pattern takes from `require` to the exports their keys
    /// name, as the member form `require('./lib').name` does. Answers whether it bound
    /// them: a pattern with a default, a rest or a nested entry binds no export.
    fn record_required_names(
        &self,
        pattern: Node<'_>,
        declarator: Node<'_>,
        scope: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<bool, RiftError> {
        let Some(value) = declarator
            .child_by_field_id(self.value)
            .filter(|value| value.kind_id() == self.call && pattern.kind_id() == self.patterns[0])
        else {
            return Ok(false);
        };
        let StaticBindingKind::JavaScriptRequire {
            require,
            module,
            export: None,
        } = self.initializer(value, source)?
        else {
            return Ok(false);
        };
        let mut names = Vec::new();
        let mut cursor = pattern.walk();
        for entry in pattern
            .named_children(&mut cursor)
            .filter(|entry| !entry.is_extra())
        {
            let (name, key) = if entry.kind_id() == self.shorthand_pattern {
                (Some(entry), Some(entry))
            } else if entry.kind_id() == self.pair_pattern {
                (entry.child_by_field_id(self.value), entry.named_child(0))
            } else {
                (None, None)
            };
            let Some((name, key)) = name
                .filter(|name| name.kind_id() == self.identifier || name == &entry)
                .zip(key.filter(|key| {
                    [self.property_identifier, self.shorthand_pattern].contains(&key.kind_id())
                }))
            else {
                return Ok(false);
            };
            names.push((name, byte_range(key)?));
        }
        let available = byte_range(declarator)?.end;
        for (name, key) in names {
            facts.bindings.push(binding(
                name,
                scope,
                available,
                StaticBindingKind::JavaScriptRequire {
                    require,
                    module,
                    export: Some(key),
                },
            )?);
        }
        Ok(true)
    }

    /// Records the names an object pattern takes from a call, each with its key.
    fn record_destructured(
        &self,
        pattern: Node<'_>,
        declarator: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let call = declarator
            .child_by_field_id(self.value)
            .filter(|value| value.kind_id() == self.call);
        let Some(call) = call.filter(|_| pattern.kind_id() == self.patterns[0]) else {
            return Ok(());
        };
        let mut cursor = pattern.walk();
        for entry in pattern.named_children(&mut cursor) {
            let (name, key) = if entry.kind_id() == self.shorthand_pattern {
                (Some(entry), Some(entry))
            } else if entry.kind_id() == self.pair_pattern {
                (
                    entry.child_by_field_id(self.value),
                    entry.child_by_field_id(shapes.key),
                )
            } else {
                (None, None)
            };
            if let Some((name, key)) = name
                .filter(|name| name.kind_id() == self.identifier || name == &entry)
                .zip(key.filter(|key| {
                    [self.property_identifier, self.shorthand_pattern].contains(&key.kind_id())
                }))
            {
                facts.destructured.push((
                    byte_range(name)?,
                    Some(byte_range(key)?),
                    byte_range(call)?,
                ));
            }
        }
        Ok(())
    }

    fn function_value<'tree>(&self, mut value: Node<'tree>) -> Option<Node<'tree>> {
        // Each step descends within the admitted syntax depth.
        while value.kind_id() == self.parenthesized {
            value = value.named_child(0)?;
        }
        (value.kind_id() == self.arrow || self.named_expression[..2].contains(&value.kind_id()))
            .then_some(value)
    }

    fn initializer(&self, value: Node<'_>, source: &str) -> Result<StaticBindingKind, RiftError> {
        if value.kind_id() == self.new
            && let Some(constructor) = value
                .child_by_field_id(self.constructor)
                .filter(|constructor| self.receiver(*constructor))
        {
            return Ok(StaticBindingKind::JavaScriptInstance {
                constructor: byte_range(constructor)?,
            });
        }
        if let Some((call, require, export)) = self.require_shape(value)
            && text(require, source) == Some("require")
            && let Some(arguments) = call.child_by_field_id(self.arguments)
            && arguments.named_child_count() == 1
            && let Some(module) = arguments
                .named_child(0)
                .map(|syntax_element| self.literal(syntax_element, source))
                .transpose()?
                .flatten()
        {
            return Ok(StaticBindingKind::JavaScriptRequire {
                require: byte_range(require)?,
                module,
                export: export.map(byte_range).transpose()?,
            });
        }
        Ok(StaticBindingKind::Local)
    }

    /// The call holding a `require` shape, its callee, and the member read from it.
    fn require_shape<'tree>(
        &self,
        syntax_element: Node<'tree>,
    ) -> Option<(Node<'tree>, Node<'tree>, Option<Node<'tree>>)> {
        if syntax_element.kind_id() == self.call {
            return Some((
                syntax_element,
                syntax_element.child_by_field_id(self.callee)?,
                None,
            ));
        }
        if syntax_element.kind_id() != self.member {
            return None;
        }
        let object = syntax_element.child_by_field_id(self.object)?;
        let export = syntax_element
            .child_by_field_id(self.property)
            .and_then(|syntax_element| identifier(syntax_element, self.property_identifier))?;
        (object.kind_id() == self.call).then_some(())?;
        Some((object, object.child_by_field_id(self.callee)?, Some(export)))
    }

    fn literal(
        &self,
        syntax_element: Node<'_>,
        source: &str,
    ) -> Result<Option<ByteRange>, RiftError> {
        let Some(value) =
            text(syntax_element, source).filter(|_| syntax_element.kind_id() == self.string)
        else {
            return Ok(None);
        };
        let bytes = value.as_bytes();
        let Some((quote, interior)) = bytes.split_first() else {
            return Ok(None);
        };
        let Some((end, interior)) = interior.split_last() else {
            return Ok(None);
        };
        if quote != end
            || !matches!(quote, b'\'' | b'"')
            || interior.is_empty()
            || interior.contains(&b'\\')
        {
            return Ok(None);
        }
        let range = byte_range(syntax_element)?;
        Ok(Some(ByteRange {
            start: range.start + 1,
            end: range.end - 1,
        }))
    }

    fn is_default_import(&self, visited: Visited<'_, '_>) -> bool {
        visited.syntax_element().kind_id() == self.identifier
            && visited
                .parent()
                .is_some_and(|parent| parent.syntax_element().kind_id() == self.import_clause)
    }

    fn record_import(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(statement) = enclosing(visited, &[self.import]) else {
            return Ok(());
        };
        let Some(scope) = enclosing(statement, &self.scopes[..1]).map(Visited::syntax_element)
        else {
            return Ok(());
        };
        let statement = statement.syntax_element();
        let export = syntax_element.child_by_field_id(self.name);
        let local = if self.is_default_import(visited) {
            Some(syntax_element)
        } else {
            syntax_element.child_by_field_id(self.alias).or(export)
        };
        let Some(name) =
            local.and_then(|syntax_element| identifier(syntax_element, self.identifier))
        else {
            return Ok(());
        };
        let module = statement
            .child_by_field_id(self.source)
            .map(|syntax_element| self.literal(syntax_element, source))
            .transpose()?
            .flatten();
        let kind = match module {
            Some(module) => StaticBindingKind::JavaScriptImport {
                module,
                export: export.map(byte_range).transpose()?,
            },
            None => StaticBindingKind::Shadow,
        };
        facts
            .bindings
            .push(binding(name, scope, byte_range(scope)?.start, kind)?);
        Ok(())
    }

    fn record_namespace_import(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let scope = enclosing(visited, &self.scopes[..1]).map(Visited::syntax_element);
        let name = syntax_element
            .named_child(0)
            .and_then(|syntax_element| identifier(syntax_element, self.identifier));
        let statement = enclosing(visited, &[self.import]).map(Visited::syntax_element);
        let module = statement
            .and_then(|statement| statement.child_by_field_id(self.source))
            .map(|syntax_element| self.literal(syntax_element, source))
            .transpose()?
            .flatten();
        if let Some(((name, scope), module)) = name.zip(scope).zip(module) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::JavaScriptEdgeNamespace { module },
            )?);
        }
        Ok(())
    }

    fn record_reexport(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(statement) = enclosing(visited, &[self.export]).map(Visited::syntax_element)
        else {
            return Ok(());
        };
        let Some(module) = statement
            .child_by_field_id(self.source)
            .map(|syntax_element| self.literal(syntax_element, source))
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        let Some(export) = syntax_element
            .child_by_field_id(self.name)
            .and_then(|syntax_element| identifier(syntax_element, self.identifier))
        else {
            return Ok(());
        };
        let name = syntax_element
            .child_by_field_id(self.alias)
            .unwrap_or(export);
        facts.reexports.push(StaticReexport {
            name: byte_range(name)?,
            module,
            export: byte_range(export)?,
        });
        Ok(())
    }

    fn record_shadow_or_mutation(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        if self.assignment.contains(&id) {
            self.record_mutation(syntax_element, self.left, facts)?;
            // A name or a member given a function written in place stays callable, and a
            // member given an object literal written in place is declared by it (D38).
            if id == self.assignment[0]
                && let Some(left) = syntax_element
                    .child_by_field_id(self.left)
                    .filter(|left| [self.identifier, self.member].contains(&left.kind_id()))
                && let Some(value) = syntax_element.child_by_field_id(self.right)
            {
                if self.function_value(value).is_some()
                    || (left.kind_id() == self.member && value.kind_id() == self.object_literal[0])
                {
                    facts.function_writes.push(byte_range(left)?);
                }
                self.record_member_copy(syntax_element, left, value, source, facts)?;
            }
        } else if id == self.update {
            self.record_mutation(syntax_element, self.argument, facts)?;
        } else if id == self.for_in {
            if syntax_element.child_by_field_id(self.loop_kind).is_none() {
                self.record_mutation(syntax_element, self.left, facts)?;
            } else if let Some(left) = syntax_element.child_by_field_id(self.left) {
                self.record_parameter(left, syntax_element, facts)?;
            }
        } else if id == self.parameters {
            self.record_parameters(visited, facts)?;
        } else if id == self.with {
            facts.unsupported_bindings.push(byte_range(syntax_element)?);
        } else if id == self.catch
            || self.named_expression.contains(&id)
            || self.function_scopes.last() == Some(&id)
        {
            self.record_named_shadow(syntax_element, facts)?;
        } else if id == self.arrow
            && let Some(parameter) = syntax_element.child_by_field_id(self.parameter)
        {
            self.record_parameter(parameter, syntax_element, facts)?;
            if parameter.kind_id() == self.identifier {
                facts.parameters.push(StaticParameter {
                    function: byte_range(syntax_element)?,
                    owner: self.function_owner(syntax_element)?,
                    index: 0,
                    name: byte_range(parameter)?,
                });
            }
        }
        Ok(())
    }

    fn record_member_copy(
        &self,
        assignment: Node<'_>,
        target: Node<'_>,
        value: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let direct_member = |expression: Node<'_>, receiver| {
            expression.kind_id() == self.member
                && expression.child_by_field_name("optional_chain").is_none()
                && expression
                    .child_by_field_id(self.object)
                    .is_some_and(|object| object.kind_id() == receiver)
        };
        let property = |expression: Node<'_>| {
            expression
                .child_by_field_id(self.property)
                .and_then(|name| text(name, source))
        };
        if direct_member(target, self.identifier)
            && direct_member(value, self.receivers[0])
            && !assignment.has_error()
            && property(target).is_some_and(|name| property(value) == Some(name))
        {
            facts.member_copies.push(StaticMemberCopy {
                assignment: byte_range(assignment)?,
                target: byte_range(target)?,
                value: byte_range(value)?,
            });
        }
        Ok(())
    }

    fn record_mutation(
        &self,
        syntax_element: Node<'_>,
        target: u16,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let mut pending = syntax_element
            .child_by_field_id(target)
            .into_iter()
            .collect::<Vec<_>>();
        // The syntax walk's element limit bounds each assignment subtree.
        while let Some(target) = pending.pop() {
            let id = target.kind_id();
            if [
                self.identifier,
                self.shorthand_pattern,
                self.member,
                self.subscript,
            ]
            .contains(&id)
            {
                facts.assignments.push(byte_range(target)?);
            } else if self.patterns.contains(&id) {
                let mut cursor = target.walk();
                pending.extend(target.named_children(&mut cursor));
            } else {
                let field = if id == self.pair_pattern {
                    Some(self.value)
                } else if self.default_patterns.contains(&id) {
                    Some(self.left)
                } else {
                    None
                };
                if let Some(child) = field.and_then(|field| target.child_by_field_id(field)) {
                    pending.push(child);
                }
            }
        }
        Ok(())
    }

    fn record_parameters(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let owner = self.function_owner(scope)?;
        let mut cursor = syntax_element.walk();
        for (index, parameter) in syntax_element
            .named_children(&mut cursor)
            .filter(|syntax_element| !syntax_element.is_extra())
            .enumerate()
        {
            self.record_parameter(parameter, scope, facts)?;
            let name = if self.parameter_wrappers.contains(&parameter.kind_id()) {
                self.pattern
                    .and_then(|field| parameter.child_by_field_id(field))
            } else {
                Some(parameter)
            };
            if let Some((name, index)) = name
                .filter(|name| name.kind_id() == self.identifier)
                .zip(u32::try_from(index).ok())
            {
                facts.parameters.push(StaticParameter {
                    function: byte_range(scope)?,
                    owner,
                    index,
                    name: byte_range(name)?,
                });
            }
        }
        Ok(())
    }

    /// Name of the declaration a function belongs to: its own name, or the name of the
    /// declaration a function value initializes.
    fn function_owner(&self, function: Node<'_>) -> Result<Option<ByteRange>, RiftError> {
        let value = function.kind_id() == self.arrow
            || self.named_expression[..2].contains(&function.kind_id());
        if !value {
            return function
                .child_by_field_id(self.name)
                .map(byte_range)
                .transpose();
        }
        function
            .parent()
            .filter(|parent| parent.child_by_field_id(self.value) == Some(function))
            .and_then(|parent| parent.child_by_field_id(self.name))
            .map(byte_range)
            .transpose()
    }

    fn record_parameter(
        &self,
        parameter: Node<'_>,
        scope: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        // Each parameter subtree is bounded by the enclosing syntax walk's element limit.
        let mut pending = vec![parameter];
        while let Some(parameter) = pending.pop() {
            let id = parameter.kind_id();
            if [self.identifier, self.shorthand_pattern].contains(&id) {
                facts.bindings.push(binding(
                    parameter,
                    scope,
                    byte_range(scope)?.start,
                    StaticBindingKind::Shadow,
                )?);
            } else if self.patterns.contains(&id) {
                let mut cursor = parameter.walk();
                pending.extend(parameter.named_children(&mut cursor));
            } else {
                let field = if id == self.pair_pattern {
                    Some(self.value)
                } else if self.default_patterns.contains(&id) {
                    Some(self.left)
                } else if self.parameter_wrappers.contains(&id) {
                    self.pattern
                } else {
                    None
                };
                if let Some(child) = field.and_then(|field| parameter.child_by_field_id(field)) {
                    pending.push(child);
                }
            }
        }
        Ok(())
    }

    fn record_named_shadow(
        &self,
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let field = if syntax_element.kind_id() == self.catch {
            self.parameter
        } else {
            self.name
        };
        if let Some(name) = syntax_element.child_by_field_id(field) {
            if self.named_expression[..2].contains(&syntax_element.kind_id()) {
                facts.bindings.push(binding(
                    name,
                    syntax_element,
                    byte_range(syntax_element)?.start,
                    StaticBindingKind::Local,
                )?);
            } else {
                self.record_parameter(name, syntax_element, facts)?;
            }
        }
        Ok(())
    }
}

fn text<'a>(syntax_element: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(syntax_element.byte_range())
}
