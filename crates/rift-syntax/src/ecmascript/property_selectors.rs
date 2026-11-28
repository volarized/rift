//! Property selectors in written TypeScript declarations.

use std::collections::BTreeMap;

use rift_error::errors;
use tree_sitter::{Language, Node};

use crate::extract::{byte_range, push_named_children};
use crate::{ByteRange, SyntaxLimits, SyntaxSource, failure::RiftError};

use super::parser_for_ranges;

/// A property selector with its checked written type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedPropertySelector {
    /// A string-literal index names a property of the specified type.
    IndexedAccess {
        /// The written type name.
        owner: ByteRange,
        /// The written property name, excluding its quotes.
        selector: ByteRange,
    },
    /// A destructured parameter names a property in its own object annotation.
    DestructuredParameter {
        /// The written property selector, before any binding alias.
        selector: ByteRange,
        /// The name of the declared property in the annotation.
        property: ByteRange,
    },
}

struct Kinds {
    indexed: u16,
    owners: [u16; 2],
    literal: u16,
    string: u16,
    fragment: u16,
    parameters: ParameterKinds,
}

impl Kinds {
    fn resolve(grammar: &Language) -> Self {
        let named = |kind| grammar.id_for_node_kind(kind, true);
        Self {
            indexed: named("lookup_type"),
            owners: [named("type_identifier"), named("nested_type_identifier")],
            literal: named("literal_type"),
            string: named("string"),
            fragment: named("string_fragment"),
            parameters: ParameterKinds::resolve(grammar),
        }
    }

    fn indexed<'tree>(&self, indexed: Node<'tree>) -> Option<(Node<'tree>, Node<'tree>)> {
        let mut cursor = indexed.walk();
        let mut parts = indexed
            .named_children(&mut cursor)
            .filter(|node| !node.is_extra());
        let owner = parts
            .next()
            .filter(|node| self.owners.contains(&node.kind_id()))?;
        let literal = parts.next().filter(|node| node.kind_id() == self.literal)?;
        if parts.next().is_some() {
            return None;
        }
        let string = literal
            .named_child(0)
            .filter(|node| node.kind_id() == self.string)?;
        let fragment = string
            .named_child(0)
            .filter(|node| node.kind_id() == self.fragment)?;
        // One source fragment states the name without decoding an escape sequence.
        (string.named_child_count() == 1 && !fragment.is_missing()).then_some((owner, fragment))
    }
}

struct ParameterKinds {
    declarations: [u16; 2],
    object_pattern: u16,
    object_type: u16,
    property_signature: u16,
    property_identifier: u16,
    shorthand: u16,
    pair: u16,
    pattern: u16,
    annotation: u16,
    name: u16,
    key: u16,
}

impl ParameterKinds {
    fn resolve(grammar: &Language) -> Self {
        let named = |kind| grammar.id_for_node_kind(kind, true);
        let field = |name| {
            grammar
                .field_id_for_name(name)
                .expect("TypeScript field")
                .get()
        };
        Self {
            declarations: [named("required_parameter"), named("optional_parameter")],
            object_pattern: named("object_pattern"),
            object_type: named("object_type"),
            property_signature: named("property_signature"),
            property_identifier: named("property_identifier"),
            shorthand: named("shorthand_property_identifier_pattern"),
            pair: named("pair_pattern"),
            pattern: field("pattern"),
            annotation: field("type"),
            name: field("name"),
            key: field("key"),
        }
    }

    fn append(
        &self,
        declaration: Node<'_>,
        source: &str,
        selected: &mut Vec<CheckedPropertySelector>,
    ) -> Result<(), RiftError> {
        let Some(pattern) = declaration
            .child_by_field_id(self.pattern)
            .filter(|node| node.kind_id() == self.object_pattern)
        else {
            return Ok(());
        };
        let Some(annotation) = declaration.child_by_field_id(self.annotation) else {
            return Ok(());
        };
        let mut cursor = annotation.walk();
        let Some(object) = annotation
            .named_children(&mut cursor)
            .find(|node| !node.is_extra())
            .filter(|node| node.kind_id() == self.object_type)
        else {
            return Ok(());
        };
        let mut properties = BTreeMap::new();
        let mut cursor = object.walk();
        for property in object
            .named_children(&mut cursor)
            .filter(|node| node.kind_id() == self.property_signature)
        {
            if let Some(name) = property
                .child_by_field_id(self.name)
                .filter(|node| node.kind_id() == self.property_identifier)
                && let Some(written) = source.get(name.byte_range())
            {
                properties
                    .entry(written)
                    .and_modify(|prior| *prior = None)
                    .or_insert(Some(name));
            }
        }
        let mut cursor = pattern.walk();
        for element in pattern.named_children(&mut cursor) {
            let selector = if element.kind_id() == self.shorthand {
                Some(element)
            } else if element.kind_id() == self.pair {
                element
                    .child_by_field_id(self.key)
                    .filter(|node| node.kind_id() == self.property_identifier)
            } else {
                None
            };
            if let Some(selector) = selector
                && let Some(written) = source.get(selector.byte_range())
                && let Some(Some(property)) = properties.get(written)
            {
                selected.push(CheckedPropertySelector::DestructuredParameter {
                    selector: byte_range(selector)?,
                    property: byte_range(*property)?,
                });
            }
        }
        Ok(())
    }
}

pub(crate) fn check(
    grammar: &Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
) -> Result<Vec<CheckedPropertySelector>, RiftError> {
    limits.admit_source(source)?;
    let mut parser = parser_for_ranges(grammar, &[])?;
    let tree = parser
        .parse(source.text, None)
        .ok_or_else(|| errors::syntax::parse_cancelled().path(source.path).error())?;
    let root = tree.root_node();
    if root.has_error() {
        return Ok(Vec::new());
    }
    let kinds = Kinds::resolve(grammar);
    let mut cursor = root.walk();
    let mut pending = vec![(root, 0)];
    let mut left = limits.syntax_elements_max();
    let mut selected = Vec::new();
    while let Some((node, depth)) = pending.pop() {
        if depth > limits.syntax_depth_max()
            || left <= pending.len().saturating_add(node.named_child_count())
        {
            return Ok(Vec::new());
        }
        left -= 1;
        if node.kind_id() == kinds.indexed
            && let Some((owner, selector)) = kinds.indexed(node)
        {
            selected.push(CheckedPropertySelector::IndexedAccess {
                owner: byte_range(owner)?,
                selector: byte_range(selector)?,
            });
        } else if kinds.parameters.declarations.contains(&node.kind_id()) {
            kinds.parameters.append(node, source.text, &mut selected)?;
        }
        push_named_children(&mut pending, &mut cursor, node, |child| (child, depth + 1));
    }
    Ok(selected)
}
