//! Authored macro declarations, calls and possible changes to their textual scopes.

use rift_error::RiftError;
use tree_sitter::Node;

use super::Kinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{StaticFactsParts, StaticRustMacroCall, StaticRustMacroDefinition};

impl Kinds {
    pub(super) fn record_textual_macro_definition(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let item = visited.syntax_element();
        if let Some(scope) = self.macro_scope(visited)
            && let Some(name) = item
                .child_by_field_id(self.name)
                .filter(|name| !name.is_missing())
        {
            facts
                .rust_macro_definitions
                .push(StaticRustMacroDefinition {
                    name: byte_range(name)?,
                    item: byte_range(item)?,
                    scope: byte_range(scope)?,
                });
        }
        Ok(())
    }

    pub(super) fn record_macro_invocation(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let invocation = visited.syntax_element();
        // An opaque item or standalone statement can introduce a later binding.
        let statement = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.value_path_statements[1])
            .unwrap_or(visited);
        self.record_macro_scope_barrier(statement, facts)?;

        let Some(path) = invocation.child_by_field_id(self.macro_name) else {
            return Ok(());
        };
        if path.is_missing() {
            return Ok(());
        }
        let (calls, name) = if path.kind_id() == self.identifier {
            (&mut facts.rust_textual_macro_calls, path)
        } else if path.kind_id() == self.scoped_identifier
            && let Some(root) = path.child_by_field_id(self.path)
            && source.get(root.byte_range()) == Some("crate")
            && let Some(name) = path.child_by_field_id(self.name)
            && !name.is_missing()
        {
            (&mut facts.rust_root_macro_calls, name)
        } else {
            return Ok(());
        };
        calls.push(StaticRustMacroCall {
            range: byte_range(invocation)?,
            name: byte_range(name)?,
        });
        Ok(())
    }

    pub(super) fn record_macro_scope_barrier(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if let Some(scope) = self.macro_scope(visited) {
            facts
                .rust_macro_barriers
                .push((byte_range(visited.syntax_element())?, byte_range(scope)?));
        }
        Ok(())
    }

    pub(super) fn record_macro_attribute_barrier(
        &self,
        attribute: Visited<'_, '_>,
        attributed_item: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        if attribute.syntax_element().kind_id() == self.inner_attribute {
            if let Some(module) = attribute
                .parent()
                .and_then(Visited::parent)
                .filter(|module| module.syntax_element().kind_id() == self.local_type_items[2])
            {
                self.record_macro_scope_barrier(module, facts)?;
            }
        } else if let Some(scope) = self.macro_scope(attribute) {
            facts
                .rust_macro_barriers
                .push((byte_range(attributed_item)?, byte_range(scope)?));
        }
        Ok(())
    }

    fn macro_scope<'tree>(&self, visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
        visited
            .parent()
            .map(Visited::syntax_element)
            .filter(|scope| self.scopes.contains(&scope.kind_id()))
    }
}
