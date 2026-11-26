//! Rust imports retain each leaf and its enclosing path components.

use rift_core::LoopBudget;
use rift_error::{RiftError, errors};
use tree_sitter::Node;

use super::Kinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{
    StaticBindingKind, StaticFactsParts, StaticRustUse, binding, enclosing,
};

impl Kinds {
    pub(super) fn record_extern_crate(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let declaration = visited.syntax_element();
        let Some(scope) = enclosing(visited, &self.scopes).map(Visited::syntax_element) else {
            return Ok(());
        };
        let Some(crate_name) = declaration.child_by_field_id(self.name) else {
            return Ok(());
        };
        let name = declaration
            .child_by_field_id(self.alias)
            .unwrap_or(crate_name);
        if declaration.has_error() || crate_name.is_missing() || name.is_missing() {
            return Ok(());
        }
        facts.bindings.push(binding(
            name,
            scope,
            byte_range(scope)?.start,
            StaticBindingKind::RustExternCrate {
                crate_name: byte_range(crate_name)?,
            },
        )?);
        Ok(())
    }

    pub(super) fn record_import(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
        budget: &mut LoopBudget,
    ) -> Result<(), RiftError> {
        let element = visited.syntax_element();
        let Some(scope) = enclosing(visited, &self.scopes).map(Visited::syntax_element) else {
            return Ok(());
        };
        let Some(argument) = element.child_by_field_id(self.argument) else {
            return Ok(());
        };
        let mut declaration_cursor = element.walk();
        let visibility = element
            .named_children(&mut declaration_cursor)
            .find(|child| child.kind() == "visibility_modifier")
            .map(byte_range)
            .transpose()?;
        let mut pending = vec![(argument, Vec::new())];
        let mut cursor = element.walk();
        while let Some((element, mut prefix)) = pending.pop() {
            if element.kind_id() == self.use_list[0] {
                for child in element
                    .named_children(&mut cursor)
                    .filter(|child| !child.is_extra())
                {
                    charge(budget, prefix.len().saturating_add(1))?;
                    pending.push((child, prefix.clone()));
                }
            } else if element.kind_id() == self.use_list[1] {
                if let Some(path) = element.child_by_field_id(self.path) {
                    charge(budget, 1)?;
                    prefix.push(byte_range(path)?);
                }
                if let Some(list) = element
                    .named_children(&mut cursor)
                    .find(|child| child.kind_id() == self.use_list[0])
                {
                    pending.push((list, prefix));
                }
            } else {
                self.import_leaf(element, scope, prefix, visibility, facts, budget)?;
            }
        }
        Ok(())
    }

    fn import_leaf(
        &self,
        element: Node<'_>,
        scope: Node<'_>,
        mut prefix: Vec<crate::ByteRange>,
        visibility: Option<crate::ByteRange>,
        facts: &mut StaticFactsParts,
        budget: &mut LoopBudget,
    ) -> Result<(), RiftError> {
        let glob = element.kind_id() == self.use_list[2];
        let (path, name) = if element.kind_id() == self.use_alias {
            (
                element.child_by_field_id(self.path),
                element.child_by_field_id(self.alias),
            )
        } else if glob {
            (element.named_child(0), Some(element))
        } else {
            (
                Some(element),
                element.child_by_field_id(self.name).or(Some(element)),
            )
        };
        if glob && path.is_none() && !prefix.is_empty() {
            facts.rust_uses.push(StaticRustUse {
                range: byte_range(element)?,
                path: prefix,
                scope: byte_range(scope)?,
                glob: true,
                visibility,
            });
            return Ok(());
        }
        let Some((path, mut name)) = path
            .zip(name)
            .filter(|(path, name)| !path.is_missing() && !name.is_missing())
        else {
            return Ok(());
        };
        if name.kind_id() == self.self_value
            && let Some(group) = element.parent().and_then(|parent| parent.parent())
            && let Some(group_path) = group.child_by_field_id(self.path)
        {
            name = group_path
                .child_by_field_id(self.name)
                .unwrap_or(group_path);
        }
        let range = byte_range(path)?;
        charge(budget, 1)?;
        prefix.push(range);
        facts.rust_uses.push(StaticRustUse {
            range,
            path: prefix,
            scope: byte_range(scope)?,
            glob,
            visibility,
        });
        if !glob {
            facts.bindings.push(binding(
                name,
                scope,
                byte_range(scope)?.start,
                StaticBindingKind::RustUse { path: range },
            )?);
        }
        Ok(())
    }
}

fn charge(budget: &mut LoopBudget, count: usize) -> Result<(), RiftError> {
    for _ in 0..count {
        budget
            .consume()
            .map_err(|_| errors::syntax::facts_count_exceeded().error())?;
    }
    Ok(())
}
