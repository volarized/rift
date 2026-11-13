use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, StaticReexport, binding, call, enclosing,
    field, identifier, kind,
};
use crate::ByteRange;
use crate::extract::byte_range;

pub(super) struct Kinds {
    identifier: u16,
    property_identifier: u16,
    call: u16,
    member: u16,
    new: u16,
    string: u16,
    variable: u16,
    var: u16,
    assignment: [u16; 2],
    update: u16,
    function: [u16; 2],
    named_expression: [u16; 3],
    class: u16,
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
    KINDS.get_or_init(|| {
        let grammar = crate::javascript::javascript_grammar();
        let id = |name| kind(&grammar, name);
        let field = |name| field(&grammar, name);
        let program = id("program");
        let function = [
            id("function_declaration"),
            id("generator_function_declaration"),
        ];
        let named_expression = [
            id("function_expression"),
            id("generator_function"),
            id("class"),
        ];
        let arrow = id("arrow_function");
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
            call: id("call_expression"),
            member: id("member_expression"),
            new: id("new_expression"),
            string: id("string"),
            variable: id("variable_declarator"),
            var: id("variable_declaration"),
            assignment: [
                id("assignment_expression"),
                id("augmented_assignment_expression"),
            ],
            update: id("update_expression"),
            function,
            named_expression,
            class: id("class_declaration"),
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
    })
}

impl Kinds {
    pub(super) fn record(
        &self,
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = node.kind_id();
        if id == self.call {
            self.record_call(node, source, facts)?;
        } else if id == self.variable {
            self.record_variable(node, source, facts)?;
        } else if self.function.contains(&id) || id == self.class {
            self.record_declaration(node, facts)?;
        } else if id == self.import_specifier || self.is_default_import(node) {
            self.record_import(node, source, facts)?;
        } else if id == self.namespace_import {
            self.record_namespace_import(node, facts)?;
        } else if id == self.export_specifier {
            self.record_reexport(node, source, facts)?;
        } else {
            self.record_shadow_or_mutation(node, facts)?;
        }
        Ok(())
    }

    fn record_call(
        &self,
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(callee) = node
            .child_by_field_id(self.callee)
            .filter(|node| !node.is_missing())
        else {
            return Ok(());
        };
        let shape = if callee.kind_id() == self.identifier {
            if text(callee, source) == Some("eval") {
                let root = enclosing(node, &self.scopes[..1]).unwrap_or(node);
                facts.unsupported_bindings.push(byte_range(root)?);
            }
            StaticCallee::Direct
        } else {
            self.member(callee)?
        };
        facts.calls.push(call(node, callee, shape)?);
        Ok(())
    }

    fn member(&self, node: Node<'_>) -> Result<StaticCallee, RiftError> {
        if node.kind_id() != self.member {
            return Ok(StaticCallee::Unsupported);
        }
        let receiver = node
            .child_by_field_id(self.object)
            .and_then(|node| identifier(node, self.identifier));
        let member = node
            .child_by_field_id(self.property)
            .and_then(|node| identifier(node, self.property_identifier));
        match receiver.zip(member) {
            Some((receiver, member)) => Ok(StaticCallee::Member {
                receiver: byte_range(receiver)?,
                member: byte_range(member)?,
            }),
            None => Ok(StaticCallee::Unsupported),
        }
    }

    fn record_declaration(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = enclosing(node, &self.scopes);
        let name = node
            .child_by_field_id(self.name)
            .and_then(|node| identifier(node, self.identifier));
        if let Some((name, scope)) = name.zip(scope) {
            let available = if node.kind_id() == self.class {
                byte_range(node)?.end
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
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(parent) = node.parent() else {
            return Ok(());
        };
        let is_var = parent.kind_id() == self.var;
        let scopes = if is_var {
            &self.function_scopes
        } else {
            &self.scopes
        };
        let Some(scope) = enclosing(node, scopes) else {
            return Ok(());
        };
        let Some(name) = node
            .child_by_field_id(self.name)
            .and_then(|node| identifier(node, self.identifier))
        else {
            facts.unsupported_bindings.push(byte_range(scope)?);
            return Ok(());
        };
        let kind = match node.child_by_field_id(self.value) {
            Some(value) if !is_var => self.initializer(value, source)?,
            _ => StaticBindingKind::Shadow,
        };
        facts
            .bindings
            .push(binding(name, scope, byte_range(node)?.end, kind)?);
        Ok(())
    }

    fn initializer(&self, value: Node<'_>, source: &str) -> Result<StaticBindingKind, RiftError> {
        if value.kind_id() == self.new
            && let Some(constructor) = value
                .child_by_field_id(self.constructor)
                .and_then(|node| identifier(node, self.identifier))
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
                .map(|node| self.literal(node, source))
                .transpose()?
                .flatten()
        {
            return Ok(StaticBindingKind::JavaScriptRequire {
                require: byte_range(require)?,
                module,
                export: export.map(byte_range).transpose()?,
            });
        }
        Ok(StaticBindingKind::Shadow)
    }

    fn require_shape<'tree>(
        &self,
        node: Node<'tree>,
    ) -> Option<(Node<'tree>, Option<Node<'tree>>)> {
        if node.kind_id() == self.call {
            return Some((node.child_by_field_id(self.callee)?, None));
        }
        if node.kind_id() != self.member {
            return None;
        }
        let object = node.child_by_field_id(self.object)?;
        let export = node
            .child_by_field_id(self.property)
            .and_then(|node| identifier(node, self.property_identifier))?;
        (object.kind_id() == self.call).then_some(())?;
        Some((object.child_by_field_id(self.callee)?, Some(export)))
    }

    fn literal(&self, node: Node<'_>, source: &str) -> Result<Option<ByteRange>, RiftError> {
        let Some(value) = text(node, source).filter(|_| node.kind_id() == self.string) else {
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
        let range = byte_range(node)?;
        Ok(Some(ByteRange {
            start: range.start + 1,
            end: range.end - 1,
        }))
    }

    fn is_default_import(&self, node: Node<'_>) -> bool {
        node.kind_id() == self.identifier
            && node
                .parent()
                .is_some_and(|parent| parent.kind_id() == self.import_clause)
    }

    fn record_import(
        &self,
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(statement) = enclosing(node, &[self.import]) else {
            return Ok(());
        };
        let Some(scope) = enclosing(statement, &self.scopes[..1]) else {
            return Ok(());
        };
        let export = node.child_by_field_id(self.name);
        let local = if self.is_default_import(node) {
            Some(node)
        } else {
            node.child_by_field_id(self.alias).or(export)
        };
        let Some(name) = local.and_then(|node| identifier(node, self.identifier)) else {
            return Ok(());
        };
        let module = statement
            .child_by_field_id(self.source)
            .map(|node| self.literal(node, source))
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
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = enclosing(node, &self.scopes[..1]);
        let name = node
            .named_child(0)
            .and_then(|node| identifier(node, self.identifier));
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

    fn record_reexport(
        &self,
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(statement) = enclosing(node, &[self.export]) else {
            return Ok(());
        };
        let Some(module) = statement
            .child_by_field_id(self.source)
            .map(|node| self.literal(node, source))
            .transpose()?
            .flatten()
        else {
            return Ok(());
        };
        let Some(export) = node
            .child_by_field_id(self.name)
            .and_then(|node| identifier(node, self.identifier))
        else {
            return Ok(());
        };
        let name = node.child_by_field_id(self.alias).unwrap_or(export);
        facts.reexports.push(StaticReexport {
            name: byte_range(name)?,
            module,
            export: byte_range(export)?,
        });
        Ok(())
    }

    fn record_shadow_or_mutation(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = node.kind_id();
        if self.assignment.contains(&id) {
            self.record_mutation(node, self.left, facts)?;
        } else if id == self.update {
            self.record_mutation(node, self.argument, facts)?;
        } else if id == self.parameters {
            self.record_parameters(node, facts)?;
        } else if id == self.with {
            facts.unsupported_bindings.push(byte_range(node)?);
        } else if id == self.catch
            || self.named_expression.contains(&id)
            || self.function_scopes.last() == Some(&id)
        {
            self.record_named_shadow(node, facts)?;
        } else if id == self.arrow
            && let Some(parameter) = node.child_by_field_id(self.parameter)
        {
            self.record_parameter(parameter, node, facts)?;
        }
        Ok(())
    }

    fn record_mutation(
        &self,
        node: Node<'_>,
        target: u16,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(target) = node
            .child_by_field_id(target)
            .filter(|node| !node.is_missing())
        {
            if target.kind_id() == self.identifier || target.kind_id() == self.member {
                facts.assignments.push(byte_range(target)?);
            } else if let Some(root) = enclosing(node, &self.scopes[..1]) {
                facts.unsupported_bindings.push(byte_range(root)?);
            }
        }
        Ok(())
    }

    fn record_parameters(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(scope) = node.parent() else {
            return Ok(());
        };
        let mut cursor = node.walk();
        for parameter in node
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra())
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
        if let Some(name) = identifier(parameter, self.identifier) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Shadow,
            )?);
        } else if !parameter.is_missing() {
            facts.unsupported_bindings.push(byte_range(scope)?);
        }
        Ok(())
    }

    fn record_named_shadow(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let field = if node.kind_id() == self.catch {
            self.parameter
        } else {
            self.name
        };
        if let Some(name) = node.child_by_field_id(field) {
            self.record_parameter(name, node, facts)?;
        }
        Ok(())
    }
}

fn text<'a>(node: Node<'_>, source: &'a str) -> Option<&'a str> {
    source.get(node.byte_range())
}
