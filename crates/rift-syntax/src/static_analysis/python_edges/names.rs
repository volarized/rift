//! Python names: the targets that bind one, and where one is read or written.

use std::collections::BTreeSet;

use rift_core::ReferenceRole;
use rift_error::RiftError;
use tree_sitter::{Language, Node};

use super::{EdgePythonKinds, edge_enclosing};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};
use crate::static_analysis::{
    PythonDeletion, StaticBinding, StaticBindingKind, StaticFactsParts, StaticReference, kind,
};

/// Syntax elements around a name that state its use.
pub(super) struct NameKinds {
    /// Patterns and displays that hold more than one target.
    containers: [u16; 10],
    as_target: u16,
    as_pattern: u16,
    except: u16,
    delete: u16,
    case: u16,
    /// Patterns of a `match` statement whose plain name is a capture.
    captures: [u16; 4],
    class_pattern: u16,
    splat: u16,
    dotted: u16,
    pub(super) type_alias: u16,
    /// Elements whose `name` field states a name.
    named: [u16; 3],
    /// Elements that hold stated names alone.
    stated: [u16; 2],
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
                id("dictionary_splat_pattern"),
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
            class_pattern: id("class_pattern"),
            splat: id("splat_pattern"),
            dotted: id("dotted_name"),
            type_alias: id("type_alias_statement"),
            named: [
                id("keyword_argument"),
                id("default_parameter"),
                id("typed_default_parameter"),
            ],
            stated: [id("typed_parameter"), id("keyword_pattern")],
        }
    }
}

impl NameKinds {
    /// Whether a pattern of this kind reads the dotted name it holds.
    fn holds_path(&self, pattern: u16) -> bool {
        pattern == self.class_pattern || self.captures.contains(&pattern)
    }
}

/// Python names and syntax checks collected during one walk.
#[derive(Default)]
pub(in crate::static_analysis) struct Names {
    deleted: Vec<StaticBinding>,
    /// The scope of each class body.
    classes: BTreeSet<ByteRange>,
    reads: Vec<ByteRange>,
    /// Written names with the scope that holds the target.
    writes: Vec<(ByteRange, ByteRange, NameUse)>,
    pub(in crate::static_analysis) invalid_type_parameter_default: bool,
}

/// What one name does where it is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameUse {
    Read,
    /// A target of an assignment, a loop or `:=`.
    Assigned,
    /// A target of `with`, `except` or `match`.
    Bound,
    /// A target of `+=` and its like.
    Augmented,
    /// A target of `del`, which makes the name one of its scope.
    Deleted,
    /// A callee, a parameter, part of a member path, or the name that a definition, an import
    /// or a keyword states.
    Stated,
}

impl Names {
    pub(super) fn class(&mut self, scope: ByteRange) {
        self.classes.insert(scope);
    }

    /// Binds each deleted name its scope does not bind otherwise: beside another binding a
    /// `del` statement changes no scope. Keeps the reads of names that the file shares and the
    /// writes of the names that its module and its classes hold.
    pub(in crate::static_analysis) fn close(mut self, source: &str, facts: &mut StaticFactsParts) {
        self.close_deleted(source, facts);
        let module = ByteRange {
            start: 0,
            end: u64::try_from(source.len()).unwrap_or(u64::MAX),
        };
        let held = |scope: &ByteRange| *scope == module || self.classes.contains(scope);
        let mut shared = BTreeSet::new();
        let mut global = BTreeSet::new();
        for binding in &facts.bindings {
            let Some(name) = text(source, binding.name) else {
                continue;
            };
            if binding.kind == StaticBindingKind::PythonGlobal {
                global.insert((binding.scope, name));
            }
            if held(&binding.scope) || shares(binding.kind) {
                shared.insert(name);
            }
        }
        let reads = self.reads.iter().filter(|name| {
            let name = text(source, **name);
            name.is_some_and(|name| shared.contains(name))
        });
        facts
            .references
            .extend(reads.map(|name| reference(*name, ReferenceRole::Read)));
        for (name, scope, name_use) in &self.writes {
            let declared = text(source, *name).is_some_and(|name| global.contains(&(*scope, name)));
            let changed = matches!(name_use, NameUse::Augmented | NameUse::Deleted);
            let written = declared || (changed && held(scope));
            if !written {
                continue;
            }
            if *name_use == NameUse::Augmented {
                facts.references.push(reference(*name, ReferenceRole::Read));
            }
            facts
                .references
                .push(reference(*name, ReferenceRole::Write));
        }
    }

    fn close_deleted(&mut self, source: &str, facts: &mut StaticFactsParts) {
        if self.deleted.is_empty() {
            return;
        }
        let key =
            |binding: &StaticBinding| text(source, binding.name).map(|name| (binding.scope, name));
        let mut bound = facts
            .bindings
            .iter()
            .filter(|binding| !matches!(binding.kind, StaticBindingKind::PythonMemberWrite { .. }))
            .map(key)
            .collect::<BTreeSet<_>>();
        let deleted = self.deleted.drain(..);
        let added = deleted.filter(|binding| bound.insert(key(binding)));
        let added = added.collect::<Vec<_>>();
        facts.bindings.extend(added);
    }
}

/// Whether code outside the scope of a binding of this kind can read what it names.
fn shares(kind: StaticBindingKind) -> bool {
    matches!(
        kind,
        StaticBindingKind::Local
            | StaticBindingKind::PythonFunction { .. }
            | StaticBindingKind::PythonImport { .. }
            | StaticBindingKind::PythonModule { .. }
            | StaticBindingKind::PythonGlobal
    )
}

fn reference(target: ByteRange, role: ReferenceRole) -> StaticReference {
    StaticReference { target, role }
}

fn text(source: &str, range: ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}

impl EdgePythonKinds {
    /// Records what a name outside annotations does.
    pub(super) fn edge_name(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
        names: &mut Names,
    ) -> Result<(), RiftError> {
        let name = visited.syntax_element();
        let range = byte_range(name)?;
        let name_use = self.name_use(visited);
        match name_use {
            NameUse::Stated => return Ok(()),
            NameUse::Read => {
                names.reads.push(range);
                return Ok(());
            }
            NameUse::Assigned | NameUse::Bound | NameUse::Augmented | NameUse::Deleted => {}
        }
        let target = visited.parent().map(Visited::syntax_element);
        let scopes = if target.is_some_and(|target| target.kind_id() == self.named) {
            &self.scopes[..4]
        } else {
            &self.scopes[..]
        };
        let Some(scope) = edge_enclosing(visited, scopes) else {
            return Ok(());
        };
        let binding = self.scoped_binding(name, scope, range.end, StaticBindingKind::Shadow)?;
        if name_use == NameUse::Bound {
            facts.bindings.push(binding);
        } else if name_use == NameUse::Deleted {
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
        names.writes.push((range, binding.scope, name_use));
        Ok(())
    }

    /// Records a member path in a `match` pattern, which reads it.
    pub(super) fn edge_pattern_path(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let path = visited.syntax_element();
        let pattern = visited
            .parent()
            .map(|pattern| pattern.syntax_element().kind_id());
        if path.kind_id() == self.names.dotted
            && path.named_child_count() > 1
            && pattern.is_some_and(|pattern| self.names.holds_path(pattern))
        {
            let path = byte_range(path)?;
            facts.references.push(reference(path, ReferenceRole::Read));
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

    fn name_use(&self, visited: Visited<'_, '_>) -> NameUse {
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
            let at = |field| {
                holder
                    .child_by_field_id(field)
                    .is_some_and(|child| child.id() == element.id())
            };
            return if id == names.delete {
                NameUse::Deleted
            } else if id == names.as_target || (id == names.except && at(self.alias)) {
                NameUse::Bound
            } else if id == self.augmented && at(self.left) {
                NameUse::Augmented
            } else if (self.assignment.contains(&id) && at(self.left))
                || (id == self.named && at(self.name.get()))
            {
                NameUse::Assigned
            } else if names.named.contains(&id) && at(self.name.get())
                || self.parameters.contains(&id)
                || names.stated.contains(&id)
            {
                NameUse::Stated
            } else if element.id() == visited.syntax_element().id() {
                self.own_use(element, parent)
            } else {
                NameUse::Read
            };
        }
        NameUse::Read
    }

    /// The use a name has in the element that holds it directly.
    fn own_use(&self, name: Node<'_>, parent: Visited<'_, '_>) -> NameUse {
        let names = &self.names;
        let holder = parent.syntax_element();
        let id = holder.kind_id();
        let at = |field| {
            holder
                .child_by_field_id(field)
                .is_some_and(|child| child.id() == name.id())
        };
        if id == names.splat {
            return NameUse::Bound;
        }
        if id == names.as_pattern {
            // `case pattern as name`. The expression form keeps its target apart.
            let pattern = holder.named_child(0);
            return if pattern.is_some_and(|pattern| pattern.kind_id() == names.case) {
                NameUse::Bound
            } else {
                NameUse::Read
            };
        }
        if id == names.dotted {
            let captures = holder.named_child_count() == 1;
            let first = holder
                .named_child(0)
                .is_some_and(|first| first.id() == name.id());
            return match parent
                .parent()
                .map(|pattern| pattern.syntax_element().kind_id())
            {
                Some(pattern) if captures && names.captures.contains(&pattern) => NameUse::Bound,
                Some(pattern) if first && names.holds_path(pattern) => NameUse::Read,
                _ => NameUse::Stated,
            };
        }
        let stated = (id == self.call && at(self.function))
            || (self.definitions.contains(&id) && at(self.name.get()))
            || [self.decorator, self.aliased, self.global, self.nonlocal].contains(&id);
        if stated {
            NameUse::Stated
        } else {
            NameUse::Read
        }
    }
}
