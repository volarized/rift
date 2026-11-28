//! Property selectors in written TypeScript indexed access types.

use rift_error::errors;
use tree_sitter::{Language, Node};

use crate::extract::{byte_range, push_named_children};
use crate::{ByteRange, SyntaxLimits, SyntaxSource, failure::RiftError};

use super::parser_for_ranges;

/// A named type and the property name written inside an indexed string literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedIndexedAccess {
    /// The written type name.
    pub owner: ByteRange,
    /// The written property name, excluding its quotes.
    pub selector: ByteRange,
}

struct Kinds {
    indexed: u16,
    owners: [u16; 2],
    literal: u16,
    string: u16,
    fragment: u16,
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
        }
    }

    fn checked<'tree>(&self, indexed: Node<'tree>) -> Option<(Node<'tree>, Node<'tree>)> {
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

pub(crate) fn check(
    grammar: &Language,
    source: SyntaxSource<'_>,
    limits: SyntaxLimits,
) -> Result<Vec<CheckedIndexedAccess>, RiftError> {
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
            && let Some((owner, selector)) = kinds.checked(node)
        {
            selected.push(CheckedIndexedAccess {
                owner: byte_range(owner)?,
                selector: byte_range(selector)?,
            });
        }
        push_named_children(&mut pending, &mut cursor, node, |child| (child, depth + 1));
    }
    Ok(selected)
}
