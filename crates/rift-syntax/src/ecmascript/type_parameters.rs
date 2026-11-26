//! Authored parameters of named TypeScript types.

use tree_sitter::Node;

use crate::extract::Visited;

use super::{
    ABSTRACT_CLASS_DECLARATION_KIND, CLASS_DECLARATION_KIND, INTERFACE_DECLARATION_KIND,
    TYPE_ALIAS_DECLARATION_KIND,
};

pub(super) fn declared_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
    named_owner(visited, structured)
}

pub(super) fn named_owner<'tree>(
    visited: Visited<'_, 'tree>,
    owns_parameter: impl Fn(&str) -> bool,
) -> Option<Node<'tree>> {
    let parameter = visited.syntax_element();
    if parameter.kind() != "type_parameter" || parameter.has_error() {
        return None;
    }
    let list = visited.parent()?.syntax_element();
    let owner = list.parent()?;
    if owner.child_by_field_name("type_parameters") != Some(list) || !owns_parameter(owner.kind()) {
        return None;
    }
    parameter
        .child_by_field_name("name")
        .filter(|name| name.kind() == "type_identifier" && !name.is_missing())
}

pub(super) fn structured(kind: &str) -> bool {
    matches!(
        kind,
        ABSTRACT_CLASS_DECLARATION_KIND
            | CLASS_DECLARATION_KIND
            | INTERFACE_DECLARATION_KIND
            | TYPE_ALIAS_DECLARATION_KIND
    )
}

pub(super) fn method(kind: &str) -> bool {
    matches!(
        kind,
        super::METHOD_DEFINITION_KIND
            | super::METHOD_SIGNATURE_KIND
            | super::ABSTRACT_METHOD_SIGNATURE_KIND
    )
}

pub(super) fn function(kind: &str) -> bool {
    kind == super::FUNCTION_DECLARATION_KIND
}
