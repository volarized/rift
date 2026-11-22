//! Python class checks that establish a receiver in a bounded source range.

use rift_error::RiftError;
use tree_sitter::{Language, Node};

use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticBinding, StaticBindingKind, StaticFactsParts, field, kind};

pub(super) struct TypeGuards {
    statement: u16,
    negative: u16,
    boolean: u16,
    comparison: u16,
    none: u16,
    parenthesized: u16,
    returns: [u16; 2],
    names: [u16; 2],
    condition: u16,
    consequence: u16,
    alternative: u16,
    argument: u16,
    left: u16,
    right: u16,
    operator: u16,
    comparison_operators: u16,
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
            comparison: id("comparison_operator"),
            none: id("none"),
            parenthesized: id("parenthesized_expression"),
            returns: [id("return_statement"), id("raise_statement")],
            names: [id("identifier"), id("attribute")],
            condition: field("condition"),
            consequence: field("consequence"),
            alternative: field("alternative"),
            argument: field("argument"),
            left: field("left"),
            right: field("right"),
            operator: field("operator"),
            comparison_operators: field("operators"),
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
        self.record_non_none(statement, facts)?;
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

    /// Collects the checks that must pass for the if statement's body to run.
    fn record_non_none(
        &self,
        statement: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let condition = statement
            .child_by_field_id(self.condition)
            .filter(|condition| !condition.has_error());
        let body = statement
            .child_by_field_id(self.consequence)
            .filter(|body| body.start_byte() < body.end_byte());
        let Some((condition, body)) = condition.zip(body) else {
            return Ok(());
        };
        let body = byte_range(body)?;
        let mut pending = vec![condition];
        // Each condition is visited once within the provider's syntax element bound.
        while let Some(condition) = pending.pop() {
            if condition.kind_id() == self.parenthesized {
                pending.extend(condition.named_child(0));
            } else if condition.kind_id() == self.boolean
                && condition
                    .child_by_field_id(self.operator)
                    .is_some_and(|operator| operator.kind() == "and")
            {
                pending.extend(condition.child_by_field_id(self.right));
                pending.extend(condition.child_by_field_id(self.left));
            } else if let Some(name) = self.non_none(condition).filter(|name| !name.is_missing()) {
                facts.non_none_guards.push((byte_range(name)?, body));
            }
        }
        Ok(())
    }

    /// A name whose truthiness or comparison excludes `None`.
    fn non_none<'tree>(&self, condition: Node<'tree>) -> Option<Node<'tree>> {
        if condition.kind_id() == self.names[0] {
            return Some(condition);
        }
        if condition.kind_id() != self.comparison
            || condition
                .child_by_field_id(self.comparison_operators)?
                .kind()
                != "is not"
        {
            return None;
        }
        let mut cursor = condition.walk();
        let mut operands = condition
            .named_children(&mut cursor)
            .filter(|operand| !operand.is_extra());
        let (left, right) = (operands.next()?, operands.next()?);
        if operands.next().is_some() {
            return None;
        }
        match (left.kind_id(), right.kind_id()) {
            (name, none) if name == self.names[0] && none == self.none => Some(left),
            (none, name) if name == self.names[0] && none == self.none => Some(right),
            _ => None,
        }
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
