//! Rust field accesses and authored type uses.

use rift_core::ReferenceRole;
use rift_error::RiftError;

use super::Kinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{
    StaticFactsParts, StaticReference, StaticRustFieldSite, StaticRustValueRead, enclosing,
};

impl Kinds {
    pub(super) fn record_reference(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let id = element.kind_id();
        if element.kind() == "crate"
            && let Some(parent) = visited.parent().map(Visited::syntax_element)
            && (parent.child_by_field_id(self.path) == Some(element)
                || parent.kind() == "use_wildcard")
        {
            facts.rust_crate_paths.push(byte_range(element)?);
        }
        let constructor = id == self.local_value_struct
            && element
                .child_by_field_id(self.body)
                .is_none_or(|body| body.kind_id() == self.ordered_value_fields);
        if self.value_items.contains(&id) || constructor {
            self.record_local_value(visited, facts)?;
        } else if [
            self.identifier,
            self.scoped_identifier,
            self.scoped_type_identifier,
        ]
        .contains(&id)
        {
            self.record_value_path(visited, facts)?;
        }
        if (id == self.local_value_struct || self.local_type_items.contains(&id))
            && let Some(block) = self.local_item_block(visited)
        {
            facts
                .rust_local_types
                .push((byte_range(element)?, byte_range(block)?));
        } else if id == self.field_pattern || self.field_initializers.contains(&id) {
            self.record_field_site(visited, facts)?;
        } else if id == self.field_expression {
            self.record_access(visited, facts)?;
        } else if [self.type_identifier, self.scoped_type_identifier].contains(&id) {
            let Some(parent) = visited.parent().map(Visited::syntax_element) else {
                return Ok(());
            };
            let declaration_name = parent.child_by_field_id(self.name) == Some(element);
            if parent.kind_id() != self.scoped_type_identifier && !declaration_name {
                facts.references.push(StaticReference {
                    target: byte_range(element)?,
                    role: ReferenceRole::Type,
                });
            }
        }
        Ok(())
    }

    fn record_local_value(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scope = self.local_item_block(visited);
        let name = visited
            .syntax_element()
            .child_by_field_id(self.name)
            .filter(|name| {
                [self.identifier, self.type_identifier].contains(&name.kind_id())
                    && !name.is_missing()
            });
        if let Some((name, scope)) = name.zip(scope) {
            facts
                .rust_local_values
                .push((byte_range(name)?, byte_range(scope)?));
        }
        Ok(())
    }

    fn local_item_block<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
    ) -> Option<tree_sitter::Node<'tree>> {
        enclosing(visited, &self.scopes)
            .map(Visited::syntax_element)
            .filter(|node| node.kind_id() == self.scopes[1])
    }

    fn record_value_path(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let Some(parent) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let id = parent.kind_id();
        let terminal_pattern = [self.scoped_identifier, self.scoped_type_identifier]
            .contains(&element.kind_id())
            && ((id == self.match_pattern
                && parent.child_by_field_id(self.condition) != Some(element))
                || (self.named_value_patterns.contains(&id)
                    && parent.child_by_field_id(self.value_type) == Some(element)));
        let value = element.kind_id() != self.scoped_type_identifier
            && ((id == self.let_declaration
                && parent.child_by_field_id(self.value) == Some(element))
                || id == self.scopes[1]
                || self.value_path_statements.contains(&id));
        if terminal_pattern || value {
            facts.rust_value_reads.push(StaticRustValueRead {
                range: byte_range(element)?,
                pattern: terminal_pattern,
            });
        }
        Ok(())
    }

    fn record_field_site(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let Some(parent) = visited.parent() else {
            return Ok(());
        };
        let pattern = element.kind_id() == self.field_pattern;
        let Some(construct) = (if pattern {
            Some(parent)
        } else {
            parent.parent()
        })
        .map(Visited::syntax_element)
        .filter(|construct| {
            construct.kind_id()
                == if pattern {
                    self.struct_pattern
                } else {
                    self.struct_expression
                }
        }) else {
            return Ok(());
        };
        let owner_field = if pattern { self.value_type } else { self.name };
        let Some(owner) = construct
            .child_by_field_id(owner_field)
            .and_then(|owner| self.named_type(owner))
        else {
            return Ok(());
        };
        // Attributes at an initializer can remove exactly this field before evaluation.
        let mut cursor = element.walk();
        if element
            .named_children(&mut cursor)
            .any(|child| child.kind_id() == self.attribute)
        {
            return Ok(());
        }
        let field = if pattern {
            element.child_by_field_id(self.name)
        } else if element.kind_id() == self.field_initializers[0] {
            element.child_by_field_id(self.member)
        } else {
            element
                .named_children(&mut cursor)
                .find(|child| child.kind_id() == self.identifier)
        };
        let Some(field) = field.filter(|field| {
            !field.is_missing()
                && [self.identifier, self.field_identifier, self.shorthand_field]
                    .contains(&field.kind_id())
        }) else {
            return Ok(());
        };
        facts.rust_field_sites.push(StaticRustFieldSite {
            owner: byte_range(owner)?,
            field: byte_range(field)?,
            literal: (!pattern).then(|| byte_range(construct)).transpose()?,
        });
        Ok(())
    }

    fn record_access(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let Some(parent) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let range = byte_range(element)?;
        let assignment = self.assignment.contains(&parent.kind_id())
            && parent.child_by_field_id(self.left) == Some(element);
        if !assignment || parent.kind_id() == self.assignment[1] {
            facts.references.push(StaticReference {
                target: range,
                role: ReferenceRole::Read,
            });
        }
        if assignment {
            facts.references.push(StaticReference {
                target: range,
                role: ReferenceRole::Write,
            });
        }
        Ok(())
    }
}
