//! Class and implementation headers retained as syntax edges.

use std::sync::OnceLock;

use rift_error::RiftError;
use rift_protocol::read::EdgeKind;
use tree_sitter::{Language, Node};

use super::{StaticFactsParts, field, kind};
use crate::extract::{Visited, byte_range};

/// One syntax header connecting a declaration to a base or implemented type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeritageEdge {
    /// Name or type path of the source declaration.
    pub from: crate::ByteRange,
    /// Name or type path on the heritage header.
    pub target: crate::ByteRange,
    /// Portable edge kind established by the header.
    pub kind: EdgeKind,
}

pub(super) struct EdgeHeritageKinds {
    header: u16,
    interface_header: Option<u16>,
    type_arguments: Option<u16>,
    extends: Option<u16>,
    implements: Option<u16>,
    generic: Option<u16>,
    name: u16,
    value_type: Option<u16>,
    trait_field: Option<u16>,
    bases: Option<u16>,
}

pub(super) fn edge_javascript_kinds() -> &'static EdgeHeritageKinds {
    static KINDS: OnceLock<EdgeHeritageKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        EdgeHeritageKinds::edge_ecmascript(&crate::javascript::javascript_grammar(), None)
    })
}

pub(super) fn edge_typescript_kinds(
    dialect: crate::typescript::TypeScriptDialect,
) -> &'static EdgeHeritageKinds {
    static TYPESCRIPT: OnceLock<EdgeHeritageKinds> = OnceLock::new();
    static TSX: OnceLock<EdgeHeritageKinds> = OnceLock::new();
    let cache = match dialect {
        crate::typescript::TypeScriptDialect::TypeScript => &TYPESCRIPT,
        crate::typescript::TypeScriptDialect::Tsx => &TSX,
    };
    cache.get_or_init(|| EdgeHeritageKinds::edge_ecmascript(&dialect.grammar(), Some(dialect)))
}

pub(super) fn edge_python_kinds() -> &'static EdgeHeritageKinds {
    static KINDS: OnceLock<EdgeHeritageKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = tree_sitter_python::LANGUAGE.into();
        EdgeHeritageKinds {
            header: kind(&grammar, "class_definition"),
            interface_header: None,
            type_arguments: None,
            name: field(&grammar, "name"),
            bases: Some(field(&grammar, "superclasses")),
            extends: None,
            implements: None,
            generic: Some(kind(&grammar, "subscript")),
            value_type: None,
            trait_field: None,
        }
    })
}

pub(super) fn edge_rust_kinds() -> &'static EdgeHeritageKinds {
    static KINDS: OnceLock<EdgeHeritageKinds> = OnceLock::new();
    KINDS.get_or_init(|| {
        let grammar = crate::rust::rust_grammar();
        EdgeHeritageKinds {
            header: kind(&grammar, "impl_item"),
            interface_header: None,
            type_arguments: None,
            name: field(&grammar, "name"),
            value_type: Some(field(&grammar, "type")),
            trait_field: Some(field(&grammar, "trait")),
            generic: Some(kind(&grammar, "generic_type")),
            extends: None,
            implements: None,
            bases: None,
        }
    })
}

impl EdgeHeritageKinds {
    fn edge_ecmascript(
        grammar: &Language,
        dialect: Option<crate::typescript::TypeScriptDialect>,
    ) -> Self {
        Self {
            header: kind(grammar, "class_heritage"),
            interface_header: dialect.map(|_| kind(grammar, "extends_type_clause")),
            type_arguments: dialect.map(|_| kind(grammar, "type_arguments")),
            name: field(grammar, "name"),
            extends: dialect.map(|_| kind(grammar, "extends_clause")),
            implements: dialect.map(|_| kind(grammar, "implements_clause")),
            generic: dialect.map(|_| kind(grammar, "generic_type")),
            value_type: None,
            trait_field: None,
            bases: None,
        }
    }

    pub(super) fn record_edge(
        &self,
        visited: Visited<'_, '_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let syntax_element = visited.syntax_element();
        if syntax_element.kind_id() != self.header
            && self.interface_header != Some(syntax_element.kind_id())
        {
            return Ok(());
        }
        if let Some(trait_field) = self.trait_field {
            let from = self
                .value_type
                .and_then(|field| syntax_element.child_by_field_id(field));
            if let Some((from, target)) = from.zip(syntax_element.child_by_field_id(trait_field)) {
                self.edge_push(from, target, EdgeKind::Implements, facts)?;
            }
            return Ok(());
        }
        let owner = if self.bases.is_some() {
            Some(syntax_element)
        } else {
            visited.parent().map(Visited::syntax_element)
        };
        let Some(from) = owner.and_then(|owner| owner.child_by_field_id(self.name)) else {
            return Ok(());
        };
        let bases = match self.bases {
            Some(field) => syntax_element.child_by_field_id(field),
            None => Some(syntax_element),
        };
        let Some(bases) = bases else {
            return Ok(());
        };
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
            if self.extends == Some(base.kind_id()) || self.implements == Some(base.kind_id()) {
                self.edge_clause(from, base, facts)?;
            } else {
                self.edge_push(from, base, EdgeKind::Extends, facts)?;
            }
        }
        Ok(())
    }

    fn edge_clause(
        &self,
        from: Node<'_>,
        clause: Node<'_>,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let kind = if self.implements == Some(clause.kind_id()) {
            EdgeKind::Implements
        } else {
            EdgeKind::Extends
        };
        let mut cursor = clause.walk();
        for target in clause.named_children(&mut cursor) {
            if self.type_arguments != Some(target.kind_id()) {
                self.edge_push(from, target, kind, facts)?;
            }
        }
        Ok(())
    }

    fn edge_push(
        &self,
        from: Node<'_>,
        target: Node<'_>,
        kind: EdgeKind,
        facts: &mut StaticFactsParts,
    ) -> Result<(), RiftError> {
        let from = self.edge_type(from);
        let target = self.edge_type(target);
        if let Some((from, target)) = from.zip(target) {
            facts.heritage_edges.push(HeritageEdge {
                from: byte_range(from)?,
                target: byte_range(target)?,
                kind,
            });
        }
        Ok(())
    }

    fn edge_type<'tree>(&self, syntax_element: Node<'tree>) -> Option<Node<'tree>> {
        if syntax_element.is_missing() {
            return None;
        }
        if self.generic == Some(syntax_element.kind_id()) {
            return syntax_element
                .child_by_field_id(self.value_type.unwrap_or(self.name))
                .or_else(|| syntax_element.named_child(0));
        }
        Some(syntax_element)
    }
}
