//! Rust field accesses and authored type uses.

use rift_core::ReferenceRole;
use rift_error::RiftError;

use super::Kinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticFactsParts, StaticReference, StaticRustValueRead, enclosing};

impl Kinds {
    pub(super) fn record_reference(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let id = element.kind_id();
        let constructor = id == self.local_value_struct
            && element
                .child_by_field_id(self.body)
                .is_none_or(|body| body.kind_id() == self.ordered_value_fields);
        if self.local_value_items.contains(&id) || constructor {
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
        if id == self.field_expression {
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
        let scope = enclosing(visited, &self.scopes)
            .map(Visited::syntax_element)
            .filter(|node| node.kind_id() == self.scopes[1]);
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
