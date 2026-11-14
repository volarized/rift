use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, StaticInherentImpl, binding, call,
    enclosing, field, identifier, kind,
};
use crate::extract::byte_range;

pub(super) struct Kinds {
    identifier: u16,
    field_identifier: u16,
    type_identifier: u16,
    scoped_identifier: u16,
    scoped_type_identifier: u16,
    call: u16,
    generic_function: u16,
    field_expression: u16,
    struct_expression: u16,
    use_declaration: u16,
    use_alias: u16,
    use_list: [u16; 3],
    let_declaration: u16,
    parameter: u16,
    closure_parameters: u16,
    parameters: u16,
    conditional: [u16; 4],
    attribute: u16,
    inner_attribute: u16,
    impl_item: u16,
    assignment: [u16; 2],
    scopes: [u16; 3],
    functions: [u16; 3],
    function: u16,
    value: u16,
    member: u16,
    argument: u16,
    name: u16,
    alias: u16,
    path: u16,
    pattern: u16,
    value_type: u16,
    left: u16,
    trait_field: u16,
}

pub(super) fn kinds() -> &'static Kinds {
    static KINDS: OnceLock<Kinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = crate::rust::rust_grammar();
        let id = |name| kind(&grammar, name);
        let field = |name| field(&grammar, name);
        Kinds {
            identifier: id("identifier"),
            field_identifier: id("field_identifier"),
            type_identifier: id("type_identifier"),
            scoped_identifier: id("scoped_identifier"),
            scoped_type_identifier: id("scoped_type_identifier"),
            call: id("call_expression"),
            generic_function: id("generic_function"),
            field_expression: id("field_expression"),
            struct_expression: id("struct_expression"),
            use_declaration: id("use_declaration"),
            use_alias: id("use_as_clause"),
            use_list: [id("use_list"), id("scoped_use_list"), id("use_wildcard")],
            let_declaration: id("let_declaration"),
            parameter: id("parameter"),
            closure_parameters: id("closure_parameters"),
            parameters: id("parameters"),
            conditional: [
                id("let_condition"),
                id("match_arm"),
                id("for_expression"),
                id("if_expression"),
            ],
            attribute: id("attribute_item"),
            inner_attribute: id("inner_attribute_item"),
            impl_item: id("impl_item"),
            assignment: [id("assignment_expression"), id("compound_assignment_expr")],
            scopes: [id("source_file"), id("block"), id("declaration_list")],
            functions: [
                id("function_item"),
                id("function_signature_item"),
                id("closure_expression"),
            ],
            function: field("function"),
            value: field("value"),
            member: field("field"),
            argument: field("argument"),
            name: field("name"),
            alias: field("alias"),
            path: field("path"),
            pattern: field("pattern"),
            value_type: field("type"),
            left: field("left"),
            trait_field: field("trait"),
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
            self.record_call(node, facts)?;
        } else if id == self.use_declaration {
            self.record_import(node, facts)?;
        } else if id == self.let_declaration || id == self.parameter {
            self.record_value(node, facts)?;
        } else if id == self.identifier
            && node
                .parent()
                .is_some_and(|parent| parent.kind_id() == self.closure_parameters)
        {
            self.record_closure_argument(node, facts)?;
        } else if id == self.functions[0] {
            self.record_function(node, facts)?;
        } else if self.assignment.contains(&id) {
            self.record_mutation(node, facts)?;
        } else if id == self.impl_item {
            self.record_impl(node, facts)?;
        } else if id == self.attribute || id == self.inner_attribute {
            self.record_attribute(node, source, facts)?;
        } else if self.conditional[..3].contains(&id) {
            let scope = if id == self.conditional[0] {
                enclosing(node, &self.conditional[3..]).unwrap_or(node)
            } else {
                node
            };
            facts.unsupported_bindings.push(byte_range(scope)?);
        } else if id == self.parameters || id == self.closure_parameters {
            self.record_patterns(node, facts)?;
        }
        Ok(())
    }

    fn record_attribute(
        &self,
        node: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = node
            .named_child(0)
            .and_then(|attribute| attribute.named_child(0));
        let name = name.and_then(|name| source.get(name.byte_range()));
        if !matches!(name, Some("cfg" | "cfg_attr")) {
            return Ok(());
        }
        let conditional = if node.kind_id() == self.inner_attribute {
            node.parent()
        } else {
            // Consecutive attributes are source-ordered siblings. A cfg at the end of
            // a list owns the next item only when that item exists.
            let mut next = node.next_named_sibling();
            while let Some(sibling) = next {
                if sibling.kind_id() != self.attribute && !sibling.is_extra() {
                    break;
                }
                next = sibling.next_named_sibling();
            }
            next
        };
        if let Some(conditional) = conditional {
            facts.conditional_items.push(byte_range(conditional)?);
        }
        Ok(())
    }

    fn record_call(&self, node: Node<'_>, facts: &mut StaticFactsParts) -> Result<(), RiftError> {
        let Some(expression) = node
            .child_by_field_id(self.function)
            .filter(|node| !node.is_missing())
        else {
            return Ok(());
        };
        let callee = if expression.kind_id() == self.generic_function {
            expression
                .child_by_field_id(self.function)
                .unwrap_or(expression)
        } else {
            expression
        };
        let shape = if callee.kind_id() == self.identifier {
            StaticCallee::Direct
        } else if callee.kind_id() == self.scoped_identifier {
            StaticCallee::Qualified
        } else {
            self.member(callee)?
        };
        facts.calls.push(call(node, callee, shape)?);
        Ok(())
    }

    fn member(&self, node: Node<'_>) -> Result<StaticCallee, RiftError> {
        if node.kind_id() != self.field_expression {
            return Ok(StaticCallee::Unsupported);
        }
        let receiver = node
            .child_by_field_id(self.value)
            .and_then(|node| identifier(node, self.identifier));
        let member = node
            .child_by_field_id(self.member)
            .and_then(|node| identifier(node, self.field_identifier));
        match receiver.zip(member) {
            Some((receiver, member)) => Ok(StaticCallee::Member {
                receiver: byte_range(receiver)?,
                member: byte_range(member)?,
            }),
            None => Ok(StaticCallee::Unsupported),
        }
    }

    fn record_import(&self, node: Node<'_>, facts: &mut StaticFactsParts) -> Result<(), RiftError> {
        let Some(scope) = enclosing(node, &self.scopes) else {
            return Ok(());
        };
        let Some(argument) = node
            .child_by_field_id(self.argument)
            .filter(|node| !node.is_missing())
        else {
            return Ok(());
        };
        let (path, name) = if argument.kind_id() == self.use_alias {
            (
                argument.child_by_field_id(self.path),
                argument.child_by_field_id(self.alias),
            )
        } else if argument.kind_id() == self.scoped_identifier {
            (Some(argument), argument.child_by_field_id(self.name))
        } else if argument.kind_id() == self.identifier {
            (Some(argument), Some(argument))
        } else {
            if self.use_list.contains(&argument.kind_id()) {
                facts.unsupported_bindings.push(byte_range(scope)?);
            }
            return Ok(());
        };
        if let Some((path, name)) = path
            .zip(name)
            .filter(|(path, name)| !path.is_missing() && !name.is_missing())
        {
            let kind = StaticBindingKind::RustUse {
                path: byte_range(path)?,
            };
            facts
                .bindings
                .push(binding(name, scope, byte_range(scope)?.start, kind)?);
        }
        Ok(())
    }

    fn record_value(&self, node: Node<'_>, facts: &mut StaticFactsParts) -> Result<(), RiftError> {
        let is_let = node.kind_id() == self.let_declaration;
        let scopes = if is_let {
            &self.scopes
        } else {
            &self.functions
        };
        let Some(scope) = enclosing(node, scopes) else {
            return Ok(());
        };
        let Some(name) = node
            .child_by_field_id(self.pattern)
            .and_then(|node| identifier(node, self.identifier))
        else {
            facts.unsupported_bindings.push(byte_range(scope)?);
            return Ok(());
        };
        let value_type = node
            .child_by_field_id(self.value_type)
            .or_else(|| self.struct_value(node));
        let kind = match value_type.filter(|node| self.is_type(*node)) {
            Some(value_type) => StaticBindingKind::RustReceiver {
                type_path: byte_range(value_type)?,
            },
            None => StaticBindingKind::Shadow,
        };
        let available_from = if is_let {
            byte_range(node)?.end
        } else {
            byte_range(scope)?.start
        };
        facts
            .bindings
            .push(binding(name, scope, available_from, kind)?);
        Ok(())
    }

    fn record_impl(&self, node: Node<'_>, facts: &mut StaticFactsParts) -> Result<(), RiftError> {
        if node.child_by_field_id(self.trait_field).is_none()
            && let Some(value_type) = node
                .child_by_field_id(self.value_type)
                .filter(|node| self.is_type(*node))
        {
            facts.inherent_impls.push(StaticInherentImpl {
                range: byte_range(node)?,
                type_path: byte_range(value_type)?,
            });
        }
        Ok(())
    }

    fn is_type(&self, node: Node<'_>) -> bool {
        !node.is_missing()
            && [self.type_identifier, self.scoped_type_identifier].contains(&node.kind_id())
    }

    fn struct_value<'tree>(&self, node: Node<'tree>) -> Option<Node<'tree>> {
        let value = node.child_by_field_id(self.value)?;
        (value.kind_id() == self.struct_expression).then_some(())?;
        value.child_by_field_id(self.name)
    }

    fn record_closure_argument(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(scope) = enclosing(node, &self.functions) {
            facts.bindings.push(binding(
                node,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Shadow,
            )?);
        }
        Ok(())
    }

    fn record_function(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = node
            .child_by_field_id(self.name)
            .and_then(|node| identifier(node, self.identifier));
        if let Some((name, scope)) = name.zip(enclosing(node, &self.scopes)) {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Local,
            )?);
        }
        Ok(())
    }

    fn record_mutation(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(target) = node
            .child_by_field_id(self.left)
            .filter(|node| !node.is_missing())
        {
            facts.assignments.push(byte_range(target)?);
        }
        Ok(())
    }

    fn record_patterns(
        &self,
        node: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(scope) = enclosing(node, &self.functions) else {
            return Ok(());
        };
        let mut cursor = node.walk();
        let has_unparsed_pattern = node.named_children(&mut cursor).any(|child| {
            !child.is_extra()
                && child.kind_id() != self.parameter
                && child.kind_id() != self.identifier
                && !child.is_missing()
        });
        if node.kind_id() == self.closure_parameters && has_unparsed_pattern {
            facts.unsupported_bindings.push(byte_range(scope)?);
        }
        Ok(())
    }
}
