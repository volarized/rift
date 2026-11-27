//! Python names: the targets that bind one, and where one is read or written.

use std::collections::{BTreeMap, BTreeSet};

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
    expression_statement: u16,
    parenthesized: u16,
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
            expression_statement: id("expression_statement"),
            parenthesized: id("parenthesized_expression"),
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
pub(in crate::static_analysis) struct Names<'source> {
    deleted: Vec<StaticBinding>,
    /// The scope of each class body.
    classes: BTreeSet<ByteRange>,
    comprehensions: BTreeSet<ByteRange>,
    reads: Vec<ByteRange>,
    direct_locals: BTreeMap<&'source str, BTreeMap<ByteRange, DirectLocal>>,
    loop_locals: BTreeMap<&'source str, BTreeMap<ByteRange, LoopLocal>>,
    receivers: BTreeMap<ByteRange, ByteRange>,
    /// Written names with the scope that holds the target.
    writes: Vec<(ByteRange, ByteRange, NameUse)>,
    pub(in crate::static_analysis) invalid_type_parameter_default: bool,
}

impl<'source> Names<'source> {
    pub(super) fn receiver_binding(&mut self, scope: ByteRange, name: ByteRange) {
        self.receivers.insert(scope, name);
    }

    pub(super) fn loop_binding(
        &mut self,
        source: &'source str,
        name: ByteRange,
        scope: ByteRange,
        body: ByteRange,
    ) {
        if let Some(spelling) = text(source, name) {
            self.loop_locals
                .entry(spelling)
                .or_default()
                .entry(scope)
                .or_insert(LoopLocal { name, body });
        }
    }

    pub(super) fn direct_binding(
        &mut self,
        source: &'source str,
        name: ByteRange,
        scope: ByteRange,
        end: u64,
        kind: DirectBindingKind,
    ) {
        if let Some(spelling) = text(source, name) {
            self.direct_locals
                .entry(spelling)
                .or_default()
                .entry(scope)
                .or_insert(DirectLocal { name, end, kind });
        }
    }
}

/// A loop target and the written suite where it already has its item.
struct LoopLocal {
    name: ByteRange,
    body: ByteRange,
}

/// One parameter or initialized name directly inside a function body.
struct DirectLocal {
    name: ByteRange,
    end: u64,
    kind: DirectBindingKind,
}

/// The written binding a function-local read could select.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DirectBindingKind {
    Local,
    Parameter,
    VariadicParameter,
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

impl Names<'_> {
    pub(super) fn class(&mut self, scope: ByteRange) {
        self.classes.insert(scope);
    }

    pub(super) fn comprehension(&mut self, scope: ByteRange) {
        self.comprehensions.insert(scope);
    }

    /// Binds each deleted name its scope does not bind otherwise: beside another binding a
    /// `del` statement changes no scope. Keeps the reads of names that the file shares and the
    /// writes of the names that its module and its classes hold.
    pub(in crate::static_analysis) fn close(mut self, source: &str, facts: &mut StaticFactsParts) {
        self.close_deleted(source, facts);
        self.close_comprehensions(source, facts);
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

    fn close_comprehensions(&self, source: &str, facts: &mut StaticFactsParts) {
        if self.comprehensions.is_empty() {
            return;
        }
        let mut scopes = BTreeMap::<&str, Vec<ByteRange>>::new();
        for binding in &facts.bindings {
            if self.comprehensions.contains(&binding.scope)
                && let Some(name) = text(source, binding.name)
            {
                scopes.entry(name).or_default().push(binding.scope);
            }
        }
        // A name's overlapping candidate regions form a union. The resolver still
        // chooses its exact lexical scope and checks the first iterable.
        for regions in scopes.values_mut() {
            regions.sort_unstable();
            let mut end = 0;
            for region in regions {
                end = end.max(region.end);
                region.end = end;
            }
        }
        facts
            .python_comprehension_reads
            .extend(self.reads.iter().copied().filter(|&read| {
                let Some(regions) = text(source, read).and_then(|name| scopes.get(name)) else {
                    return false;
                };
                let before = regions.partition_point(|scope| scope.start <= read.start);
                before > 0 && regions[before - 1].encloses(read)
            }));
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
    /// Retains the initialized name of a plain assignment directly in a function's body.
    pub(super) fn edge_direct_local<'source>(
        &self,
        visited: Visited<'_, '_>,
        target: Node<'_>,
        source: &'source str,
        names: &mut Names<'source>,
    ) -> Result<(), RiftError> {
        let assignment = visited.syntax_element();
        if assignment.kind_id() != self.assignment[0]
            || target.kind_id() != self.identifier
            || assignment.has_error()
            || assignment.child_by_field_id(self.right).is_none()
        {
            return Ok(());
        }
        let Some(statement) = visited
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.names.expression_statement)
        else {
            return Ok(());
        };
        if let Some(owner) = statement
            .parent()
            .filter(|parent| parent.syntax_element().kind_id() == self.block)
            .and_then(Visited::parent)
            .filter(|parent| parent.syntax_element().kind_id() == self.definitions[0])
        {
            names.direct_binding(
                source,
                byte_range(target)?,
                self.scope_range(owner.syntax_element())?,
                byte_range(assignment)?.end,
                DirectBindingKind::Local,
            );
        }
        Ok(())
    }

    /// A plain value is read in its nearest function, outside a class or an expression scope.
    fn direct_local_read_scope<'tree>(&self, visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
        let mut value = visited;
        while let Some(parent) = value.parent() {
            let node = parent.syntax_element();
            if node.kind_id() == self.names.parenthesized {
                value = parent;
                continue;
            }
            let callee = node.kind_id() == self.call
                && node.child_by_field_id(self.function) == Some(value.syntax_element());
            if callee || node.kind_id() == self.attribute {
                return None;
            }
            break;
        }
        self.function_body_scope(visited)
    }

    fn function_body_scope<'tree>(&self, visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
        let scope = edge_enclosing(visited, &self.scopes)?;
        let body = scope.child_by_field_id(self.body)?;
        (scope.kind_id() == self.definitions[0]
            && byte_range(body)
                .ok()?
                .encloses(byte_range(visited.syntax_element()).ok()?))
        .then_some(scope)
    }

    /// A named function declared directly in another function's authored body.
    fn enclosing_local_function<'tree>(&self, scope: Node<'tree>) -> Option<Node<'tree>> {
        let block = scope
            .parent()
            .filter(|parent| parent.kind_id() == self.block)?;
        let outer = block.parent()?;
        (outer.kind_id() == self.definitions[0]
            && outer.child_by_field_id(self.body) == Some(block))
        .then_some(outer)
    }

    fn edge_local_read(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        range: ByteRange,
        facts: &mut StaticFactsParts,
        names: &Names<'_>,
    ) -> Result<(), RiftError> {
        let Some(spelling) = text(source, range) else {
            return Ok(());
        };
        let assignments = names.direct_locals.get(spelling);
        let loops = names.loop_locals.get(spelling);
        if assignments.is_none() && loops.is_none() {
            return Ok(());
        }
        let Some(scope) = self.direct_local_read_scope(visited) else {
            return Ok(());
        };
        let inner = byte_range(scope)?;
        if let Some(local) = loops.and_then(|loops| loops.get(&inner))
            && local.body.encloses(range)
        {
            facts.python_local_reads.push((range, local.name));
        }
        let Some(assignments) = assignments else {
            return Ok(());
        };
        if let Some(local) = assignments.get(&inner) {
            if local.end <= range.start {
                let reads = if local.kind == DirectBindingKind::VariadicParameter {
                    &mut facts.python_variadic_reads
                } else {
                    &mut facts.python_local_reads
                };
                reads.push((range, local.name));
            }
        } else if let Some(outer) = self.enclosing_local_function(scope)
            && let Some(local) = assignments.get(&byte_range(outer)?)
            && local.kind == DirectBindingKind::Local
            && local.end <= inner.start
        {
            facts.python_captured_local_reads.push((range, local.name));
        }
        Ok(())
    }

    fn edge_receiver_read(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        read: ByteRange,
        facts: &mut StaticFactsParts,
        names: &Names<'_>,
    ) -> Result<(), RiftError> {
        if names.receivers.is_empty() {
            return Ok(());
        }
        if let Some(scope) = self.function_body_scope(visited)
            && let Some(&name) = names.receivers.get(&byte_range(scope)?)
            && text(source, read) == text(source, name)
        {
            facts.python_receiver_reads.push((read, name));
        }
        Ok(())
    }

    /// Records what a name outside annotations does.
    pub(super) fn edge_name(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
        names: &mut Names<'_>,
    ) -> Result<(), RiftError> {
        let name = visited.syntax_element();
        let range = byte_range(name)?;
        let name_use = self.name_use(visited);
        let callee = name_use == NameUse::Stated
            && visited.parent().is_some_and(|parent| {
                let parent = parent.syntax_element();
                parent.kind_id() == self.call
                    && parent.child_by_field_id(self.function) == Some(name)
            });
        if name_use == NameUse::Read || callee {
            self.edge_receiver_read(visited, source, range, facts, names)?;
        }
        match name_use {
            NameUse::Stated => return Ok(()),
            NameUse::Read => {
                names.reads.push(range);
                return self.edge_local_read(visited, source, range, facts, names);
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
        let alias = visited.syntax_element();
        let Some((name, parameters)) = crate::python::type_alias_parts(alias) else {
            return Ok(());
        };
        if let Some(parameters) = parameters {
            self.edge_type_parameters(alias, parameters, facts)?;
        }
        if let Some(scope) = edge_enclosing(visited, &self.scopes) {
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
