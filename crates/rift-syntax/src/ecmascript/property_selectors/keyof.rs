//! Exact member selection through a lexically bound `keyof` index.

use std::collections::BTreeSet;

use tree_sitter::{Language, Node};

use crate::extract::byte_range;
use crate::{ByteRange, SyntaxLimits, failure::RiftError};

use super::{CheckedKeyofBound, CheckedPropertySelector};

enum Witness<'tree> {
    Parameter(Node<'tree>),
    TrueBranch {
        owner: Node<'tree>,
        test: Node<'tree>,
    },
}

impl<'tree> Witness<'tree> {
    fn owner(&self) -> Node<'tree> {
        match *self {
            Self::Parameter(owner) | Self::TrueBranch { owner, .. } => owner,
        }
    }

    fn proof(&self) -> Result<CheckedKeyofBound, RiftError> {
        Ok(match *self {
            Self::Parameter(_) => CheckedKeyofBound::Parameter,
            Self::TrueBranch { test, .. } => CheckedKeyofBound::TrueBranch {
                key: byte_range(test)?,
            },
        })
    }
}

pub(super) struct Kinds {
    type_identifier: u16,
    type_parameter: u16,
    index_query: u16,
    conditional: u16,
    nested_type_identifier: u16,
    predefined_type: u16,
    broad_predefined_types: [u16; 3],
    interface: u16,
    interface_body: u16,
    type_alias: u16,
    object: u16,
    property: u16,
    property_identifier: u16,
    string: u16,
    fragment: u16,
    optional: u16,
    type_parameters: u16,
    constraint: u16,
    name: u16,
    body: u16,
    value: u16,
    annotation: u16,
    left: u16,
    right: u16,
    consequence: u16,
}

impl Kinds {
    pub(super) fn resolve(grammar: &Language) -> Self {
        let named = |kind| grammar.id_for_node_kind(kind, true);
        let field = |name| {
            grammar
                .field_id_for_name(name)
                .expect("pinned TypeScript field")
                .get()
        };
        Self {
            type_identifier: named("type_identifier"),
            type_parameter: named("type_parameter"),
            index_query: named("index_type_query"),
            conditional: named("conditional_type"),
            nested_type_identifier: named("nested_type_identifier"),
            predefined_type: named("predefined_type"),
            broad_predefined_types: ["string", "unknown", "any"]
                .map(|kind| grammar.id_for_node_kind(kind, false)),
            interface: named("interface_declaration"),
            interface_body: named("interface_body"),
            type_alias: named("type_alias_declaration"),
            object: named("object_type"),
            property: named("property_signature"),
            property_identifier: named("property_identifier"),
            string: named("string"),
            fragment: named("string_fragment"),
            optional: grammar.id_for_node_kind("?", false),
            type_parameters: field("type_parameters"),
            constraint: field("constraint"),
            name: field("name"),
            body: field("body"),
            value: field("value"),
            annotation: field("type"),
            left: field("left"),
            right: field("right"),
            consequence: field("consequence"),
        }
    }

    pub(super) fn select(
        &self,
        indexed: Node<'_>,
        selector: Node<'_>,
        member: Node<'_>,
        source: &str,
        depth_max: usize,
    ) -> Result<Option<CheckedPropertySelector>, RiftError> {
        let mut cursor = indexed.walk();
        let mut parts = indexed
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra());
        let (Some(owner), Some(key)) = (parts.next(), parts.next()) else {
            return Ok(None);
        };
        if parts.next().is_some()
            || owner.kind_id() != self.type_identifier
            || key.kind_id() != self.type_identifier
        {
            return Ok(None);
        }
        let Some((parameter, witness)) = self.witness(indexed, key, source, depth_max) else {
            return Ok(None);
        };
        let bound = witness.owner();
        if source.get(owner.byte_range()) != source.get(bound.byte_range()) {
            return Ok(None);
        }
        Ok(Some(CheckedPropertySelector::KeyofIndexedAccess {
            owner: byte_range(owner)?,
            bound: byte_range(bound)?,
            key: byte_range(key)?,
            parameter: byte_range(parameter)?,
            proof: witness.proof()?,
            member: byte_range(member)?,
            selector: byte_range(selector)?,
        }))
    }

    fn witness<'tree>(
        &self,
        indexed: Node<'tree>,
        key: Node<'tree>,
        source: &str,
        depth_max: usize,
    ) -> Option<(Node<'tree>, Witness<'tree>)> {
        let spelling = source.get(key.byte_range())?;
        let mut scope = indexed;
        let mut conditional = None;
        for _ in 0..depth_max {
            let child = scope;
            scope = scope.parent()?;
            if scope.kind_id() == self.conditional && conditional.is_none() {
                conditional = Some(self.true_branch(scope, child, spelling, source));
            }
            let Some(parameters) = scope.child_by_field_id(self.type_parameters) else {
                continue;
            };
            let mut cursor = parameters.walk();
            let mut matching = parameters
                .named_children(&mut cursor)
                .filter_map(|parameter| {
                    if parameter.kind_id() != self.type_parameter {
                        return None;
                    }
                    let name = parameter.child_by_field_id(self.name)?;
                    (source.get(name.byte_range())? == spelling).then_some((parameter, name))
                });
            if let Some((parameter, name)) = matching.next() {
                if matching.next().is_some() {
                    return None;
                }
                if let Some(constraint) = parameter.child_by_field_id(self.constraint) {
                    let constraint = only_child(constraint)?;
                    if let Some(owner) = self.keyof_type(constraint) {
                        return Some((name, Witness::Parameter(owner)));
                    }
                    if !self.condition_compatible_bound(constraint) {
                        return None;
                    }
                }
                return Some((name, conditional.flatten()?));
            }
        }
        None
    }

    fn true_branch<'tree>(
        &self,
        scope: Node<'tree>,
        child: Node<'tree>,
        spelling: &str,
        source: &str,
    ) -> Option<Witness<'tree>> {
        if scope.child_by_field_id(self.consequence)? != child {
            return None;
        }
        let test = scope.child_by_field_id(self.left)?;
        if test.kind_id() != self.type_identifier || source.get(test.byte_range())? != spelling {
            return None;
        }
        let owner = self.keyof_type(scope.child_by_field_id(self.right)?)?;
        Some(Witness::TrueBranch { owner, test })
    }

    fn keyof_type<'tree>(&self, query: Node<'tree>) -> Option<Node<'tree>> {
        if query.kind_id() != self.index_query {
            return None;
        }
        let owner = only_child(query)?;
        (owner.kind_id() == self.type_identifier).then_some(owner)
    }

    fn condition_compatible_bound(&self, constraint: Node<'_>) -> bool {
        let kind = constraint.kind_id();
        kind == self.type_identifier
            || kind == self.nested_type_identifier
            || (kind == self.predefined_type
                && constraint
                    .child(0)
                    .is_some_and(|name| self.broad_predefined_types.contains(&name.kind_id())))
    }

    pub(super) fn closed_members(
        &self,
        root: Node<'_>,
        owner: ByteRange,
        member: &str,
        source: &str,
        limits: SyntaxLimits,
    ) -> Result<Option<Vec<ByteRange>>, RiftError> {
        let (Ok(start), Ok(end)) = (usize::try_from(owner.start), usize::try_from(owner.end))
        else {
            return Ok(None);
        };
        let Some(object) = root
            .named_descendant_for_byte_range(start, end)
            .filter(|name| {
                name.kind_id() == self.type_identifier && name.byte_range() == (start..end)
            })
            .and_then(|name| self.object_declaration(name))
        else {
            return Ok(None);
        };
        let mut routes = BTreeSet::new();
        let mut members = Vec::new();
        let mut remaining = limits.syntax_elements_max();
        let mut cursor = object.walk();
        for field in object
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra())
        {
            let Some(name) = self.property_name(field, source) else {
                return Ok(None);
            };
            if !routes.insert(name) || self.optional_property(field) {
                return Ok(None);
            }
            let Some(nested) = field
                .child_by_field_id(self.annotation)
                .and_then(only_child)
                .filter(|node| node.kind_id() == self.object)
            else {
                return Ok(None);
            };
            let Some(after) = remaining.checked_sub(1 + nested.named_child_count()) else {
                return Ok(None);
            };
            remaining = after;
            let Some(selected) = self.nested_member(nested, member, source) else {
                return Ok(None);
            };
            members.push(byte_range(selected)?);
        }
        Ok((!members.is_empty()).then_some(members))
    }

    fn object_declaration<'tree>(&self, name: Node<'tree>) -> Option<Node<'tree>> {
        let owner = name.parent()?;
        if owner.child_by_field_id(self.name) != Some(name)
            || owner.child_by_field_id(self.type_parameters).is_some()
        {
            return None;
        }
        if owner.kind_id() == self.type_alias {
            return owner
                .child_by_field_id(self.value)
                .filter(|node| node.kind_id() == self.object);
        }
        if owner.kind_id() != self.interface {
            return None;
        }
        let body = owner.child_by_field_id(self.body)?;
        let mut cursor = owner.walk();
        let only_witnessed = owner
            .named_children(&mut cursor)
            .filter(|child| !child.is_extra())
            .all(|child| child == name || child == body);
        (body.kind_id() == self.interface_body && only_witnessed).then_some(body)
    }

    fn property_name<'source>(
        &self,
        field: Node<'_>,
        source: &'source str,
    ) -> Option<&'source str> {
        if field.kind_id() != self.property {
            return None;
        }
        let name = field.child_by_field_id(self.name)?;
        if name.kind_id() == self.property_identifier {
            return source.get(name.byte_range());
        }
        let fragment = only_child(name)?;
        (name.kind_id() == self.string && fragment.kind_id() == self.fragment)
            .then(|| source.get(fragment.byte_range()))
            .flatten()
    }

    fn optional_property(&self, property: Node<'_>) -> bool {
        let mut cursor = property.walk();
        property
            .children(&mut cursor)
            .any(|child| child.kind_id() == self.optional)
    }

    fn nested_member<'tree>(
        &self,
        object: Node<'tree>,
        wanted: &str,
        source: &str,
    ) -> Option<Node<'tree>> {
        let mut selected = None;
        let mut cursor = object.walk();
        for property in object
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra())
        {
            let name = self.property_name(property, source)?;
            if name == wanted
                && selected
                    .replace(property.child_by_field_id(self.name)?)
                    .is_some()
            {
                return None;
            }
        }
        selected
    }
}

fn only_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    let mut children = node
        .named_children(&mut cursor)
        .filter(|child| !child.is_extra());
    let child = children.next()?;
    children.next().is_none().then_some(child)
}
