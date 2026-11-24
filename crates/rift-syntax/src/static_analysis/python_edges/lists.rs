//! Uses that keep the elements of a name's empty list known.

use std::collections::BTreeMap;

use rift_error::RiftError;
use tree_sitter::Node;

use super::EdgePythonKinds;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::StaticFactsParts;
use crate::{ByteRange, StaticBinding, StaticBindingKind};

/// List methods after which the elements are the ones appended so far.
const METHODS: [&str; 9] = [
    "append", "clear", "copy", "count", "index", "pop", "remove", "reverse", "sort",
];

/// Names bound to an empty list literal and the positions of their captured bindings.
#[derive(Debug, Default)]
pub(in crate::static_analysis) struct EmptyLists(BTreeMap<String, Vec<usize>>);

impl EmptyLists {
    /// Notes the empty lists among the bindings captured from `first` on. A second list of one
    /// name in one scope leaves both unknown.
    pub(in crate::static_analysis) fn note(
        &mut self,
        facts: &mut StaticFactsParts,
        first: usize,
        source: &str,
    ) {
        for position in first..facts.bindings.len() {
            let binding = facts.bindings[position];
            if binding.kind != StaticBindingKind::PythonEmptyList {
                continue;
            }
            let Some(name) = text(source, binding.name) else {
                continue;
            };
            let lists = self.0.entry(name.to_owned()).or_default();
            let known = lists
                .iter()
                .copied()
                .find(|&list| facts.bindings[list].scope == binding.scope);
            if let Some(known) = known {
                leave_unknown(&mut facts.bindings[known]);
                leave_unknown(&mut facts.bindings[position]);
            } else {
                lists.push(position);
            }
        }
    }

    /// Leaves a list unknown when `visited` uses its name in a way that may add, replace or
    /// share elements: anything but iteration, `len` and the listed methods.
    pub(in crate::static_analysis) fn record_use(
        &mut self,
        kinds: &EdgePythonKinds,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let name = visited.syntax_element();
        if self.0.is_empty() || name.kind_id() != kinds.identifier {
            return Ok(());
        }
        let Some(lists) = source
            .get(name.byte_range())
            .and_then(|name| self.0.get_mut(name))
        else {
            return Ok(());
        };
        let used = byte_range(name)?;
        // The walk is in source order: a scope it has left holds no further use.
        lists.retain(|&list| facts.bindings[list].scope.contains(used.start));
        if lists.is_empty() || kinds.keeps_list(visited, source) {
            return Ok(());
        }
        for &list in lists.iter() {
            if facts.bindings[list].name != used {
                leave_unknown(&mut facts.bindings[list]);
            }
        }
        Ok(())
    }
}

fn leave_unknown(binding: &mut StaticBinding) {
    binding.kind = StaticBindingKind::Shadow;
    binding.available_from = binding.scope.start;
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

impl EdgePythonKinds {
    fn keeps_list(&self, name: Visited<'_, '_>, source: &str) -> bool {
        let Some(parent) = name.parent() else {
            return false;
        };
        let holder = parent.syntax_element();
        let id = holder.kind_id();
        let is = |field: u16, element: Node<'_>| {
            holder
                .child_by_field_id(field)
                .is_some_and(|child| child.id() == element.id())
        };
        let element = name.syntax_element();
        if id == self.assignment[0] {
            return is(self.left, element);
        }
        if self.assignment[2..].contains(&id) {
            return is(self.right, element);
        }
        if id == self.keyword_argument {
            return is(self.name.get(), element);
        }
        if id == self.attribute {
            return !is(self.object, element) || self.calls_list_method(parent, source);
        }
        id == self.argument_list
            && holder.named_child_count() == 1
            && parent
                .parent()
                .and_then(|call| call.syntax_element().child_by_field_id(self.function))
                .is_some_and(|function| source.get(function.byte_range()) == Some("len"))
    }

    fn calls_list_method(&self, attribute: Visited<'_, '_>, source: &str) -> bool {
        let element = attribute.syntax_element();
        let called = attribute.parent().is_some_and(|call| {
            let call = call.syntax_element();
            call.kind_id() == self.call
                && call
                    .child_by_field_id(self.function)
                    .is_some_and(|function| function.id() == element.id())
        });
        called
            && element
                .child_by_field_id(self.member)
                .and_then(|method| source.get(method.byte_range()))
                .is_some_and(|method| METHODS.contains(&method))
    }
}
