//! Python names that a statement binds outside assignments, loops and parameters.

use std::collections::BTreeSet;

use rift_error::RiftError;
use tree_sitter::Language;

use super::{EdgePythonKinds, edge_enclosing};
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{
    PythonDeletion, StaticBinding, StaticBindingKind, StaticFactsParts, kind,
};

/// Syntax elements around a name that state its use.
pub(super) struct NameKinds {
    /// Patterns and displays that hold more than one target.
    containers: [u16; 9],
    as_target: u16,
    as_pattern: u16,
    except: u16,
    delete: u16,
    case: u16,
    /// Patterns of a `match` statement whose plain name is a capture.
    captures: [u16; 4],
    splat: u16,
    dotted: u16,
    pub(super) type_alias: u16,
}

impl NameKinds {
    pub(super) fn new(grammar: &Language) -> Self {
        let id = |name| kind(grammar, name);
        Self {
            containers: [
                id("pattern_list"),
                id("tuple_pattern"),
                id("list_pattern"),
                id("list_splat_pattern"),
                id("tuple"),
                id("list"),
                id("list_splat"),
                id("parenthesized_expression"),
                id("expression_list"),
            ],
            as_target: id("as_pattern_target"),
            as_pattern: id("as_pattern"),
            except: id("except_clause"),
            delete: id("delete_statement"),
            case: id("case_pattern"),
            captures: [
                id("case_pattern"),
                id("union_pattern"),
                id("keyword_pattern"),
                id("dict_pattern"),
            ],
            splat: id("splat_pattern"),
            dotted: id("dotted_name"),
            type_alias: id("type_alias_statement"),
        }
    }
}

/// The names of one file that wait for its other bindings.
#[derive(Default)]
pub(in crate::static_analysis) struct Names {
    deleted: Vec<StaticBinding>,
}

impl Names {
    /// Binds each deleted name its scope does not bind otherwise. Beside another binding a
    /// `del` statement changes no scope.
    pub(in crate::static_analysis) fn close(self, source: &str, facts: &mut StaticFactsParts) {
        if self.deleted.is_empty() {
            return;
        }
        let text = |binding: &StaticBinding| {
            let start = usize::try_from(binding.name.start).ok()?;
            let end = usize::try_from(binding.name.end).ok()?;
            source.get(start..end).map(|name| (binding.scope, name))
        };
        let mut bound = facts
            .bindings
            .iter()
            .filter(|binding| !matches!(binding.kind, StaticBindingKind::PythonMemberWrite { .. }))
            .map(text)
            .collect::<BTreeSet<_>>();
        let deleted = self.deleted.into_iter();
        let added = deleted.filter(|binding| bound.insert(text(binding)));
        let added = added.collect::<Vec<_>>();
        facts.bindings.extend(added);
    }
}

/// What binds one name outside assignments, loops and parameters.
#[derive(Clone, Copy)]
enum NameTarget {
    /// A target of `with`, `except` or `match`.
    Bound,
    /// A target of `del`, which makes the name one of its scope.
    Deleted,
}

impl EdgePythonKinds {
    /// Binds the targets of `with`, `except` and `match`, and collects the targets of `del`.
    pub(super) fn edge_name_target(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
        names: &mut Names,
    ) -> Result<(), RiftError> {
        let name = visited.syntax_element();
        if name.kind_id() != self.identifier {
            return Ok(());
        }
        let Some(target) = self.name_target(visited) else {
            return Ok(());
        };
        let Some(scope) = edge_enclosing(visited, &self.scopes) else {
            return Ok(());
        };
        let end = byte_range(name)?.end;
        let binding = self.scoped_binding(name, scope, end, StaticBindingKind::Shadow)?;
        match target {
            NameTarget::Bound => facts.bindings.push(binding),
            NameTarget::Deleted => {
                names.deleted.push(StaticBinding {
                    available_from: binding.scope.start,
                    ..binding
                });
                if scope.kind_id() == self.scopes[1]
                    && let Some(block) = edge_enclosing(visited, &[self.block])
                {
                    facts.python_deletions.push(PythonDeletion {
                        name: binding.name,
                        function: binding.scope,
                        block: byte_range(block)?,
                    });
                }
            }
        }
        Ok(())
    }

    /// Binds the name a `type` statement declares.
    pub(super) fn edge_type_alias(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let mut name = visited.syntax_element().child_by_field_id(self.left);
        // The name stands under `type`, and one with parameters under `generic_type` as well.
        for _ in 0..2 {
            name = name
                .filter(|element| element.kind_id() != self.identifier)
                .and_then(|element| element.named_child(0))
                .or(name);
        }
        if let Some(name) = name.filter(|element| element.kind_id() == self.identifier)
            && let Some(scope) = edge_enclosing(visited, &self.scopes)
        {
            let end = byte_range(name)?.end;
            let binding = self.scoped_binding(name, scope, end, StaticBindingKind::Shadow)?;
            facts.bindings.push(binding);
        }
        Ok(())
    }

    /// The target `visited` is, outside assignments, loops and parameters.
    fn name_target(&self, visited: Visited<'_, '_>) -> Option<NameTarget> {
        let names = &self.names;
        let mut current = visited;
        // Each step climbs one syntax element the shared walk has admitted.
        while let Some(parent) = current.parent() {
            let element = current.syntax_element();
            let holder = parent.syntax_element();
            let id = holder.kind_id();
            if names.containers.contains(&id) {
                current = parent;
                continue;
            }
            if id == names.delete {
                return Some(NameTarget::Deleted);
            }
            let direct = element.id() == visited.syntax_element().id();
            let bound = if id == names.as_target {
                true
            } else if id == names.except {
                holder
                    .child_by_field_id(self.alias)
                    .is_some_and(|alias| alias.id() == element.id())
            } else if id == names.as_pattern {
                // `case pattern as name`. The expression form keeps its target apart.
                holder
                    .named_child(0)
                    .is_some_and(|pattern| pattern.kind_id() == names.case)
            } else if id == names.dotted {
                holder.named_child_count() == 1
                    && parent.parent().is_some_and(|pattern| {
                        names.captures.contains(&pattern.syntax_element().kind_id())
                    })
            } else {
                id == names.splat
            };
            return (bound && (direct || id == names.as_target || id == names.except))
                .then_some(NameTarget::Bound);
        }
        None
    }
}
