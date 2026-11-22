use super::{EcmaScriptGrammarField, EcmaScriptKinds, ExportTarget, named_children};
use crate::{
    StaticCommonJsExport, SyntaxExportBinding, SyntaxExportKind, SyntaxLimits, SyntaxSource,
    extract,
};
use rift_error::{RiftError, errors};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

pub(super) fn record<'tree>(
    root: Node<'tree>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    kinds: &EcmaScriptKinds,
    bindings: &mut BTreeMap<usize, SyntaxExportBinding>,
) -> Result<(Vec<Node<'tree>>, Option<StaticCommonJsExport>), RiftError> {
    let CapturedAssignments {
        assignments,
        mut blocked,
        replaced,
        synchronized,
        direct_default,
    } = capture_assignments(root, source, limits, kinds)?;
    let synchronized = synchronized.and_then(|(outer, inner)| {
        if !blocked.contains(&root.id())
            && closed_wrapper_uses(root, Some(outer), inner, source.text, kinds)
        {
            Some(inner)
        } else {
            blocked.insert(root.id());
            None
        }
    });
    let mut closed_export = None;
    let synchronized_scope = synchronized.map(|inner| (root.id(), inner.end_byte()));
    for &syntax_element in &assignments {
        if synchronized != Some(syntax_element) && !module_execution(syntax_element, &blocked) {
            continue;
        }
        let Some(binding) = assignment(syntax_element, source.text, kinds)? else {
            continue;
        };
        if binding.kind == SyntaxExportKind::Named
            && !kept_after_replacement(syntax_element, source.text, &replaced, synchronized_scope)
        {
            continue;
        }
        if binding.kind == SyntaxExportKind::Default
            && binding.local.is_some()
            && kinds
                .child(syntax_element, EcmaScriptGrammarField::Right)
                .is_some_and(|value| value.kind_id() == kinds.identifier)
            && (synchronized == Some(syntax_element)
                || (direct_default == Some(syntax_element)
                    && closed_wrapper_uses(root, None, syntax_element, source.text, kinds)))
        {
            closed_export = Some((binding.range, synchronized == Some(syntax_element)));
        }
        bindings.insert(syntax_element.id(), binding);
    }
    let closed_export = if let Some((assignment, synchronized)) = closed_export {
        Some(StaticCommonJsExport {
            assignment,
            synchronized,
            initialized: initialized_exports(root, source.text, kinds, bindings)?,
        })
    } else {
        None
    };
    Ok((assignments, closed_export))
}

struct CapturedAssignments<'tree> {
    assignments: Vec<Node<'tree>>,
    blocked: BTreeSet<usize>,
    replaced: BTreeMap<usize, usize>,
    synchronized: Option<(Node<'tree>, Node<'tree>)>,
    direct_default: Option<Node<'tree>>,
}

/// Captures assignments and wrapper rebindings in the bounded syntax walk.
fn capture_assignments<'tree>(
    root: Node<'tree>,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
    kinds: &EcmaScriptKinds,
) -> Result<CapturedAssignments<'tree>, RiftError> {
    let mut pending = vec![(root, 0_usize)];
    let mut blocked = BTreeSet::new();
    // Scope of each `module.exports = value`, with the end of its last write there.
    let mut replaced: BTreeMap<usize, usize> = BTreeMap::new();
    let mut synchronized = None;
    let mut direct_default = None;
    let mut assignments = Vec::new();
    let mut visited = 0_usize;
    let mut cursor = root.walk();
    while let Some((syntax_element, depth)) = pending.pop() {
        visited += 1;
        if depth > limits.syntax_depth_max() {
            return errors::syntax::too_deep()
                .path(source.path)
                .syntax_depth_max(limits.syntax_depth_max())
                .fail();
        }
        if visited + pending.len() > limits.syntax_elements_max() {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        if blocked_binding(syntax_element, source.text)
            && let Some(scope) = lexical_scope(syntax_element)
        {
            if let Some(module_write) = synchronization(syntax_element, root, source.text, kinds) {
                if synchronized
                    .replace((syntax_element, module_write))
                    .is_some()
                {
                    blocked.insert(root.id());
                }
            } else {
                blocked.insert(scope.id());
            }
        }
        if syntax_element.kind_id() == kinds.assignment_expression {
            assignments.push(syntax_element);
            if syntax_element
                .child_by_field_name("left")
                .and_then(|left| source.text.get(left.byte_range()))
                == Some("module.exports")
                && let Some(scope) = lexical_scope(syntax_element)
            {
                let end = replaced.entry(scope.id()).or_default();
                *end = (*end).max(syntax_element.end_byte());
                if syntax_element.parent().is_some_and(|statement| {
                    statement.kind_id() == kinds.expression_statement
                        && statement.parent() == Some(root)
                }) {
                    direct_default = Some(syntax_element);
                }
            }
        }
        if visited
            .saturating_add(pending.len())
            .saturating_add(syntax_element.named_child_count())
            > limits.syntax_elements_max()
        {
            return errors::syntax::too_many_syntax_elements()
                .path(source.path)
                .syntax_elements_max(limits.syntax_elements_max())
                .fail();
        }
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| {
            (child, depth + 1)
        });
    }
    Ok(CapturedAssignments {
        assignments,
        blocked,
        replaced,
        synchronized,
        direct_default,
    })
}

/// Finds the authored bindings that supply these exports directly from the module body.
fn initialized_exports(
    root: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
    bindings: &BTreeMap<usize, SyntaxExportBinding>,
) -> Result<Vec<crate::ByteRange>, RiftError> {
    let exported = bindings
        .values()
        .filter(|binding| binding.container.is_none() && binding.source.is_none())
        .filter_map(|binding| {
            let range = binding.local?;
            source.get(usize::try_from(range.start).ok()?..usize::try_from(range.end).ok()?)
        })
        .collect::<BTreeSet<_>>();
    let mut initialized = Vec::new();
    let mut record = |declaration| -> Result<(), RiftError> {
        if let Some(name) = kinds.child(declaration, EcmaScriptGrammarField::Name)
            && name.kind_id() == kinds.identifier
            && source
                .get(name.byte_range())
                .is_some_and(|name| exported.contains(name))
        {
            initialized.push(extract::byte_range(name)?);
        }
        Ok(())
    };
    // The main capture walk bounded this tree. Only direct children can run
    // unconditionally in the module, regardless of their lexical hoisting scope.
    for statement in named_children(root) {
        match statement.kind() {
            "function_declaration" => record(statement)?,
            "variable_declaration" | "lexical_declaration" => {
                for declaration in named_children(statement) {
                    if declaration.kind_id() == kinds.variable_declarator
                        && kinds
                            .child(declaration, EcmaScriptGrammarField::Value)
                            .is_some()
                    {
                        record(declaration)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(initialized)
}

/// Inner write of one unconditional `exports = module.exports = name` statement.
fn synchronization<'tree>(
    outer: Node<'tree>,
    root: Node<'tree>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Option<Node<'tree>> {
    if outer.kind_id() != kinds.assignment_expression {
        return None;
    }
    let left = kinds.child(outer, EcmaScriptGrammarField::Left)?;
    if source.get(left.byte_range())? != "exports" {
        return None;
    }
    let statement = outer.parent()?;
    if outer.has_error()
        || statement.kind_id() != kinds.expression_statement
        || statement.parent() != Some(root)
    {
        return None;
    }
    let inner = kinds.child(outer, EcmaScriptGrammarField::Right)?;
    if inner.kind_id() != kinds.assignment_expression {
        return None;
    }
    let module = kinds.child(inner, EcmaScriptGrammarField::Left)?;
    let value = kinds.child(inner, EcmaScriptGrammarField::Right)?;
    (source.get(module.byte_range())? == "module.exports" && value.kind_id() == kinds.identifier)
        .then_some(inner)
}

/// The wrapper names stay inside the synchronized writes and direct root export writes.
fn closed_wrapper_uses(
    root: Node<'_>,
    outer: Option<Node<'_>>,
    inner: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> bool {
    let outer_left = outer.and_then(|outer| kinds.child(outer, EcmaScriptGrammarField::Left));
    let module = kinds
        .child(inner, EcmaScriptGrammarField::Left)
        .and_then(|left| kinds.child(left, EcmaScriptGrammarField::Object));
    let mut pending = vec![root];
    let mut cursor = root.walk();
    // The capture walk has already admitted the element count and depth of this tree.
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "identifier"
                | "shorthand_property_identifier"
                | "shorthand_property_identifier_pattern"
        ) && source.get(node.byte_range()).is_some_and(|name| {
            name.contains('\\')
                || (matches!(name, "module" | "exports" | "eval")
                    && Some(node) != outer_left
                    && Some(node) != module
                    && !(outer.is_some() && direct_exports_write(node, root, source, kinds)))
        }) {
            return false;
        }
        extract::push_named_children(&mut pending, &mut cursor, node, |child| child);
    }
    true
}

fn direct_exports_write(
    receiver: Node<'_>,
    root: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> bool {
    if source.get(receiver.byte_range()) != Some("exports") {
        return false;
    }
    receiver.parent().is_some_and(|member| {
        member.kind_id() == kinds.member_expression
            && kinds.child(member, EcmaScriptGrammarField::Object) == Some(receiver)
            && kinds
                .child(member, EcmaScriptGrammarField::Property)
                .is_some_and(|name| name.kind_id() == kinds.property_identifier)
            && member.parent().is_some_and(|assignment| {
                assignment.kind_id() == kinds.assignment_expression
                    && !assignment.has_error()
                    && kinds.child(assignment, EcmaScriptGrammarField::Left) == Some(member)
                    && assignment.parent().is_some_and(|statement| {
                        statement.kind_id() == kinds.expression_statement
                            && statement.parent() == Some(root)
                    })
            })
    })
}

/// Whether a named export survives every `module.exports = value` of the scopes around it.
///
/// A replacement written later replaces the object the name was added to. One written
/// earlier is the object `module.exports.name = ...` adds to. A checked synchronization
/// also establishes that `exports` names that same object. The default slot stays ambiguous.
fn kept_after_replacement(
    syntax_element: Node<'_>,
    source: &str,
    replaced: &BTreeMap<usize, usize>,
    synchronized: Option<(usize, usize)>,
) -> bool {
    let left = syntax_element
        .child_by_field_name("left")
        .and_then(|left| source.get(left.byte_range()))
        .unwrap_or_default();
    let through_module = left.starts_with("module.exports.") && left != "module.exports.default";
    let mut scope = syntax_element;
    while let Some(parent) = scope.parent() {
        if let Some(&end) = replaced.get(&parent.id()) {
            let through_synchronized = synchronized == Some((parent.id(), end))
                && left.starts_with("exports.")
                && left != "exports.default";
            if end > syntax_element.start_byte() || !(through_module || through_synchronized) {
                return false;
            }
        }
        scope = parent;
    }
    true
}

fn receiver(text: &str) -> bool {
    matches!(text, "exports" | "module" | "require" | "eval")
}

fn binding_contains(syntax_element: Node<'_>, source: &str) -> bool {
    let mut pending = vec![syntax_element];
    let mut cursor = syntax_element.walk();
    while let Some(syntax_element) = pending.pop() {
        if matches!(
            syntax_element.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) && source
            .get(syntax_element.byte_range())
            .is_some_and(receiver)
        {
            return true;
        }
        extract::push_named_children(&mut pending, &mut cursor, syntax_element, |child| child);
    }
    false
}

fn blocked_binding(syntax_element: Node<'_>, source: &str) -> bool {
    let binding = match syntax_element.kind() {
        "variable_declarator" | "function_declaration" | "class_declaration" => {
            syntax_element.child_by_field_name("name")
        }
        "formal_parameters" | "import_clause" | "catch_clause" => Some(syntax_element),
        "assignment_expression" | "augmented_assignment_expression" => syntax_element
            .child_by_field_name("left")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "update_expression" => syntax_element
            .child_by_field_name("argument")
            .filter(|left| !matches!(left.kind(), "member_expression" | "subscript_expression")),
        "call_expression" => {
            return syntax_element
                .child_by_field_name("function")
                .and_then(|function| source.get(function.byte_range()))
                == Some("eval");
        }
        _ => None,
    };
    binding.is_some_and(|binding| binding_contains(binding, source))
}

fn lexical_scope(mut syntax_element: Node<'_>) -> Option<Node<'_>> {
    let function_binding = matches!(
        syntax_element.kind(),
        "formal_parameters"
            | "variable_declarator"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "update_expression"
            | "call_expression"
            | "function_declaration"
    );
    while let Some(parent) = syntax_element.parent() {
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
        syntax_element = parent;
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

fn module_execution(mut syntax_element: Node<'_>, blocked: &BTreeSet<usize>) -> bool {
    while let Some(parent) = syntax_element.parent() {
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
        syntax_element = parent;
    }
    false
}

fn assignment(
    syntax_element: Node<'_>,
    source: &str,
    kinds: &EcmaScriptKinds,
) -> Result<Option<SyntaxExportBinding>, RiftError> {
    let Some(left) = kinds.child(syntax_element, EcmaScriptGrammarField::Left) else {
        return Ok(None);
    };
    let Some(target) = kinds.export_target(left, source) else {
        return Ok(None);
    };
    let range = extract::byte_range(syntax_element)?;
    if let Some((property, function)) = kinds.export_function(syntax_element, source) {
        // A named function assigned to `module.exports` is declared under its own name.
        let written = (target == ExportTarget::Module)
            .then(|| kinds.child(function, EcmaScriptGrammarField::Name))
            .flatten();
        let name = extract::byte_range(written.unwrap_or(property))?;
        return Ok(Some(SyntaxExportBinding {
            kind: if target == ExportTarget::Property {
                SyntaxExportKind::Named
            } else {
                SyntaxExportKind::Default
            },
            range,
            local: Some(name),
            exported: (target == ExportTarget::Property).then_some(name),
            source: None,
            container: None,
            type_only: false,
        }));
    }
    if target == ExportTarget::Property {
        // `exports.name = local` exports the declaration the local name binds.
        let property = kinds.child(left, EcmaScriptGrammarField::Property);
        let value = kinds.child(syntax_element, EcmaScriptGrammarField::Right);
        let Some((property, value)) = property
            .filter(|property| property.kind_id() == kinds.property_identifier)
            .zip(value.filter(|value| value.kind_id() == kinds.identifier))
        else {
            return Ok(None);
        };
        return Ok(Some(SyntaxExportBinding {
            kind: SyntaxExportKind::Named,
            range,
            local: Some(extract::byte_range(value)?),
            exported: Some(extract::byte_range(property)?),
            source: None,
            container: None,
            type_only: false,
        }));
    }
    let Some(value) = kinds.child(syntax_element, EcmaScriptGrammarField::Right) else {
        return Ok(None);
    };
    if value.kind_id() == kinds.identifier {
        // `module.exports = name` exports the declaration the name binds, as
        // `export default name` does.
        return Ok(Some(SyntaxExportBinding {
            kind: SyntaxExportKind::Default,
            range,
            local: Some(extract::byte_range(value)?),
            exported: None,
            source: None,
            container: None,
            type_only: false,
        }));
    }
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
