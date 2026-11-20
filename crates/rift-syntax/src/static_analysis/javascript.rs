use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticDeclaredReturn, StaticFactsParts, StaticFunctionContext,
    StaticMember, StaticParameter, StaticReexport, StaticReference, binding, call, enclosing,
    field, identifier, kind,
};
use crate::ByteRange;
use crate::extract::byte_range;

mod documentation;

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
    generic: Option<u16>,
    type_alias: Option<u16>,
    type_field: Option<u16>,
    parameter_wrappers: Vec<u16>,
    patterns: [u16; 3],
    shorthand_pattern: u16,
    pair_pattern: u16,
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
    /// A method with a body, a `return` statement and a call's argument list.
    method: u16,
    return_statement: u16,
    argument_list: u16,
    /// Field holding an arrow function's or a function's body.
    body: u16,
    /// Fields holding a type parameter's constraint and a pattern pair's key.
    constraint: u16,
    key: u16,
    /// A member of an object type with a name and a type.
    property_signature: u16,
    /// A union of written types.
    union: u16,
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
            argument_list: id("arguments"),
            body: field(grammar, "body"),
            constraint: field(grammar, "constraint"),
            key: field(grammar, "key"),
            property_signature: id("property_signature"),
            union: id("union_type"),
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
            scopes: [
                program,
                id("statement_block"),
                id("switch_body"),
                id("for_statement"),
                id("for_in_statement"),
                id("catch_clause"),
                id("class_static_block"),
                id("class_body"),
            ],
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
            argument: field("argument"),
            parameter: field("parameter"),
        }
    }
}

impl Kinds {
    pub(super) fn record(
        &self,
        syntax_element: Node<'_>,
        source: &str,
        documentation: &mut Documentation,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = syntax_element.kind_id();
        self.record_documented_function(syntax_element, source, documentation, facts)?;
        self.record_module_syntax(syntax_element, source, facts);
        self.record_reference(syntax_element, facts)?;
        self.record_member(syntax_element, facts)?;
        self.record_declared_type(syntax_element, source, facts)?;
        self.record_type_signature(syntax_element, facts)?;
        self.record_return_type(syntax_element, source, facts)?;
        self.record_function(syntax_element, facts)?;
        if id == self.call {
            self.record_call(syntax_element, source, facts)?;
        } else if id == self.new {
            if let Some(callee) = syntax_element.child_by_field_id(self.constructor) {
                facts
                    .calls
                    .push(call(syntax_element, callee, StaticCallee::Constructor)?);
            }
        } else if id == self.variable {
            self.record_variable(syntax_element, source, facts)?;
        } else if self.type_parameter == Some(id) {
            self.record_type_parameter(syntax_element, facts)?;
        } else if self.function.contains(&id) || id == self.class || self.edge_types.contains(&id) {
            self.record_declaration(syntax_element, source, facts)?;
        } else if id == self.import_specifier || self.is_default_import(syntax_element) {
            self.record_import(syntax_element, source, facts)?;
        } else if id == self.namespace_import {
            self.record_namespace_import(syntax_element, source, facts)?;
        } else if id == self.export_specifier {
            self.record_reexport(syntax_element, source, facts)?;
        } else {
            self.record_shadow_or_mutation(syntax_element, facts)?;
        }
        Ok(())
    }

    fn record_module_syntax(&self, element: Node<'_>, source: &str, facts: &mut StaticFactsParts) {
        if facts.javascript_module_syntax {
            return;
        }
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
            && enclosing(element, &self.function_scopes)
                .is_some_and(|scope| scope.kind_id() == self.function_scopes[0])
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
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
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
        let mut value = element;
        // The syntax walk has admitted every enclosing expression's depth.
        loop {
            facts.function_values.push((range, byte_range(value)?));
            let Some(parent) = value.parent() else {
                break;
            };
            if !matches!(
                parent.kind(),
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
        Ok(())
    }

    /// The written place that can type the function value `value`: the declaration it
    /// initializes, the call it is an argument of, or the function that returns it.
    fn function_context(
        &self,
        value: Node<'_>,
    ) -> Result<Option<StaticFunctionContext>, RiftError> {
        let Some((shapes, parent)) = self.type_shapes.as_ref().zip(value.parent()) else {
            return Ok(None);
        };
        if parent.child_by_field_id(self.value) == Some(value)
            && let Some(name) = parent.child_by_field_id(self.name)
        {
            return Ok(Some(StaticFunctionContext::Initializer(byte_range(name)?)));
        }
        if parent.kind_id() == shapes.argument_list
            && let Some(call) = parent.parent()
        {
            let mut cursor = parent.walk();
            let index = parent
                .named_children(&mut cursor)
                .filter(|argument| !argument.is_extra())
                .position(|argument| argument == value);
            return Ok(match index.and_then(|index| u32::try_from(index).ok()) {
                Some(index) => Some(StaticFunctionContext::Argument {
                    call: byte_range(call)?,
                    index,
                }),
                None => None,
            });
        }
        if parent.kind_id() == self.arrow && parent.child_by_field_id(shapes.body) == Some(value) {
            return Ok(Some(StaticFunctionContext::Returned(byte_range(parent)?)));
        }
        if parent.kind_id() != shapes.return_statement {
            return Ok(None);
        }
        // The syntax walk has admitted every enclosing statement's depth.
        let mut enclosing = parent.parent();
        while let Some(function) = enclosing {
            let id = function.kind_id();
            if id == self.arrow
                || id == shapes.method
                || self.function.contains(&id)
                || self.named_expression[..2].contains(&id)
            {
                return Ok(Some(StaticFunctionContext::Returned(byte_range(function)?)));
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
        parameter: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = parameter.child_by_field_id(self.name);
        let scope = parameter
            .parent()
            .and_then(|parameters| parameters.parent());
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
    fn nullable_type<'tree>(&self, mut written: Node<'tree>, source: &str) -> Node<'tree> {
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
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(callee) = syntax_element
            .child_by_field_id(self.callee)
            .filter(|syntax_element| !syntax_element.is_missing())
        else {
            return Ok(());
        };
        let shape = if callee.kind_id() == self.identifier {
            if text(callee, source) == Some("eval") {
                let root = enclosing(syntax_element, &self.scopes[..1]).unwrap_or(syntax_element);
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
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = element.kind_id();
        let lexical = [self.identifier, self.shorthand, self.shorthand_pattern].contains(&id)
            && self.value_reference(element);
        if id == self.member || lexical {
            let access = self.access_target(element);
            let parent = access.parent();
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
            let parent = element.parent();
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

    fn value_reference(&self, element: Node<'_>) -> bool {
        if enclosing(element, &self.excluded_values).is_some() {
            return false;
        }
        let mut current = element;
        while let Some(parent) = current.parent() {
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
            current = parent;
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

    fn access_target<'tree>(&self, element: Node<'tree>) -> Node<'tree> {
        let mut current = element;
        while let Some(parent) = current.parent() {
            if !self.pattern_parent(current, parent) {
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
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = enclosing(syntax_element, &self.scopes);
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
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(parent) = syntax_element.parent() else {
            return Ok(());
        };
        let is_var = parent.kind_id() == self.var;
        let scopes = if is_var {
            &self.function_scopes
        } else {
            &self.scopes
        };
        let Some(scope) = enclosing(syntax_element, scopes) else {
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
        if let Some(scope) = syntax_element
            .child_by_field_id(self.value)
            .and_then(|value| self.function_value(value))
        {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Local,
            )?);
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
        if let Some((require, export)) = self.require_shape(value)
            && text(require, source) == Some("require")
            && let Some(arguments) = require
                .parent()
                .and_then(|call| call.child_by_field_id(self.arguments))
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

    fn require_shape<'tree>(
        &self,
        syntax_element: Node<'tree>,
    ) -> Option<(Node<'tree>, Option<Node<'tree>>)> {
        if syntax_element.kind_id() == self.call {
            return Some((syntax_element.child_by_field_id(self.callee)?, None));
        }
        if syntax_element.kind_id() != self.member {
            return None;
        }
        let object = syntax_element.child_by_field_id(self.object)?;
        let export = syntax_element
            .child_by_field_id(self.property)
            .and_then(|syntax_element| identifier(syntax_element, self.property_identifier))?;
        (object.kind_id() == self.call).then_some(())?;
        Some((object.child_by_field_id(self.callee)?, Some(export)))
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

    fn is_default_import(&self, syntax_element: Node<'_>) -> bool {
        syntax_element.kind_id() == self.identifier
            && syntax_element
                .parent()
                .is_some_and(|parent| parent.kind_id() == self.import_clause)
    }

    fn record_import(
        &self,
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(statement) = enclosing(syntax_element, &[self.import]) else {
            return Ok(());
        };
        let Some(scope) = enclosing(statement, &self.scopes[..1]) else {
            return Ok(());
        };
        let export = syntax_element.child_by_field_id(self.name);
        let local = if self.is_default_import(syntax_element) {
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
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = enclosing(syntax_element, &self.scopes[..1]);
        let name = syntax_element
            .named_child(0)
            .and_then(|syntax_element| identifier(syntax_element, self.identifier));
        let statement = enclosing(syntax_element, &[self.import]);
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
        syntax_element: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(statement) = enclosing(syntax_element, &[self.export]) else {
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
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = syntax_element.kind_id();
        if self.assignment.contains(&id) {
            self.record_mutation(syntax_element, self.left, facts)?;
        } else if id == self.update {
            self.record_mutation(syntax_element, self.argument, facts)?;
        } else if id == self.for_in {
            if syntax_element.child_by_field_id(self.loop_kind).is_none() {
                self.record_mutation(syntax_element, self.left, facts)?;
            } else if let Some(left) = syntax_element.child_by_field_id(self.left) {
                self.record_parameter(left, syntax_element, facts)?;
            }
        } else if id == self.parameters {
            self.record_parameters(syntax_element, facts)?;
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
        syntax_element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(scope) = syntax_element.parent() else {
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
