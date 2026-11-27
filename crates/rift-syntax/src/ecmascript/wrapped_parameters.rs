//! Names of written JavaScript defaulted and rest parameters.

use tree_sitter::Node;

use super::{
    FUNCTION_DECLARATION_KIND, GENERATOR_FUNCTION_DECLARATION_KIND, METHOD_DEFINITION_KIND,
    VARIABLE_DECLARATOR_KIND,
};
use crate::{ByteRange, SyntaxSymbol, extract};

pub(super) fn declared_name(parameter: Node<'_>) -> Option<Node<'_>> {
    let (name, scope) = written_parameter(parameter)?;
    accepted_owner(scope.kind()).then_some(name)
}

pub(super) fn arrow_parameter_name(parameter: Node<'_>) -> Option<Node<'_>> {
    let (name, scope) = written_parameter(parameter)?;
    direct_arrow_holder(scope)?;
    Some(name)
}

pub(crate) fn arrow_body_with_wrapped_parameters(scope: Node<'_>) -> Option<Node<'_>> {
    direct_arrow_holder(scope)?;
    let parameters = scope.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    parameters
        .named_children(&mut cursor)
        .any(|parameter| written_parameter(parameter).is_some())
        .then(|| scope.child_by_field_name("body"))
        .flatten()
}

fn written_parameter(parameter: Node<'_>) -> Option<(Node<'_>, Node<'_>)> {
    let name = match parameter.kind() {
        "assignment_pattern" => parameter.child_by_field_name("left"),
        "rest_pattern" => parameter.named_child(0),
        _ => None,
    }?;
    if parameter.has_error() || name.kind() != "identifier" || name.is_missing() {
        return None;
    }
    let list = parameter.parent()?;
    let scope = list.parent()?;
    if list.kind() != "formal_parameters"
        || scope.child_by_field_name("parameters") != Some(list)
        || scope.child_by_field_name("body").is_none()
    {
        return None;
    }
    Some((name, scope))
}

fn direct_arrow_holder(scope: Node<'_>) -> Option<Node<'_>> {
    if scope.kind() != "arrow_function" || scope.has_error() {
        return None;
    }
    let holder = scope.parent()?;
    (holder.kind() == VARIABLE_DECLARATOR_KIND
        && holder.child_by_field_name("value") == Some(scope)
        && holder
            .child_by_field_name("name")
            .is_some_and(|name| name.kind() == "identifier" && !name.is_missing()))
    .then_some(holder)
}

pub(super) fn arrow_binding_scope(
    root: Node<'_>,
    owner: &SyntaxSymbol,
    name: ByteRange,
) -> Option<ByteRange> {
    if owner.syntax_element_kind != Some(VARIABLE_DECLARATOR_KIND) {
        return None;
    }
    let start = usize::try_from(name.start).ok()?;
    let end = usize::try_from(name.end).ok()?;
    let identifier = root.named_descendant_for_byte_range(start, end)?;
    let (written, scope) = written_parameter(identifier.parent()?)?;
    let holder = direct_arrow_holder(scope)?;
    if start >= end
        || written != identifier
        || identifier.byte_range() != (start..end)
        || extract::byte_range(holder).ok()? != owner.item_range
        || extract::byte_range(holder.child_by_field_name("name")?).ok()? != owner.name_range?
    {
        return None;
    }
    extract::byte_range(scope).ok()
}

pub(super) fn accepted_owner(kind: &str) -> bool {
    matches!(
        kind,
        FUNCTION_DECLARATION_KIND | GENERATOR_FUNCTION_DECLARATION_KIND | METHOD_DEFINITION_KIND
    )
}

/// Names in patterns enter the scope of the function that writes the pattern.
pub(super) fn destructured_name(name: Node<'_>, typed: bool) -> Option<Node<'_>> {
    if !matches!(
        name.kind(),
        "identifier" | "shorthand_property_identifier_pattern"
    ) || name.is_missing()
    {
        return None;
    }
    let mut current = name;
    let mut inside_pattern = false;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "object_pattern" | "array_pattern" => inside_pattern = true,
            "required_parameter" | "optional_parameter"
                if typed && parent.child_by_field_name("pattern") == Some(current) => {}
            "pair_pattern" if parent.child_by_field_name("value") == Some(current) => {}
            "assignment_pattern" | "object_assignment_pattern"
                if parent.child_by_field_name("left") == Some(current) => {}
            "rest_pattern" if parent.named_child(0) == Some(current) => {}
            "formal_parameters" if inside_pattern && !current.has_error() => {
                let owner = parent.parent()?;
                return (accepted_owner(owner.kind())
                    && owner.child_by_field_name("parameters") == Some(parent)
                    && owner.child_by_field_name("body").is_some())
                .then_some(name);
            }
            _ => return None,
        }
        current = parent;
    }
    None
}
