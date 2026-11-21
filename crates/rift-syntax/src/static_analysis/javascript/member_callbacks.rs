//! Written types of callbacks in object literals assigned to TypeScript members.

use rift_error::RiftError;
use tree_sitter::Node;

use super::{Kinds, TypeShapeKinds, text};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticFactsParts, StaticMemberCallback};

impl Kinds {
    pub(super) fn record_member_type_shape(
        &self,
        declaration: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let id = declaration.kind_id();
        let shape = if id == shapes.interface {
            let mut cursor = declaration.walk();
            let extends = declaration
                .named_children(&mut cursor)
                .any(|child| child.kind() == "extends_type_clause");
            if extends {
                return Ok(());
            }
            declaration.child_by_field_id(shapes.body)
        } else if id == shapes.property_signature {
            self.type_field
                .and_then(|field| declaration.child_by_field_id(field))
                .and_then(|annotation| annotation.named_child(0))
        } else if Some(id) == self.type_alias {
            declaration.child_by_field_id(self.value)
        } else {
            return Ok(());
        };
        let name = declaration
            .child_by_field_id(self.name)
            .filter(|name| !name.is_missing());
        let Some((name, shape)) = name.zip(shape) else {
            return Ok(());
        };
        let shape = Self::unparenthesized(shape, shapes);
        let Some(shape) = single_inhabited_branch(shape, source) else {
            return Ok(());
        };
        if self.closed_member_type(shape, shapes) {
            facts
                .member_type_shapes
                .push((byte_range(name)?, byte_range(shape)?));
        }
        Ok(())
    }

    // Declarations with index signatures, mapped keys, or computed keys do not establish
    // which names are absent from the object type.
    fn closed_member_type(&self, shape: Node<'_>, shapes: &TypeShapeKinds) -> bool {
        if shape.has_error() || !shapes.signature_holders[..2].contains(&shape.kind_id()) {
            return false;
        }
        let mut cursor = shape.walk();
        shape.named_children(&mut cursor).all(|member| {
            member.is_extra()
                || shapes.signatures[..2].contains(&member.kind_id())
                || (matches!(member.kind(), "property_signature" | "method_signature")
                    && member
                        .child_by_field_id(self.name)
                        .is_some_and(|name| name.kind_id() == self.property_identifier))
        })
    }

    pub(super) fn record_member_callback(
        &self,
        function: Node<'_>,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(shapes) = &self.type_shapes else {
            return Ok(());
        };
        let value = visited.syntax_element();
        if value != function {
            return Ok(());
        }
        let Some(initializer) = visited.parent().filter(|parent| {
            let pair = parent.syntax_element();
            pair.kind() == "pair" && pair.child_by_field_id(self.value) == Some(value)
        }) else {
            return Ok(());
        };
        let callback = initializer
            .syntax_element()
            .child_by_field_id(shapes.key)
            .filter(|name| name.kind_id() == self.property_identifier && !name.is_missing());
        let Some((callback, object)) = callback.zip(initializer.parent()) else {
            return Ok(());
        };
        let Some(member) = self.asserted_assignment_member(object) else {
            return Ok(());
        };
        let property = member
            .child_by_field_id(self.property)
            .filter(|name| name.kind_id() == self.property_identifier && !name.is_missing());
        let mut receiver = member.child_by_field_id(self.object);
        // Each step descends within the admitted syntax depth.
        while let Some(parenthesized) = receiver.filter(|node| node.kind_id() == self.parenthesized)
        {
            receiver = parenthesized.named_child(0);
        }
        let asserted = receiver
            .filter(|receiver| Some(receiver.kind_id()) == self.asserted)
            .and_then(|receiver| receiver.named_child(1))
            .and_then(|asserted| self.member_assertion_types(asserted, shapes));
        let Some((property, (asserted, intersection))) = property.zip(asserted) else {
            return Ok(());
        };
        let property = byte_range(property)?;
        let callback = byte_range(callback)?;
        for parameter in self.callback_parameters(function) {
            if let Some(parameter) = self.unannotated_callback_parameter(parameter) {
                facts.member_callbacks.push(StaticMemberCallback {
                    parameter: byte_range(parameter)?,
                    property,
                    callback,
                    asserted,
                    intersection,
                });
            }
        }
        Ok(())
    }

    fn asserted_assignment_member<'tree>(
        &self,
        mut object: Visited<'_, 'tree>,
    ) -> Option<Node<'tree>> {
        if object.syntax_element().kind_id() != self.object_literal[0] {
            return None;
        }
        let mut parent = object.parent()?;
        // The syntax walk has admitted every enclosing expression's depth.
        while parent.syntax_element().kind_id() == self.parenthesized {
            object = parent;
            parent = parent.parent()?;
        }
        let assignment = parent.syntax_element();
        if assignment.kind() != "assignment_expression"
            || assignment.child_by_field_id(self.right) != Some(object.syntax_element())
        {
            return None;
        }
        assignment
            .child_by_field_id(self.left)
            .filter(|member| member.kind_id() == self.member)
    }

    fn member_assertion_types(
        &self,
        asserted: Node<'_>,
        shapes: &TypeShapeKinds,
    ) -> Option<(ByteRange, Option<ByteRange>)> {
        let asserted = Self::unparenthesized(asserted, shapes);
        let range = |node| byte_range(self.named_type(Self::unparenthesized(node, shapes))?).ok();
        if asserted.kind() == "intersection_type" {
            let mut cursor = asserted.walk();
            let mut arms = asserted
                .named_children(&mut cursor)
                .filter(|arm| !arm.is_extra());
            let first = range(arms.next()?)?;
            let second = range(arms.next()?)?;
            return arms.next().is_none().then_some((first, Some(second)));
        }
        Some((range(asserted)?, None))
    }
}

fn single_inhabited_branch<'tree>(shape: Node<'tree>, source: &str) -> Option<Node<'tree>> {
    if shape.kind() != "conditional_type" {
        return Some(shape);
    }
    if shape.has_error() {
        return None;
    }
    let consequence = shape.child_by_field_name("consequence")?;
    let alternative = shape.child_by_field_name("alternative")?;
    match (text(consequence, source), text(alternative, source)) {
        (Some("never"), _) => Some(alternative),
        (_, Some("never")) => Some(consequence),
        _ => None,
    }
}
