use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, StaticReexport, StaticReference, binding,
    call, enclosing, field, identifier, kind,
};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};

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
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        self.record_reference(visited, facts)?;
        self.record_declared_type(syntax_element, facts)?;
        self.record_function(visited, facts)?;
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
            self.record_declaration(visited, facts)?;
        } else if id == self.import_specifier || self.is_default_import(visited) {
            self.record_import(visited, source, facts)?;
        } else if id == self.namespace_import {
            self.record_namespace_import(visited, source, facts)?;
        } else if id == self.export_specifier {
            self.record_reexport(visited, source, facts)?;
        } else {
            self.record_shadow_or_mutation(visited, facts)?;
        }
        Ok(())
    }

    fn record_function(
        &self,
        visited: Visited<'_, '_>,
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
        Ok(())
    }

    fn record_type_parameter(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = visited.syntax_element().child_by_field_id(self.name);
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
        let receiver = syntax_element
            .child_by_field_id(self.object)
            .filter(|receiver| self.receiver(*receiver));
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
        if self.generic == Some(annotation.kind_id()) {
            let Some(inner) = annotation.child_by_field_id(self.name) else {
                return Ok(());
            };
            annotation = inner;
        }
        if name.is_missing() || annotation.is_missing() {
            return Ok(());
        }
        if (self.type_identifier == Some(annotation.kind_id())
            || self.nested_type == Some(annotation.kind_id()))
            && ([self.identifier, self.property_identifier].contains(&name.kind_id())
                || self.type_identifier == Some(name.kind_id()))
        {
            facts
                .declared_types
                .push((byte_range(name)?, byte_range(annotation)?));
        }
        Ok(())
    }

    fn record_declaration(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let scope = enclosing(visited, &self.scopes).map(Visited::syntax_element);
        let name = syntax_element
            .child_by_field_id(self.name)
            .filter(|syntax_element| !syntax_element.is_missing());
        if let Some((name, scope)) = name.zip(scope) {
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
            return self.record_parameter(name, scope, facts);
        }
        let kind = match syntax_element.child_by_field_id(self.value) {
            Some(value) if !is_var => self.initializer(value, source)?,
            _ => StaticBindingKind::Local,
        };
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
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
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
        let mut cursor = syntax_element.walk();
        for parameter in syntax_element
            .named_children(&mut cursor)
            .filter(|syntax_element| !syntax_element.is_extra())
        {
            self.record_parameter(parameter, scope, facts)?;
        }
        Ok(())
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
            self.record_parameter(name, syntax_element, facts)?;
        }
        Ok(())
    }
}

fn text<'a>(syntax_element: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(syntax_element.byte_range())
}
