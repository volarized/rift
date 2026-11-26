//! Authored parameters of named TypeScript types.

use tree_sitter::Node;

use crate::extract::Visited;

use super::{
    ABSTRACT_CLASS_DECLARATION_KIND, CLASS_DECLARATION_KIND, INTERFACE_DECLARATION_KIND,
    TYPE_ALIAS_DECLARATION_KIND,
};

pub(super) fn declared_name<'tree>(visited: Visited<'_, 'tree>) -> Option<Node<'tree>> {
    let parameter = visited.syntax_element();
    if parameter.kind() != "type_parameter" || parameter.has_error() {
        return None;
    }
    let list = visited.parent()?.syntax_element();
    let owner = list.parent()?;
    if owner.child_by_field_name("type_parameters") != Some(list) || !structured(owner.kind()) {
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
