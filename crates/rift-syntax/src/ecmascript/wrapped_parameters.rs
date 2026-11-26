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
