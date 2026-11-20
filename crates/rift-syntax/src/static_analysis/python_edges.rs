//! Captured Python call sites and lexical import bindings.

use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, binding, call, field, identifier, kind,
};
use crate::extract::{Visited, byte_range};

pub(super) struct EdgePythonKinds {
    identifier: u16,
    call: u16,
    attribute: u16,
    definitions: [u16; 2],
    scopes: [u16; 4],
    imports: [u16; 2],
    aliased: u16,
    parameters: [u16; 2],
    assignment: [u16; 3],
    unsupported: [u16; 7],
    name: std::num::NonZeroU16,
    alias: u16,
    module: u16,
    function: u16,
    object: u16,
    member: u16,
    left: u16,
}

pub(super) fn edge_python_kinds() -> &'static EdgePythonKinds {
    static KINDS: OnceLock<EdgePythonKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = tree_sitter_python::LANGUAGE.into();
        let id = |name| kind(&grammar, name);
        let field = |name| field(&grammar, name);
        let definitions = [id("function_definition"), id("class_definition")];
        EdgePythonKinds {
            identifier: id("identifier"),
            call: id("call"),
            attribute: id("attribute"),
            definitions,
            scopes: [id("module"), definitions[0], definitions[1], id("lambda")],
            imports: [id("import_statement"), id("import_from_statement")],
            aliased: id("aliased_import"),
            parameters: [id("parameters"), id("lambda_parameters")],
            assignment: [
                id("assignment"),
                id("augmented_assignment"),
                id("for_statement"),
            ],
            unsupported: [
                id("global_statement"),
                id("nonlocal_statement"),
                id("wildcard_import"),
                id("named_expression"),
                id("list_comprehension"),
                id("dictionary_comprehension"),
                id("set_comprehension"),
            ],
            name: grammar
                .field_id_for_name("name")
                .unwrap_or_else(|| panic!("the pinned Python grammar must define field name")),
            alias: field("alias"),
            module: field("module_name"),
            function: field("function"),
            object: field("object"),
            member: field("attribute"),
            left: field("left"),
        }
    })
}

impl EdgePythonKinds {
    pub(super) fn record_edge(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        if id == self.call {
            if let Some(callee) = syntax_element.child_by_field_id(self.function) {
                facts
                    .calls
                    .push(call(syntax_element, callee, self.edge_callee(callee)?)?);
            }
        } else if self.definitions.contains(&id) {
            self.edge_definition(visited, facts)?;
            self.edge_type_parameters(syntax_element, facts)?;
        } else if self.imports.contains(&id) {
            self.edge_import(visited, facts)?;
        } else if self.parameters.contains(&id) {
            self.edge_parameters(visited, facts)?;
        } else if self.assignment.contains(&id) {
            if let Some(target) = syntax_element.child_by_field_id(self.left) {
                self.edge_shadow(target, visited, facts)?;
            }
        } else if [self.identifier, self.attribute].contains(&id)
            || syntax_element.kind() == "member_type"
        {
            self.edge_annotation(syntax_element, facts)?;
        } else if self.unsupported.contains(&id)
            && let Some(scope) = edge_enclosing(visited, &self.scopes)
        {
            facts.unsupported_bindings.push(byte_range(scope)?);
        }
        Ok(())
    }

    fn edge_type_parameters(
        &self,
        definition: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(parameters) = definition.child_by_field_name("type_parameters") else {
            return Ok(());
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            let mut candidate = Some(parameter);
            let mut name = None;
            // A declared name sits below `type`, `constrained_type` or `splat_type` wrappers.
            for _ in 0..4 {
                let Some(element) = candidate else {
                    break;
                };
                if element.kind_id() == self.identifier {
                    name = Some(element);
                    break;
                }
                candidate = element.named_child(0);
            }
            if let Some(name) = name {
                facts.bindings.push(binding(
                    name,
                    definition,
                    byte_range(definition)?.start,
                    StaticBindingKind::Shadow,
                )?);
            } else {
                facts.unsupported_bindings.push(byte_range(definition)?);
            }
        }
        Ok(())
    }

    fn edge_annotation(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if element
            .parent()
            .is_some_and(|parent| matches!(parent.kind(), "attribute" | "member_type"))
        {
            return Ok(());
        }
        let mut current = element;
        let mut annotation = false;
        while let Some(parent) = current.parent() {
            if parent.kind() == "type" {
                annotation = true;
            }
            if parent.kind() == "type_alias_statement"
                && parent.child_by_field_name("left").is_some_and(|left| {
                    left.start_byte() <= element.start_byte()
                        && element.end_byte() <= left.end_byte()
                })
            {
                return Ok(());
            }
            if self.definitions.contains(&parent.kind_id()) {
                if parent
                    .child_by_field_name("type_parameters")
                    .is_some_and(|parameters| {
                        parameters.start_byte() <= element.start_byte()
                            && element.end_byte() <= parameters.end_byte()
                    })
                {
                    return Ok(());
                }
                break;
            }
            if matches!(
                parent.kind(),
                "block" | "module" | "expression_statement" | "call"
            ) {
                break;
            }
            current = parent;
        }
        if annotation {
            facts.references.push(super::StaticReference {
                range: byte_range(element)?,
                role: rift_core::ReferenceRole::Type,
            });
        }
        Ok(())
    }

    fn edge_callee(&self, callee: Node<'_>) -> Result<StaticCallee, RiftError> {
        if callee.kind_id() == self.identifier {
            return Ok(StaticCallee::Direct);
        }
        if callee.kind_id() == self.attribute {
            let receiver = callee
                .child_by_field_id(self.object)
                .and_then(|value| identifier(value, self.identifier));
            let member = callee
                .child_by_field_id(self.member)
                .and_then(|value| identifier(value, self.identifier));
            if let Some((receiver, member)) = receiver.zip(member) {
                return Ok(StaticCallee::Member {
                    receiver: byte_range(receiver)?,
                    member: byte_range(member)?,
                });
            }
        }
        Ok(StaticCallee::Unsupported)
    }

    fn edge_definition(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let name = syntax_element.child_by_field_id(self.name.get());
        let scope = edge_enclosing(visited, &self.scopes);
        if let Some((name, scope)) = name.zip(scope) {
            let kind = if scope.kind_id() == self.definitions[1] {
                StaticBindingKind::Shadow
            } else {
                StaticBindingKind::Local
            };
            facts
                .bindings
                .push(binding(name, scope, byte_range(scope)?.start, kind)?);
        }
        Ok(())
    }

    fn edge_import(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = edge_enclosing(visited, &self.scopes) else {
            return Ok(());
        };
        let module = syntax_element.child_by_field_id(self.module);
        let mut cursor = syntax_element.walk();
        for imported in syntax_element.children_by_field_id(self.name, &mut cursor) {
            let (name, alias) = if imported.kind_id() == self.aliased {
                (
                    imported.child_by_field_id(self.name.get()),
                    imported.child_by_field_id(self.alias),
                )
            } else {
                (Some(imported), None)
            };
            let Some(name) = name else {
                continue;
            };
            let local = alias.or_else(|| name.named_child(0)).unwrap_or(name);
            let (module, export) = match module {
                Some(module) => (module, Some(byte_range(name)?)),
                None => (name, None),
            };
            let kind = if export.is_none() && alias.is_none() && name.named_child_count() > 1 {
                StaticBindingKind::Shadow
            } else {
                StaticBindingKind::PythonImport {
                    module: byte_range(module)?,
                    export,
                }
            };
            facts
                .bindings
                .push(binding(local, scope, byte_range(scope)?.start, kind)?);
        }
        Ok(())
    }

    fn edge_parameters(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let mut cursor = syntax_element.walk();
        for parameter in syntax_element.named_children(&mut cursor) {
            let name = if parameter.kind_id() == self.identifier {
                Some(parameter)
            } else {
                parameter
                    .child_by_field_id(self.name.get())
                    .or_else(|| parameter.named_child(0))
            };
            if let Some(name) = name.filter(|name| name.kind_id() == self.identifier) {
                facts.bindings.push(binding(
                    name,
                    scope,
                    byte_range(scope)?.start,
                    StaticBindingKind::Shadow,
                )?);
            } else {
                facts.unsupported_bindings.push(byte_range(scope)?);
            }
        }
        Ok(())
    }

    fn edge_shadow(
        &self,
        target: Node<'_>,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(scope) = edge_enclosing(visited, &self.scopes) else {
            return Ok(());
        };
        if target.kind_id() == self.identifier {
            facts.bindings.push(binding(
                target,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::Shadow,
            )?);
        } else if target.kind_id() == self.attribute {
            facts.assignments.push(byte_range(target)?);
        } else {
            facts.unsupported_bindings.push(byte_range(scope)?);
        }
        Ok(())
    }
}

fn edge_enclosing<'tree>(visited: Visited<'_, 'tree>, kinds: &[u16]) -> Option<Node<'tree>> {
    let mut parent = visited.parent();
    while let Some(candidate) = parent {
        let syntax_element = candidate.syntax_element();
        if kinds.contains(&syntax_element.kind_id()) {
            return Some(syntax_element);
        }
        parent = candidate.parent();
    }
    None
}
