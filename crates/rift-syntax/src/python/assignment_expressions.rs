//! Python targets written with an assignment expression.

use std::num::NonZeroU16;
use std::sync::OnceLock;

use tree_sitter::Node;

use super::{DeclarationScope, PythonRules, field_id, kind_id};
use crate::extract::{Declaration, Visited};

pub(crate) const KIND: &str = "python_assignment_expression_binding";

struct AssignmentExpressionKinds {
    named: u16,
    identifier: u16,
    function: u16,
    scope_boundary: [u16; 3],
    parenthesized: u16,
    boolean: u16,
    conditionals: [u16; 2],
    expression_statement: u16,
    name: NonZeroU16,
    body: NonZeroU16,
    left: NonZeroU16,
    condition: NonZeroU16,
}

fn kinds() -> &'static AssignmentExpressionKinds {
    static KINDS: OnceLock<AssignmentExpressionKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = tree_sitter_python::LANGUAGE.into();
        let kind = |name| kind_id(&grammar, name);
        let field = |name| field_id(&grammar, name);
        AssignmentExpressionKinds {
            named: kind("named_expression"),
            identifier: kind("identifier"),
            function: kind("function_definition"),
            scope_boundary: ["lambda", "class_definition", "module"].map(kind),
            parenthesized: kind("parenthesized_expression"),
            boolean: kind("boolean_operator"),
            conditionals: ["if_statement", "while_statement"].map(kind),
            expression_statement: kind("expression_statement"),
            name: field("name"),
            body: field("body"),
            left: field("left"),
            condition: field("condition"),
        }
    })
}

fn named_function(assignment: Node<'_>) -> Option<Node<'_>> {
    let kinds = kinds();
    if assignment.kind_id() != kinds.named || assignment.has_error() {
        return None;
    }
    let mut current = assignment;
    while let Some(parent) = current.parent() {
        if parent.kind_id() == kinds.function {
            let body = parent.child_by_field_id(kinds.body.get())?;
            return (body == current).then_some(parent);
        }
        if kinds.scope_boundary.contains(&parent.kind_id()) {
            break;
        }
        current = parent;
    }
    None
}

pub(super) fn declaration(visited: Visited<'_, '_>, source: &str) -> Option<Declaration> {
    let name = visited.syntax_element();
    let assignment = visited.parent()?.syntax_element();
    let kinds = kinds();
    if assignment.kind_id() != kinds.named
        || name.kind_id() != kinds.identifier
        || assignment.child_by_field_id(kinds.name.get()) != Some(name)
    {
        return None;
    }
    named_function(assignment)?;
    Some(Declaration {
        kind: KIND,
        ..PythonRules::variable(source.get(name.byte_range())?, DeclarationScope::Local)
    })
}

/// A direct function-body statement evaluates this expression before its following reads.
pub(crate) fn direct_owner(assignment: Node<'_>) -> Option<Node<'_>> {
    let owner = named_function(assignment)?;
    let kinds = kinds();
    let body = owner.child_by_field_id(kinds.body.get())?;
    let mut value = assignment;
    loop {
        let parent = value.parent()?;
        let kind = parent.kind_id();
        if kind == kinds.parenthesized
            || (kind == kinds.boolean && parent.child_by_field_id(kinds.left.get()) == Some(value))
        {
            value = parent;
            continue;
        }
        let statement = kind == kinds.expression_statement
            || (kinds.conditionals.contains(&kind)
                && parent.child_by_field_id(kinds.condition.get()) == Some(value));
        return (statement && parent.parent() == Some(body)).then_some(owner);
    }
}

/// Keeps a late target only when its authored scope has no established declaration for it.
pub(super) fn retain(
    symbols: &mut Vec<crate::SyntaxSymbol>,
    facts: &mut crate::StaticFacts,
    source: &str,
) {
    use crate::StaticBindingKind;
    use std::collections::{BTreeMap, BTreeSet};

    if !symbols
        .iter()
        .any(crate::SyntaxSymbol::is_python_assignment_expression_binding)
    {
        return;
    }
    let mut counts = BTreeMap::new();
    for symbol in symbols.iter() {
        *counts
            .entry(symbol.qualified_name.as_str())
            .or_insert(0_usize) += 1;
    }
    let mut scopes = BTreeMap::new();
    let mut redirects = BTreeSet::new();
    for binding in facts.bindings() {
        if matches!(
            binding.kind,
            StaticBindingKind::PythonGlobal | StaticBindingKind::PythonNonlocal
        ) {
            if let Some(name) = binding_text(source, binding.name) {
                redirects.insert((binding.scope, name));
            }
        } else if binding.kind == StaticBindingKind::Shadow {
            scopes.insert(binding.name, binding.scope);
        }
    }
    let withheld = symbols
        .iter()
        .filter(|symbol| symbol.is_python_assignment_expression_binding())
        .filter_map(|symbol| {
            let name = symbol.name_range?;
            let established = counts.get(symbol.qualified_name.as_str()) == Some(&1)
                && scopes
                    .get(&name)
                    .is_some_and(|scope| !redirects.contains(&(*scope, symbol.name.as_str())))
                && rift_core::is_portable_name(&symbol.qualified_name);
            (!established).then_some(name)
        })
        .collect::<BTreeSet<_>>();
    symbols.retain(|symbol| {
        !symbol.is_python_assignment_expression_binding()
            || symbol
                .name_range
                .is_some_and(|name| !withheld.contains(&name))
    });
    facts.retain_python_local_reads(|name| !withheld.contains(&name));
}

fn binding_text(source: &str, range: crate::ByteRange) -> Option<&str> {
    source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
}
