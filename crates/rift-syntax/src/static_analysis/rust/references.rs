//! Rust field accesses and authored type uses.

use rift_core::ReferenceRole;
use rift_error::RiftError;

use super::Kinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticFactsParts, StaticReference};

impl Kinds {
    pub(super) fn record_reference(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let id = element.kind_id();
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
