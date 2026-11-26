//! Names of written JavaScript defaulted and rest parameters.

use tree_sitter::Node;

use super::{
    FUNCTION_DECLARATION_KIND, GENERATOR_FUNCTION_DECLARATION_KIND, METHOD_DEFINITION_KIND,
};

pub(super) fn declared_name(parameter: Node<'_>) -> Option<Node<'_>> {
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
        || !accepted_owner(scope.kind())
    {
        return None;
    }
    Some(name)
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
