use super::{EcmaScriptGrammarField, EcmaScriptKinds, ExportTarget, named_children};
use crate::{SyntaxExportBinding, SyntaxExportKind, SyntaxLimits, SyntaxSource, extract};
use rift_error::{RiftError, errors};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

pub(super) fn record(
    root: Node<'_>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    kinds: &EcmaScriptKinds,
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<(), RiftError> {
    let mut pending = vec![(root, 0_usize)];
    let mut blocked = BTreeSet::new();
    let mut replaced = BTreeSet::new();
    let mut assignments = Vec::new();
    let mut visited = 0_usize;
    let mut cursor = root.walk();
    while let Some((node, depth)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        if visited + pending.len() > limits.syntax_nodes_max() {
            return errors::syntax::too_many_nodes()
                .path(source.path)
                .syntax_nodes_max(limits.syntax_nodes_max())
                .fail();
        }
        if blocked_binding(node, source.text)
            && let Some(scope) = lexical_scope(node)
        {
            blocked.insert(scope.id());
        }
        if node.kind_id() == kinds.assignment_expression {
            assignments.push(node);
            if node
                .child_by_field_name("left")
                .and_then(|left| source.text.get(left.byte_range()))
                == Some("module.exports")
                && let Some(scope) = lexical_scope(node)
            {
                replaced.insert(scope.id());
            }
        }
        if visited
            .saturating_add(pending.len())
            .saturating_add(node.named_child_count())
            > limits.syntax_nodes_max()
        {
            return errors::syntax::too_many_nodes()
                .path(source.path)
                .syntax_nodes_max(limits.syntax_nodes_max())
                .fail();
        }
        extract::push_named_children(&mut pending, &mut cursor, node, |child| (child, depth + 1));
    }
    for node in assignments {
        if !module_execution(node, &blocked) {
            continue;
        }
        let Some(binding) = assignment(node, source.text, kinds)? else {
            continue;
        };
        if binding.kind == SyntaxExportKind::Named && !module_execution(node, &replaced) {
            continue;
        }
        bindings.insert(node.id(), binding);
    }
    Ok(())
}

fn receiver(text: &str) -> bool {
    matches!(text, "exports" | "module" | "require" | "eval")
}

fn binding_contains(node: Node<'_>, source: &str) -> bool {
    let mut pending = vec![node];
    let mut cursor = node.walk();
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && source.get(node.byte_range()).is_some_and(receiver)
        {
            return true;
        }
        extract::push_named_children(&mut pending, &mut cursor, node, |child| child);
    }
    false
}

fn blocked_binding(node: Node<'_>, source: &str) -> bool {
    let binding = match node.kind() {
        "variable_declarator" | "function_declaration" | "class_declaration" => {
            node.child_by_field_name("name")
        }
        "formal_parameters" | "import_clause" | "catch_clause" => Some(node),
        "assignment_expression" | "augmented_assignment_expression" => node
            .child_by_field_name("left")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "update_expression" => node
            .child_by_field_name("argument")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "call_expression" => {
            return node
                .child_by_field_name("function")
                .and_then(|function| source.get(function.byte_range()))
                == Some("eval");
        }
        _ => None,
    };
    binding.is_some_and(|binding| binding_contains(binding, source))
}

fn lexical_scope(mut node: Node<'_>) -> Option<Node<'_>> {
    let function_binding = matches!(
        node.kind(),
        "formal_parameters"
            | "variable_declarator"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "update_expression"
            | "call_expression"
            | "function_declaration"
    );
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "program"
                | "function_expression"
                | "function_declaration"
                | "arrow_function"
                | "generator_function"
                | "generator_function_declaration"
        ) || (!function_binding && parent.kind() == "statement_block")
        {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn immediate_function(function: Node<'_>) -> bool {
    if function.kind() != "function_expression"
        || function.child_by_field_name("name").is_some()
        || function
            .child_by_field_name("parameters")
            .is_none_or(|parameters| parameters.named_child_count() != 0)
    {
        return false;
    }
    let mut called = function;
    while let Some(parent) = called.parent().filter(|parent| {
        parent.kind() == "parenthesized_expression" && parent.named_child_count() == 1
    }) {
        called = parent;
    }
    called.parent().is_some_and(|call| {
        call.kind() == "call_expression"
            && call.child_by_field_name("function") == Some(called)
            && call
                .child_by_field_name("arguments")
                .is_some_and(|arguments| arguments.named_child_count() == 0)
    })
}

fn module_execution(mut node: Node<'_>, blocked: &BTreeSet<usize>) -> bool {
    while let Some(parent) = node.parent() {
        if blocked.contains(&parent.id()) {
            return false;
        }
        match parent.kind() {
            "program" => return true,
            "function_expression" if immediate_function(parent) => {}
            "expression_statement"
            | "statement_block"
            | "if_statement"
            | "else_clause"
            | "binary_expression"
            | "parenthesized_expression"
            | "call_expression" => {}
            _ => return false,
        }
        node = parent;
    }
    false
}

fn assignment(
    node: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Result<Option<SyntaxExportBinding>, RiftError> {
    let Some(left) = kinds.child(node, EcmaScriptGrammarField::Left) else {
        return Ok(None);
    };
    let Some(target) = kinds.export_target(left, source) else {
        return Ok(None);
    };
    let range = extract::byte_range(node)?;
    if target == ExportTarget::Property {
        let Some((property, _)) = kinds.export_function(node, source) else {
            return Ok(None);
        };
        let name = extract::byte_range(property)?;
        return Ok(Some(SyntaxExportBinding {
            kind: SyntaxExportKind::Named,
            range,
            local: Some(name),
            exported: Some(name),
            source: None,
            container: None,
            type_only: false,
        }));
    }
    let Some(value) = kinds.child(node, EcmaScriptGrammarField::Right) else {
        return Ok(None);
    };
    if value.kind() != "call_expression"
        || value
            .child_by_field_name("function")
            .and_then(|function| source.get(function.byte_range()))
            != Some("require")
    {
        return Ok(None);
    }
    let Some(arguments) = value.child_by_field_name("arguments") else {
        return Ok(None);
    };
    let mut arguments = named_children(arguments);
    let Some(argument) = arguments
        .next()
        .filter(|argument| argument.kind() == "string")
    else {
        return Ok(None);
    };
    if arguments.next().is_some() {
        return Ok(None);
    }
    Ok(Some(SyntaxExportBinding {
        kind: SyntaxExportKind::All,
        range,
        local: None,
        exported: None,
        source: Some(extract::byte_range(argument)?),
        container: None,
        type_only: false,
    }))
}
