//! Uses that keep the elements of a name's empty list known.

use std::collections::BTreeMap;

use rift_error::RiftError;
use tree_sitter::Node;

use super::EdgePythonKinds;
use crate::extract::byte_range;
use crate::static_analysis::StaticFactsParts;
use crate::{ByteRange, StaticBinding, StaticBindingKind};

/// List methods after which the elements are the ones appended so far.
const METHODS: [&str; 9] = [
    "append", "clear", "copy", "count", "index", "pop", "remove", "reverse", "sort",
];

/// The empty lists of one name, by the start of the scope each is bound in.
#[derive(Default)]
struct NamedLists {
    /// Positions of the captured bindings whose scope the walk has not reached, the last first.
    ahead: Vec<usize>,
    /// Positions of the captured bindings whose scope holds the current element, outer first.
    around: Vec<usize>,
}

impl EdgePythonKinds {
    /// Leaves every empty list unknown that its scope uses in a way that may add, replace or
    /// share elements: anything but iteration, `len` and the listed methods. Reads each
    /// accepted syntax element once when the file binds an empty list.
    pub(in crate::static_analysis) fn close_lists(
        &self,
        root: Node<'_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let mut lists = empty_lists(source, facts);
        if lists.is_empty() {
            return Ok(());
        }
        let mut around = Vec::new();
        let mut cursor = root.walk();
        'walk: loop {
            let element = cursor.node();
            if element.kind_id() == self.identifier
                && let Some(named) = source
                    .get(element.byte_range())
                    .and_then(|name| lists.get_mut(name))
            {
                let used = byte_range(element)?;
                named.enter(used.start, facts);
                if !self.keeps_list(element, &around, source) {
                    let named_scope = self.named_scope(element, &around)?;
                    for &list in &named.around {
                        let binding = &mut facts.bindings[list];
                        if binding.name != used && Some(binding.scope) != named_scope {
                            leave_unknown(binding);
                        }
                    }
                }
            }
            if cursor.goto_first_child() {
                around.push(element);
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    continue 'walk;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
                around.pop();
            }
        }
        Ok(())
    }

    /// The definition `element` names. Its name belongs to the scope around the definition.
    fn named_scope(
        &self,
        element: Node<'_>,
        around: &[Node<'_>],
    ) -> Result<Option<ByteRange>, RiftError> {
        around
            .last()
            .filter(|holder| {
                self.definitions.contains(&holder.kind_id())
                    && holder.child_by_field_id(self.name.get()) == Some(element)
            })
            .map(|holder| byte_range(*holder))
            .transpose()
    }

    fn keeps_list(&self, element: Node<'_>, around: &[Node<'_>], source: &str) -> bool {
        let mut around = around.iter().rev();
        let Some(holder) = around.next() else {
            return false;
        };
        let id = holder.kind_id();
        let is = |field: u16, child: Node<'_>| holder.child_by_field_id(field) == Some(child);
        if id == self.assignment[0] {
            return is(self.left, element);
        }
        if self.assignment[2..].contains(&id) {
            return is(self.right, element);
        }
        if id == self.keyword_argument {
            return is(self.name.get(), element);
        }
        let call = around.next().filter(|call| call.kind_id() == self.call);
        let function = call.and_then(|call| call.child_by_field_id(self.function));
        if id == self.attribute {
            return !is(self.object, element)
                || (function == Some(*holder)
                    && holder
                        .child_by_field_id(self.member)
                        .and_then(|method| source.get(method.byte_range()))
                        .is_some_and(|method| METHODS.contains(&method)));
        }
        id == self.argument_list
            && holder.named_child_count() == 1
            && function.is_some_and(|function| source.get(function.byte_range()) == Some("len"))
    }
}

impl NamedLists {
    /// Moves to `position`, which follows every earlier one.
    fn enter(&mut self, position: u64, facts: &StaticFactsParts) {
        loop {
            while self
                .around
                .last()
                .is_some_and(|&list| facts.bindings[list].scope.end <= position)
            {
                self.around.pop();
            }
            let reached = self
                .ahead
                .last()
                .copied()
                .filter(|&list| facts.bindings[list].scope.start <= position);
            let Some(list) = reached else {
                return;
            };
            self.ahead.pop();
            self.around.push(list);
        }
    }
}

/// The empty lists each name is bound to. Two of one name in one scope stay unknown.
fn empty_lists(source: &str, facts: &mut StaticFactsParts) -> BTreeMap<String, NamedLists> {
    let mut scopes = BTreeMap::<(String, ByteRange), usize>::new();
    for position in 0..facts.bindings.len() {
        let binding = facts.bindings[position];
        if binding.kind != StaticBindingKind::PythonEmptyList {
            continue;
        }
        let Some(name) = text(source, binding.name) else {
            continue;
        };
        if let Some(known) = scopes.insert((name.to_owned(), binding.scope), position) {
            leave_unknown(&mut facts.bindings[known]);
            leave_unknown(&mut facts.bindings[position]);
        }
    }
    let mut lists = BTreeMap::<String, NamedLists>::new();
    // Scopes come in order of their start, an outer scope before the ones it holds.
    let mut ordered = scopes.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|((_, scope), _)| (scope.start, std::cmp::Reverse(scope.end)));
    for ((name, _), position) in ordered.into_iter().rev() {
        lists.entry(name).or_default().ahead.push(position);
    }
    lists
}

fn leave_unknown(binding: &mut StaticBinding) {
    binding.kind = StaticBindingKind::Shadow;
    binding.available_from = binding.scope.start;
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
