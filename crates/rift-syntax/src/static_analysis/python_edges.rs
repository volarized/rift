//! Captured Python call sites and lexical import bindings.

mod guards;

use std::sync::OnceLock;

use rift_error::RiftError;
use tree_sitter::Node;

use super::{
    StaticBindingKind, StaticCallee, StaticFactsParts, StaticReference, binding, call, field,
    identifier, kind,
};
use crate::ByteRange;
use crate::extract::{Visited, byte_range};

pub(super) struct EdgePythonKinds {
    identifier: u16,
    none: u16,
    call: u16,
    attribute: u16,
    definitions: [u16; 2],
    scopes: [u16; 8],
    patterns: [u16; 5],
    annotation: u16,
    member_type: u16,
    string_content: u16,
    superclasses: u16,
    augmented: u16,
    named: u16,
    decorated: u16,
    decorator: u16,
    imports: [u16; 2],
    aliased: u16,
    parameters: [u16; 2],
    assignment: [u16; 4],
    unsupported: u16,
    global: u16,
    nonlocal: u16,
    name: std::num::NonZeroU16,
    alias: u16,
    module: u16,
    function: u16,
    object: u16,
    member: u16,
    type_field: u16,
    left: u16,
    right: u16,
    guards: guards::TypeGuards,
    return_type: u16,
    loops: [u16; 2],
    /// Compound statements and clauses a class body may define methods under.
    conditional: [u16; 6],
    block: u16,
}

pub(super) fn edge_python_kinds() -> &'static EdgePythonKinds {
    static KINDS: OnceLock<EdgePythonKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = tree_sitter_python::LANGUAGE.into();
        let id = |name| kind(&grammar, name);
        let field = |name| field(&grammar, name);
        let definitions = [id("function_definition"), id("class_definition")];
        EdgePythonKinds {
            identifier: id("identifier"),
            none: id("none"),
            call: id("call"),
            attribute: id("attribute"),
            definitions,
            scopes: [
                id("module"),
                definitions[0],
                definitions[1],
                id("lambda"),
                id("list_comprehension"),
                id("dictionary_comprehension"),
                id("set_comprehension"),
                id("generator_expression"),
            ],
            patterns: [
                id("pattern_list"),
                id("tuple_pattern"),
                id("list_pattern"),
                id("list_splat_pattern"),
                id("dictionary_splat_pattern"),
            ],
            annotation: id("type"),
            member_type: id("member_type"),
            string_content: id("string_content"),
            superclasses: field("superclasses"),
            augmented: id("augmented_assignment"),
            named: id("named_expression"),
            decorated: id("decorated_definition"),
            decorator: id("decorator"),
            imports: [id("import_statement"), id("import_from_statement")],
            aliased: id("aliased_import"),
            parameters: [id("parameters"), id("lambda_parameters")],
            assignment: [
                id("assignment"),
                id("augmented_assignment"),
                id("for_statement"),
                id("for_in_clause"),
            ],
            unsupported: id("wildcard_import"),
            global: id("global_statement"),
            nonlocal: id("nonlocal_statement"),
            name: grammar
                .field_id_for_name("name")
                .unwrap_or_else(|| panic!("the pinned Python grammar must define field name")),
            alias: field("alias"),
            module: field("module_name"),
            function: field("function"),
            object: field("object"),
            member: field("attribute"),
            type_field: field("type"),
            left: field("left"),
            right: field("right"),
            guards: guards::TypeGuards::new(&grammar),
            return_type: field("return_type"),
            loops: [id("for_statement"), id("while_statement")],
            conditional: [
                id("if_statement"),
                id("elif_clause"),
                id("else_clause"),
                id("try_statement"),
                id("except_clause"),
                id("finally_clause"),
            ],
            block: id("block"),
        }
    })
}

impl EdgePythonKinds {
    pub(super) fn record_edge(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let id = syntax_element.kind_id();
        if self.loops.contains(&id) {
            facts.loops.push(byte_range(syntax_element)?);
        } else if self.scopes[3..].contains(&id) {
            facts.expression_scopes.push(byte_range(syntax_element)?);
        }
        if id == self.call {
            if let Some(callee) = syntax_element.child_by_field_id(self.function) {
                facts
                    .calls
                    .push(call(syntax_element, callee, self.edge_callee(callee)?)?);
            }
        } else if id == self.decorator {
            // A decorator written without arguments is called with the decorated definition.
            if let Some(expression) = syntax_element
                .named_children(&mut syntax_element.walk())
                .find(|element| !element.is_extra())
                .filter(|expression| {
                    [self.identifier, self.attribute].contains(&expression.kind_id())
                })
            {
                facts
                    .calls
                    .push(call(expression, expression, self.edge_callee(expression)?)?);
            }
        } else if self.definitions.contains(&id) {
            self.edge_definition(visited, facts)?;
            self.edge_type_parameters(syntax_element, facts)?;
        } else if self.imports.contains(&id) {
            self.edge_import(visited, facts)?;
        } else if self.parameters.contains(&id) {
            self.edge_parameters(visited, source, facts)?;
        } else if self.assignment.contains(&id) {
            if let Some(target) = syntax_element.child_by_field_id(self.left) {
                self.edge_shadow(target, visited, facts)?;
            }
        } else if id == self.named {
            if let Some(name) = syntax_element.child_by_field_id(self.name.get()) {
                self.edge_shadow(name, visited, facts)?;
            }
        } else if (id == self.global || id == self.nonlocal)
            && let Some(scope) = edge_enclosing(visited, &self.scopes)
        {
            if scope.kind_id() != self.scopes[0] {
                let kind = if id == self.global {
                    StaticBindingKind::PythonGlobal
                } else {
                    StaticBindingKind::PythonNonlocal
                };
                let mut cursor = syntax_element.walk();
                for name in syntax_element
                    .named_children(&mut cursor)
                    .filter(|name| name.kind_id() == self.identifier)
                {
                    facts
                        .bindings
                        .push(binding(name, scope, byte_range(scope)?.start, kind)?);
                }
            }
        } else if id == self.unsupported
            && let Some(scope) = edge_enclosing(visited, &self.scopes)
        {
            facts.unsupported_bindings.push(byte_range(scope)?);
            if let Some(module) = visited
                .parent()
                .and_then(|parent| parent.syntax_element().child_by_field_id(self.module))
            {
                facts.bindings.push(binding(
                    syntax_element,
                    scope,
                    byte_range(scope)?.start,
                    StaticBindingKind::PythonImport {
                        module: byte_range(module)?,
                        export: None,
                    },
                )?);
            }
        }
        self.guards.record(visited, source, facts)?;
        self.edge_reference(visited, source, facts)?;
        Ok(())
    }

    fn edge_type_parameters(
        &self,
        definition: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let Some(parameters) = definition.child_by_field_name("type_parameters") else {
            return Ok(());
        };
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            let mut candidate = Some(parameter);
            let mut name = None;
            // A declared name sits below `type`, `constrained_type` or `splat_type` wrappers.
            for _ in 0..4 {
                let Some(element) = candidate else {
                    break;
                };
                if element.kind_id() == self.identifier {
                    name = Some(element);
                    break;
                }
                candidate = element.named_child(0);
            }
            if let Some(name) = name {
                facts.bindings.push(binding(
                    name,
                    definition,
                    byte_range(definition)?.start,
                    StaticBindingKind::Shadow,
                )?);
            } else {
                facts.unsupported_bindings.push(byte_range(definition)?);
            }
        }
        Ok(())
    }

    fn edge_callee(&self, callee: Node<'_>) -> Result<StaticCallee, RiftError> {
        if callee.kind_id() == self.identifier {
            return Ok(StaticCallee::Direct);
        }
        if callee.kind_id() == self.attribute {
            let receiver = callee.child_by_field_id(self.object).filter(|value| {
                [self.identifier, self.attribute, self.call].contains(&value.kind_id())
            });
            let member = callee
                .child_by_field_id(self.member)
                .and_then(|value| identifier(value, self.identifier));
            if let Some((receiver, member)) = receiver.zip(member) {
                return Ok(StaticCallee::Member {
                    receiver: byte_range(receiver)?,
                    member: byte_range(member)?,
                });
            }
        }
        Ok(StaticCallee::Unsupported)
    }

    fn edge_definition(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let name = syntax_element.child_by_field_id(self.name.get());
        let scope = edge_enclosing(visited, &self.scopes);
        if let Some((name, scope)) = name.zip(scope) {
            let annotation = syntax_element.child_by_field_id(self.return_type);
            let kind = match annotation {
                Some(annotation) if syntax_element.kind_id() == self.definitions[0] => {
                    StaticBindingKind::PythonFunction {
                        annotation: byte_range(annotation)?,
                        decorator: self
                            .edge_function_decorator(visited)
                            .map(byte_range)
                            .transpose()?,
                    }
                }
                _ => StaticBindingKind::Local,
            };
            facts
                .bindings
                .push(binding(name, scope, byte_range(scope)?.start, kind)?);
        }
        Ok(())
    }

    fn edge_function_decorator<'tree>(&self, visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
        let decorated = visited.parent()?.syntax_element();
        if decorated.kind_id() != self.decorated {
            return None;
        }
        let mut cursor = decorated.walk();
        let mut decorators = decorated
            .named_children(&mut cursor)
            .filter(|element| element.kind_id() == self.decorator);
        let expression = decorators.next()?.named_child(0)?;
        (decorators.next().is_none() && expression.kind_id() == self.identifier)
            .then_some(expression)
    }

    fn edge_import(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = edge_enclosing(visited, &self.scopes) else {
            return Ok(());
        };
        let module = syntax_element.child_by_field_id(self.module);
        let mut cursor = syntax_element.walk();
        for imported in syntax_element.children_by_field_id(self.name, &mut cursor) {
            let (name, alias) = if imported.kind_id() == self.aliased {
                (
                    imported.child_by_field_id(self.name.get()),
                    imported.child_by_field_id(self.alias),
                )
            } else {
                (Some(imported), None)
            };
            let Some(name) = name else {
                continue;
            };
            let local = alias.or_else(|| name.named_child(0)).unwrap_or(name);
            let (module, export) = match module {
                Some(module) => (module, Some(byte_range(name)?)),
                None => (name, None),
            };
            let kind = if export.is_none() && alias.is_none() && name.named_child_count() > 1 {
                StaticBindingKind::PythonModule {
                    module: byte_range(module)?,
                }
            } else {
                StaticBindingKind::PythonImport {
                    module: byte_range(module)?,
                    export,
                }
            };
            facts
                .bindings
                .push(binding(local, scope, byte_range(scope)?.start, kind)?);
        }
        Ok(())
    }

    fn edge_parameters(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        let Some(scope) = visited.parent().map(Visited::syntax_element) else {
            return Ok(());
        };
        let class_name = self.edge_receiver_class(visited, source);
        let mut cursor = syntax_element.walk();
        let mut first = true;
        for parameter in syntax_element.named_children(&mut cursor) {
            if parameter.is_extra() {
                continue;
            }
            let name = if parameter.kind_id() == self.identifier {
                Some(parameter)
            } else {
                parameter
                    .child_by_field_id(self.name.get())
                    .or_else(|| parameter.named_child(0))
            };
            if let Some(name) = name.filter(|name| name.kind_id() == self.identifier) {
                let annotated = parameter
                    .child_by_field_id(self.type_field)
                    .map(byte_range)
                    .transpose()?;
                facts.bindings.push(binding(
                    name,
                    scope,
                    byte_range(scope)?.start,
                    match class_name
                        .filter(|_| first && !self.patterns.contains(&parameter.kind_id()))
                    {
                        Some(class_name) => StaticBindingKind::PythonReceiver {
                            class_name: byte_range(class_name)?,
                        },
                        None => annotated.map_or(StaticBindingKind::Shadow, |annotation| {
                            StaticBindingKind::PythonAnnotation { annotation }
                        }),
                    },
                )?);
            }
            first = false;
        }
        Ok(())
    }

    fn edge_receiver_class<'tree>(
        &self,
        visited: Visited<'_, 'tree>,
        source: &str,
    ) -> Option<Node<'tree>> {
        let function = visited.parent()?;
        if function.syntax_element().kind_id() != self.definitions[0] {
            return None;
        }
        let mut parent = function.parent()?;
        if parent.syntax_element().kind_id() == self.decorated {
            let element = parent.syntax_element();
            let mut cursor = element.walk();
            for decorator in element
                .named_children(&mut cursor)
                .filter(|item| item.kind_id() == self.decorator)
            {
                let expression = decorator.named_child(0)?;
                if source.get(expression.byte_range())?.rsplit('.').next() == Some("staticmethod") {
                    return None;
                }
            }
            parent = parent.parent()?;
        }
        // A method defined under `if`, `try` and their clauses still belongs to the class body.
        let mut owner = parent.parent()?;
        while self.conditional.contains(&owner.syntax_element().kind_id()) {
            owner = owner.parent()?;
            if owner.syntax_element().kind_id() == self.block {
                owner = owner.parent()?;
            }
        }
        let class = owner.syntax_element();
        (class.kind_id() == self.definitions[1])
            .then(|| class.child_by_field_id(self.name.get()))
            .flatten()
    }

    fn edge_reference(
        &self,
        visited: Visited<'_, '_>,
        source: &str,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        use rift_core::ReferenceRole;
        let element = visited.syntax_element();
        let id = element.kind_id();
        if ![
            self.identifier,
            self.attribute,
            self.member_type,
            self.string_content,
        ]
        .contains(&id)
        {
            return Ok(());
        }
        let parent = visited.parent().map(Visited::syntax_element);
        if parent.is_some_and(|parent| {
            (parent.kind_id() == self.attribute && id == self.identifier)
                || parent.kind_id() == self.member_type
        }) || visited
            .parent()
            .and_then(Visited::parent)
            .is_some_and(|parent| parent.syntax_element().kind_id() == self.member_type)
        {
            return Ok(());
        }
        let mut ancestor = visited.parent();
        let mut annotation = false;
        while let Some(current) = ancestor {
            let item = current.syntax_element();
            if item.kind_id() == self.annotation {
                annotation = true;
            }
            if id == self.string_content
                && let Some(base) = item.named_child(0)
                && source
                    .get(base.byte_range())
                    .is_some_and(|name| name.rsplit('.').next() == Some("Literal"))
            {
                return Ok(());
            }
            if item.kind_id() == self.definitions[1]
                && item
                    .child_by_field_id(self.superclasses)
                    .is_some_and(|bases| bases.byte_range().contains(&element.start_byte()))
            {
                annotation = true;
            }
            if item.kind_id() == self.call {
                annotation = false;
                break;
            }
            if self.scopes.contains(&item.kind_id()) {
                break;
            }
            ancestor = current.parent();
        }
        let range = byte_range(element)?;
        if annotation {
            facts.references.push(StaticReference {
                target: range,
                role: ReferenceRole::Type,
            });
        } else if id == self.attribute {
            let mut target_element = element;
            let mut ancestor = visited.parent();
            while let Some(current) = ancestor {
                if !self.patterns.contains(&current.syntax_element().kind_id()) {
                    break;
                }
                target_element = current.syntax_element();
                ancestor = current.parent();
            }
            let assignment = ancestor
                .map(Visited::syntax_element)
                .filter(|parent| self.assignment.contains(&parent.kind_id()));
            let target = assignment.and_then(|parent| parent.child_by_field_id(self.left));
            let written = target.is_some_and(|target| target.id() == target_element.id());
            if written
                && assignment.is_some_and(|parent| {
                    parent.kind_id() == self.assignment[0]
                        && parent.child_by_field_id(self.right).is_none()
                })
            {
                return Ok(());
            }
            if !written || assignment.is_some_and(|parent| parent.kind_id() == self.augmented) {
                facts.references.push(StaticReference {
                    target: range,
                    role: ReferenceRole::Read,
                });
            }
            if written {
                facts.references.push(StaticReference {
                    target: range,
                    role: ReferenceRole::Write,
                });
            }
        }
        Ok(())
    }

    fn edge_shadow(
        &self,
        target: Node<'_>,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let scopes = if visited.syntax_element().kind_id() == self.named {
            &self.scopes[..4]
        } else {
            &self.scopes
        };
        let Some(scope) = edge_enclosing(visited, scopes) else {
            return Ok(());
        };
        let mut pending = vec![target];
        while let Some(target) = pending.pop() {
            if target.kind_id() == self.identifier {
                let kind = self.edge_assignment_kind(visited, target)?;
                let available = match kind {
                    StaticBindingKind::PythonInstance { .. }
                    | StaticBindingKind::PythonAlias { .. }
                    | StaticBindingKind::PythonNone => byte_range(visited.syntax_element())?.end,
                    // The loop body reads each element after the iterated value.
                    StaticBindingKind::PythonElement { iterable } => iterable.end,
                    _ => byte_range(scope)?.start,
                };
                facts
                    .bindings
                    .push(binding(target, scope, available, kind)?);
            } else if target.kind_id() == self.attribute {
                facts.assignments.push(byte_range(target)?);
                if let Some((receiver, member)) = target
                    .child_by_field_id(self.object)
                    .zip(target.child_by_field_id(self.member))
                {
                    facts.bindings.push(binding(
                        member,
                        scope,
                        byte_range(visited.syntax_element())?.end,
                        StaticBindingKind::PythonMemberWrite {
                            receiver: byte_range(receiver)?,
                        },
                    )?);
                }
                let kind = self.edge_assignment_kind(visited, target)?;
                if kind != StaticBindingKind::Shadow {
                    facts
                        .bindings
                        .push(binding(target, scope, byte_range(scope)?.start, kind)?);
                }
            } else if self.patterns.contains(&target.kind_id()) {
                let mut cursor = target.walk();
                pending.extend(target.named_children(&mut cursor));
            }
        }
        Ok(())
    }

    fn edge_assignment_kind(
        &self,
        visited: Visited<'_, '_>,
        target: Node<'_>,
    ) -> Result<StaticBindingKind, RiftError> {
        let assignment = visited.syntax_element();
        if assignment.kind_id() == self.assignment[2]
            && target.kind_id() == self.identifier
            && assignment.child_by_field_id(self.left) == Some(target)
            && let Some(iterable) = assignment.child_by_field_id(self.right)
        {
            return Ok(StaticBindingKind::PythonElement {
                iterable: byte_range(iterable)?,
            });
        }
        if assignment.kind_id() != self.assignment[0]
            || assignment.child_by_field_id(self.left) != Some(target)
        {
            return Ok(StaticBindingKind::Shadow);
        }
        if let Some(annotation) = assignment.child_by_field_id(self.type_field) {
            return Ok(StaticBindingKind::PythonAnnotation {
                annotation: byte_range(annotation)?,
            });
        }
        let mut assigned = assignment.child_by_field_id(self.right);
        // Chained assignments have one value. Each step descends within the admitted tree.
        while let Some(next) = assigned.filter(|value| value.kind_id() == self.assignment[0]) {
            assigned = next.child_by_field_id(self.right);
        }
        if assigned.is_some_and(|value| value.kind_id() == self.none && !value.is_missing()) {
            return Ok(StaticBindingKind::PythonNone);
        }
        let constructor = assignment
            .child_by_field_id(self.right)
            .filter(|value| value.kind_id() == self.call)
            .and_then(|call| call.child_by_field_id(self.function))
            .filter(|callee| [self.identifier, self.attribute].contains(&callee.kind_id()));
        match constructor {
            Some(constructor) => Ok(StaticBindingKind::PythonInstance {
                constructor: byte_range(constructor)?,
            }),
            None => match assignment.child_by_field_id(self.right) {
                Some(value) if [self.identifier, self.attribute].contains(&value.kind_id()) => {
                    Ok(StaticBindingKind::PythonAlias {
                        value: byte_range(value)?,
                    })
                }
                _ => Ok(StaticBindingKind::Shadow),
            },
        }
    }
}

/// Gives the bindings of a module the whole file as their scope. The syntax root, `root`, leaves
/// out the spaces, line ends and byte order mark the file starts with.
pub(super) fn widen_module_scope(facts: &mut StaticFactsParts, root: ByteRange, file: ByteRange) {
    if root == file {
        return;
    }
    for binding in &mut facts.bindings {
        if binding.scope == root {
            binding.scope = file;
            if binding.available_from == root.start {
                binding.available_from = file.start;
            }
        }
    }
    for scope in &mut facts.unsupported_bindings {
        if *scope == root {
            *scope = file;
        }
    }
}

fn edge_enclosing<'tree>(visited: Visited<'_, 'tree>, kinds: &[u16]) -> Option<Node<'tree>> {
    let mut parent = visited.parent();
    while let Some(candidate) = parent {
        let syntax_element = candidate.syntax_element();
        if kinds.contains(&syntax_element.kind_id()) {
            return Some(syntax_element);
        }
        parent = candidate.parent();
    }
    None
}
