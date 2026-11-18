//! Python class checks that establish a receiver in a bounded source range.

use rift_error::RiftError;
use tree_sitter::{Language, Node};

use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticBinding, StaticBindingKind, StaticFactsParts, field, kind};

pub(super) struct TypeGuards {
    statement: u16,
    negative: u16,
    boolean: u16,
    parenthesized: u16,
    returns: [u16; 2],
    names: [u16; 2],
    condition: u16,
    consequence: u16,
    alternative: u16,
    argument: u16,
    left: u16,
    operator: u16,
    function: u16,
    arguments: u16,
}

impl TypeGuards {
    pub(super) fn new(grammar: &Language) -> Self {
        let id = |name| kind(grammar, name);
        let field = |name| field(grammar, name);
        Self {
            statement: id("if_statement"),
            negative: id("not_operator"),
            boolean: id("boolean_operator"),
            parenthesized: id("parenthesized_expression"),
            returns: [id("return_statement"), id("raise_statement")],
            names: [id("identifier"), id("attribute")],
            condition: field("condition"),
            consequence: field("consequence"),
            alternative: field("alternative"),
            argument: field("argument"),
            left: field("left"),
            operator: field("operator"),
            function: field("function"),
            arguments: field("arguments"),
        }
    }

    pub(super) fn record(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let statement = visited.syntax_element();
        if statement.kind_id() != self.statement {
            return Ok(());
        }
        let Some((name, annotation, scope)) = self.guard(visited, source) else {
            return Ok(());
        };
        facts.bindings.push(StaticBinding {
            name: byte_range(name)?,
            scope: crate::ByteRange {
                start: byte_range(name)?.start,
                end: byte_range(scope)?.end,
            },
            available_from: byte_range(statement)?.end,
            kind: StaticBindingKind::PythonGuard {
                annotation: byte_range(annotation)?,
            },
        });
        Ok(())
    }

    fn guard<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
        source: &str,
    ) -> Option<(Node<'tree>, Node<'tree>, Node<'tree>)> {
        let statement = visited.syntax_element();
        let mut condition = statement.child_by_field_id(self.condition)?;
        while condition.kind_id() == self.parenthesized || condition.kind_id() == self.boolean {
            condition = if condition.kind_id() == self.boolean {
                let operator = statement_text(condition.child_by_field_id(self.operator)?, source)?;
                if operator != "or" {
                    return None;
                }
                condition.child_by_field_id(self.left)?
            } else {
                condition.named_child(0)?
            };
        }
        if condition.kind_id() != self.negative
            || statement.child_by_field_id(self.alternative).is_some()
        {
            return None;
        }
        let call = condition.child_by_field_id(self.argument)?;
        if statement_text(call.child_by_field_id(self.function)?, source)? != "isinstance" {
            return None;
        }
        let arguments = call.child_by_field_id(self.arguments)?;
        if arguments.named_child_count() != 2 {
            return None;
        }
        let name = arguments.named_child(0)?;
        let annotation = arguments.named_child(1)?;
        if name.kind_id() != self.names[0] || !self.names.contains(&annotation.kind_id()) {
            return None;
        }
        let body = statement.child_by_field_id(self.consequence)?;
        let last =
            body.named_child(u32::try_from(body.named_child_count().checked_sub(1)?).ok()?)?;
        if !self.returns.contains(&last.kind_id()) {
            return None;
        }
        let scope = visited.parent()?.syntax_element();
        Some((name, annotation, scope))
    }
}

fn statement_text<'source>(element: Node<'_>, source: &'source str) -> Option<&'source str> {
    source.get(element.byte_range())
}
