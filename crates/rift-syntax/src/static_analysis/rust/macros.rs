//! Bindings proved by simple local declarative macro rules.

use std::collections::BTreeMap;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{Kinds, StaticBindingKind, StaticFactsParts, binding, enclosing};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};

#[derive(Default)]
pub(in crate::static_analysis) struct Macros<'source>(BTreeMap<&'source str, Vec<Definition>>);

struct Definition {
    scope: ByteRange,
    binds_argument: bool,
}

impl Kinds {
    pub(super) fn record_macro_definition<'source>(
        &self,
        visited: Visited<'_, '_>,
        source: &'source str,
        macros: &mut Macros<'source>,
    ) -> Result<(), RiftError> {
        let definition = visited.syntax_element();
        if let Some(name) = definition
            .child_by_field_id(self.name)
            .and_then(|name| written(name, source))
            && let Some(scope) = enclosing(visited, &self.scopes)
        {
            let attributes = visited.previous_sibling().is_some_and(|previous| {
                let previous = previous.syntax_element();
                previous.kind_id() == self.attribute || previous.is_extra()
            });
            let binds_argument = !attributes && simple_binding(definition, source).is_some();
            let scope = byte_range(scope.syntax_element())?;
            let start = byte_range(definition)?.start;
            let definitions = macros.0.entry(name).or_default();
            discard_ended(definitions, start);
            let definition = Definition {
                scope,
                binds_argument,
            };
            if let Some(current) = definitions
                .last_mut()
                .filter(|current| current.scope == scope)
            {
                *current = definition;
            } else {
                definitions.push(definition);
            }
        }
        Ok(())
    }

    pub(super) fn record_macro_binding<'source>(
        &self,
        visited: Visited<'_, '_>,
        source: &'source str,
        macros: &mut Macros<'source>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let invocation = visited.syntax_element();
        let Some((argument, name)) = self.simple_macro_invocation(invocation, source) else {
            return Ok(());
        };
        let Some(parent) = visited.parent() else {
            return Ok(());
        };
        let statement = if parent.syntax_element().kind() == "expression_statement" {
            parent
        } else {
            visited
        };
        let Some(block) = statement
            .parent()
            .map(Visited::syntax_element)
            .filter(|block| block.kind_id() == self.scopes[1])
        else {
            return Ok(());
        };
        let Some(definitions) = macros.0.get_mut(name) else {
            return Ok(());
        };
        let range = byte_range(statement.syntax_element())?;
        discard_ended(definitions, range.start);
        if definitions
            .last()
            .is_some_and(|definition| definition.binds_argument)
        {
            facts.bindings.push(binding(
                argument,
                block,
                range.end,
                StaticBindingKind::Shadow,
            )?);
        }
        Ok(())
    }

    fn simple_macro_invocation<'tree, 'source>(
        &self,
        invocation: Node<'tree>,
        source: &'source str,
    ) -> Option<(Node<'tree>, &'source str)> {
        let name = invocation
            .child_by_field_id(self.macro_name)
            .filter(|name| name.kind_id() == self.identifier)?;
        let arguments = invocation.named_child(1)?;
        let argument = arguments.child(1)?;
        if arguments.child_count() == 3
            && argument.kind_id() == self.identifier
            && !invocation.has_error()
        {
            Some((argument, written(name, source)?))
        } else {
            None
        }
    }
}

/// A single ident argument is bound by an unconditional let in the expansion.
fn simple_binding(definition: Node<'_>, source: &str) -> Option<()> {
    if definition.has_error() || definition.named_child_count() != 2 {
        return None;
    }
    let rule = definition.named_child(1)?;
    let pattern = rule.child_by_field_name("left")?;
    let expansion = rule.child_by_field_name("right")?;
    if pattern.child_count() != 3 || expansion.child_count() != 7 {
        return None;
    }
    let argument = pattern.child(1)?;
    let variable = written(argument.child_by_field_name("name")?, source)?;
    (written(argument.child_by_field_name("type")?, source) == Some("ident")
        && written(expansion.child(1)?, source) == Some("let")
        && written(expansion.child(2)?, source) == Some(variable)
        && written(expansion.child(3)?, source) == Some("=")
        && expansion.child(4)?.kind() == "identifier"
        && written(expansion.child(5)?, source) == Some(";"))
    .then_some(())
}

/// The syntax walk visits authored statements in order, so an ended scope stays ended.
fn discard_ended(definitions: &mut Vec<Definition>, position: u64) {
    while definitions
        .last()
        .is_some_and(|definition| !definition.scope.contains(position))
    {
        definitions.pop();
    }
}

fn written<'source>(node: Node<'_>, source: &'source str) -> Option<&'source str> {
    source.get(node.byte_range())
}
