//! Rust field accesses and authored type uses.

use rift_core::ReferenceRole;
use rift_error::RiftError;
use tree_sitter::Node;

use super::Kinds;
use crate::extract::byte_range;
use crate::static_analysis::{StaticFactsParts, StaticReference};

impl Kinds {
    pub(super) fn record_reference(
        &self,
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let id = element.kind_id();
        if id == self.field_expression {
            self.record_access(element, facts)?;
        } else if [self.type_identifier, self.scoped_type_identifier].contains(&id) {
            let Some(parent) = element.parent() else {
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
        element: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(parent) = element.parent() else {
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
